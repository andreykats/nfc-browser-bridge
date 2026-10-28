//! NFC Browser Bridge core: PC/SC monitoring, serialized card access, and the protocol v1
//! WebSocket server. The tray app (bridge-app) is a thin shell around [`Bridge`].

pub mod backend;
pub mod fake;
pub mod health;
pub mod hub;
pub mod monitor;
pub mod protocol;
pub mod real;
pub mod recovery;
pub mod server;
pub mod usb;
#[cfg(target_os = "macos")]
mod usb_macos;
pub mod worker;

use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::sync::Arc;
use std::thread::JoinHandle;

use tokio::sync::watch;

use backend::PcscBackend;
use health::Health;
use hub::Hub;
pub use hub::{BridgeStatus, ServerState, StatusListener};
use recovery::{Recovery, RecoveryConfig};

pub struct BridgeConfig {
    /// 0 picks an ephemeral port (tests).
    pub port: u16,
    pub version: String,
    /// USB access and timings of reader recovery.
    pub recovery: RecoveryConfig,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            port: protocol::DEFAULT_PORT,
            version: env!("CARGO_PKG_VERSION").into(),
            recovery: RecoveryConfig::default(),
        }
    }
}

#[derive(Debug)]
pub enum StartError {
    /// Something already listens on the port.
    PortInUse,
    Io(io::Error),
}

impl std::fmt::Display for StartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PortInUse => write!(f, "port already in use"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StartError {}

/// A running bridge: monitor, worker and reader-recovery threads, and a server thread with its own
/// tokio runtime.
pub struct Bridge<B: PcscBackend> {
    addr: SocketAddr,
    hub: Arc<Hub>,
    health: Arc<Health>,
    monitor: monitor::MonitorHandle<B::Context>,
    recovery: recovery::RecoveryHandle,
    worker_thread: JoinHandle<()>,
    server_thread: JoinHandle<()>,
    shutdown: watch::Sender<bool>,
}

impl<B: PcscBackend> Bridge<B> {
    pub fn start(
        backend: B,
        config: BridgeConfig,
        listener: Option<StatusListener>,
    ) -> Result<Self, StartError> {
        let tcp =
            TcpListener::bind((Ipv4Addr::LOCALHOST, config.port)).map_err(|e| match e.kind() {
                io::ErrorKind::AddrInUse => StartError::PortInUse,
                _ => StartError::Io(e),
            })?;
        tcp.set_nonblocking(true).map_err(StartError::Io)?;
        let addr = tcp.local_addr().map_err(StartError::Io)?;

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(StartError::Io)?;
        let hub = Arc::new(Hub::new(listener));
        let backend = Arc::new(backend);
        let health = Arc::new(Health::default());
        let recovery = Arc::new(Recovery::new(config.recovery, hub.clone(), health.clone()));
        let (worker, worker_thread) = worker::spawn(
            backend.clone(),
            hub.clone(),
            health.clone(),
            recovery.clone(),
        );
        let monitor = monitor::spawn(
            backend,
            hub.clone(),
            worker.clone(),
            health.clone(),
            recovery.reopen_flag(),
        );
        let recovery_thread = recovery::spawn(recovery.clone(), worker.clone());

        let (shutdown, shutdown_rx) = watch::channel(false);
        let ctx = server::ServerCtx {
            hub: hub.clone(),
            worker,
            recovery,
            version: config.version,
            port: addr.port(),
        };
        let server_hub = hub.clone();
        let server_thread = std::thread::Builder::new()
            .name("ws-server".into())
            .spawn(move || {
                runtime.block_on(async move {
                    let listener = match tokio::net::TcpListener::from_std(tcp) {
                        Ok(l) => l,
                        Err(e) => {
                            tracing::error!(error = %e, "cannot use listener");
                            return;
                        }
                    };
                    tracing::info!(%addr, "listening");
                    server_hub.set_server(ServerState::Listening { port: addr.port() });
                    server::serve(listener, ctx, shutdown_rx).await;
                    server_hub.set_server(ServerState::Stopped);
                });
            })
            .map_err(StartError::Io)?;

        Ok(Self {
            addr,
            hub,
            health,
            monitor,
            recovery: recovery_thread,
            worker_thread,
            server_thread,
            shutdown,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn status(&self) -> protocol::Status {
        self.hub.status_snapshot()
    }

    /// What the monitor and worker threads are doing, for a watchdog.
    pub fn health(&self) -> Arc<Health> {
        self.health.clone()
    }

    /// Stop everything and wait for the threads to exit.
    pub fn shutdown(self) {
        let _ = self.shutdown.send(true);
        let _ = self.server_thread.join();
        self.monitor.stop();
        self.recovery.stop();
        // The server, monitor and recovery held the last worker handles; the worker drains and
        // exits.
        let _ = self.worker_thread.join();
    }
}
