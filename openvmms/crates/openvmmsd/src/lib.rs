pub mod server;
pub mod supervisor;

use anyhow::Context;
use openvmms_core::models::DEFAULT_DAEMON_LISTEN;
use openvmms_core::models::default_data_dir;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use supervisor::Supervisor;
use tokio::net::TcpListener;
use tracing::info;

#[derive(Debug, Clone)]
pub struct DaemonOptions {
    pub data_dir: PathBuf,
    pub listen_addr: Option<String>,
}

impl Default for DaemonOptions {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            listen_addr: None,
        }
    }
}

pub async fn run_daemon(opts: DaemonOptions) -> anyhow::Result<()> {
    let supervisor = Supervisor::new(&opts.data_dir, opts.listen_addr.as_deref())?;
    let listen_addr = supervisor.listen_addr();
    let addr: SocketAddr = listen_addr
        .parse()
        .with_context(|| format!("invalid listen address '{listen_addr}'"))?;

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding openvmmsd HTTP server on {addr}"))?;
    let bound_addr = supervisor.serve_on_listener(listener)?;
    let mut cfg = supervisor.host_config();
    cfg.listen_addr = bound_addr.to_string();
    let _ = supervisor.update_host_config(cfg).await?;
    info!(address = %bound_addr, data_dir = %opts.data_dir.display(), "openvmmsd listening");

    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    tokio::select! {
        _ = ctrl_c => {
            info!("received shutdown signal");
        }
        _ = supervisor.wait_for_shutdown_signal() => {
            info!("received API shutdown request");
        }
    }
    supervisor.mark_shutting_down();
    supervisor.shutdown_all_vms().await;
    supervisor.join_all_listeners().await;

    Ok(())
}

pub struct TestDaemonHandle {
    pub base_url: String,
    pub token: String,
    pub supervisor: Arc<Supervisor>,
    task: tokio::task::JoinHandle<()>,
}

impl TestDaemonHandle {
    pub async fn spawn(data_dir: PathBuf) -> anyhow::Result<Self> {
        let supervisor = Supervisor::new(&data_dir, Some("127.0.0.1:0"))?;
        let token = supervisor.auth_token().to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let bound_addr = supervisor.serve_on_listener(listener)?;
        // Persist actual bound port in host config so ApiClient::from_data_dir works seamlessly.
        let mut cfg = supervisor.host_config();
        cfg.listen_addr = bound_addr.to_string();
        let _ = supervisor.update_host_config(cfg).await?;

        let shutdown_sup = supervisor.clone();
        let task = tokio::spawn(async move {
            shutdown_sup.wait_for_shutdown_signal().await;
            shutdown_sup.mark_shutting_down();
            shutdown_sup.shutdown_all_vms().await;
            shutdown_sup.join_all_listeners().await;
        });

        Ok(Self {
            base_url: format!("http://{bound_addr}"),
            token,
            supervisor,
            task,
        })
    }

    pub async fn shutdown(self) {
        self.supervisor.request_shutdown();
        let _ = self.task.await;
    }
}

pub fn default_listen_addr() -> &'static str {
    DEFAULT_DAEMON_LISTEN
}
