mod connect;
mod tls_parser;
use bytes::{Bytes, BytesMut};
use log::{debug, info, warn};
use std::{
    borrow::Cow, cmp, future::Future, io, net::SocketAddr, pin::Pin, sync::Arc, time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

#[cfg(target_os = "linux")]
use crate::linux::tcp::{get_original_dest, get_original_dest6};
use crate::{
    client::connect::try_connect_all,
    monitor::ServerList,
    proxy::copy::pipe,
    proxy::{Address, Destination, ProxyServer},
};

#[derive(Debug)]
pub struct NewClient {
    left: TcpStream,
    src: SocketAddr,
    pub dest: Destination,
    list: ServerList,
    from_port: u16,
}

#[derive(Debug)]
pub struct NewClientWithData {
    client: NewClient,
    pending_data: Option<Bytes>,
    has_full_tls_hello: bool,
}

#[derive(Debug)]
pub struct ConnectedClient {
    left: TcpStream,
    right: TcpStream,
    dest: Destination,
    server: Arc<ProxyServer>,
}

#[derive(Debug)]
pub struct FailedClient {
    left: TcpStream,
    dest: Destination,
    pending_data: Option<Bytes>,
}

type ConnectServer = Pin<Box<dyn Future<Output = Result<ConnectedClient, FailedClient>> + Send>>;

pub trait Connectable {
    fn connect_server(self, n_parallel: usize) -> ConnectServer;
}

fn error_invalid_input<T>(msg: &'static str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::InvalidInput, msg))
}

fn normalize_socket_addr(socket: &SocketAddr) -> Cow<SocketAddr> {
    match socket {
        SocketAddr::V4(sock) => {
            let addr = sock.ip().to_ipv6_mapped();
            let sock = SocketAddr::new(addr.into(), sock.port());
            Cow::Owned(sock)
        }
        _ => Cow::Borrowed(socket),
    }
}

impl NewClient {
    pub async fn from_socket(mut left: TcpStream, list: ServerList) -> io::Result<Self> {
        let src = left.peer_addr()?;
        let from_port = left.local_addr()?.port();

        // Try to get original destination before NAT
        #[cfg(target_os = "linux")]
        let dest = get_original_dest(&left)
            .map(SocketAddr::V4)
            .or_else(|_| get_original_dest6(&left).map(SocketAddr::V6))
            .or_else(|_| left.local_addr())?;

        // No NAT supported, always be our local address
        #[cfg(not(target_os = "linux"))]
        let dest = left.local_addr()?;

        let is_nated = normalize_socket_addr(&dest) != normalize_socket_addr(&left.local_addr()?);
        debug!("local {} dest {}", left.local_addr()?, dest);
        let dest = if cfg!(target_os = "linux") && is_nated {
            dest.into()
        } else {
            // Not a NATed connection, treated as SOCKSv5
            // Parse version
            // TODO: add timeout
            // TODO: use buffered reader
            let ver = left.read_u8().await?;
            if ver != 0x05 {
                return error_invalid_input("Neither a NATed or SOCKSv5 connection");
            }
            // Parse auth methods
            let n_methods = left.read_u8().await?;
            let mut buf = vec![0u8; n_methods as usize];
            left.read_exact(&mut buf).await?;
            if buf.iter().find(|&&m| m == 0).is_none() {
                return error_invalid_input("SOCKSv5: No auth is required");
            }
            // Select no auth
            left.write_all(&[0x05, 0x00]).await?;
            // Parse request
            buf.resize(4, 0);
            left.read_exact(&mut buf).await?;
            if buf[0..2] != [0x05, 0x01] {
                return error_invalid_input("SOCKSv5: CONNECT is required");
            }
            let addr: Address = match buf[3] {
                0x01 => {
                    // IPv4
                    let mut buf = [0u8; 4];
                    left.read_exact(&mut buf).await?;
                    buf.into()
                }
                0x03 => {
                    // Domain name
                    let len = left.read_u8().await? as usize;
                    buf.resize(len, 0);
                    left.read_exact(&mut buf).await?;
                    let domain = String::from_utf8(buf).map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "SOCKSv5: Invalid domain name")
                    })?;
                    domain.into()
                }
                0x04 => {
                    // IPv6
                    let mut buf = [0u8; 16];
                    left.read_exact(&mut buf).await?;
                    buf.into()
                }
                _ => return error_invalid_input("SOCKSv5: unknown address type"),
            };
            let port = left.read_u16().await?;
            // Send response
            left.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).await?;

            (addr, port).into()
        };
        debug!("dest {:?}", dest);
        Ok(NewClient {
            left,
            src,
            dest,
            list,
            from_port,
        })
    }
}

impl NewClient {
    pub async fn retrive_dest(self) -> io::Result<NewClientWithData> {
        let NewClient {
            mut left,
            src,
            mut dest,
            list,
            from_port,
        } = self;
        let wait = Duration::from_millis(500);
        // try to read TLS ClientHello for
        //   1. --remote-dns: parse host name from SNI
        //   2. --n-parallel: need the whole request to be forwarded
        let mut has_full_tls_hello = false;
        let mut pending_data = None;
        let mut buf = BytesMut::with_capacity(2048);
        buf.resize(buf.capacity(), 0);
        if let Ok(len) = timeout(wait, left.read(&mut buf)).await {
            buf.truncate(len?);
            // only TLS is safe to duplicate requests.
            match tls_parser::parse_client_hello(&buf) {
                Err(err) => info!("fail to parse hello: {}", err),
                Ok(hello) => {
                    has_full_tls_hello = true;
                    if let Some(name) = hello.server_name {
                        dest = (name, dest.port).into();
                        debug!("SNI found: {}", name);
                    }
                    if hello.early_data {
                        debug!("TLS with early data");
                    }
                }
            }
            pending_data = Some(buf.freeze());
        } else {
            info!("no tls request received before timeout");
        }
        Ok(NewClientWithData {
            client: NewClient {
                left,
                src,
                dest,
                list,
                from_port,
            },
            has_full_tls_hello,
            pending_data,
        })
    }

    async fn connect_server(
        self,
        n_parallel: usize,
        wait_response: bool,
        pending_data: Option<Bytes>,
    ) -> Result<ConnectedClient, FailedClient> {
        let NewClient {
            left,
            src,
            dest,
            list,
            from_port,
        } = self;
        let list = list
            .iter()
            .filter(|s| s.serve_port(from_port))
            .cloned()
            .collect();
        let result = try_connect_all(&dest, list, n_parallel, wait_response, pending_data).await;
        if let Some((server, right)) = result {
            info!("[:{}] {} => {} via {}", from_port, src, dest, server.tag);
            Ok(ConnectedClient {
                left,
                right,
                dest,
                server,
            })
        } else {
            warn!("[:{}] {} => {} no avaiable proxy", from_port, src, dest);
            Err(FailedClient {
                left,
                dest,
                pending_data: None,
            })
        }
    }
}

impl Connectable for NewClient {
    fn connect_server(self, _n_parallel: usize) -> ConnectServer {
        Box::pin(self.connect_server(1, false, None))
    }
}

impl Connectable for NewClientWithData {
    fn connect_server(self, n_parallel: usize) -> ConnectServer {
        let NewClientWithData {
            client,
            pending_data,
            has_full_tls_hello,
        } = self;
        let n_parallel = if has_full_tls_hello {
            cmp::min(client.list.len(), n_parallel)
        } else {
            1
        };
        Box::pin(client.connect_server(n_parallel, has_full_tls_hello, pending_data))
    }
}

impl FailedClient {
    pub async fn direct_connect(
        self,
        pseudo_server: Arc<ProxyServer>,
    ) -> io::Result<ConnectedClient> {
        let Self {
            left,
            dest,
            pending_data,
        } = self;
        let mut right = match dest.host {
            Address::Ip(addr) => TcpStream::connect((addr, dest.port)).await?,
            Address::Domain(ref name) => TcpStream::connect((name.as_ref(), dest.port)).await?,
        };
        debug!("connected with {:?}", right.peer_addr());
        right.set_nodelay(true)?;

        if let Some(data) = pending_data {
            right.write_all(&data).await?;
        }

        info!(
            "{} => {} via {}",
            left.peer_addr()?,
            dest,
            pseudo_server.tag
        );
        Ok(ConnectedClient {
            left,
            right,
            dest,
            server: pseudo_server,
        })
    }
}

impl ConnectedClient {
    pub async fn serve(self) -> io::Result<()> {
        let ConnectedClient {
            left,
            right,
            dest,
            server,
        } = self;
        // TODO: make keepalive configurable
        // FIXME: set_cookies
        /*
        let timeout = Some(Duration::from_secs(180));
        FIXME: keepalive
        https://github.com/tokio-rs/tokio/issues/3109

        if let Err(e) = left
            .set_keepalive(timeout)
            .and(right.set_keepalive(timeout))
        {
            warn!("fail to set keepalive: {}", e);
        }
        */
        server.update_stats_conn_open();
        match pipe(left, right, server.clone()).await {
            Ok(amt) => {
                server.update_stats_conn_close(false);
                debug!(
                    "tx {}, rx {} bytes ({} => {})",
                    amt.tx_bytes, amt.rx_bytes, server, dest
                );
                Ok(())
            }
            Err(err) => {
                server.update_stats_conn_close(true);
                info!("{} (=> {}) close with error", server, dest);
                Err(err)
            }
        }
    }
}
