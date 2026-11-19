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
    let token = supervisor.auth_token().to_string();
    let listen_addr = supervisor.listen_addr();
    let addr: SocketAddr = listen_addr
        .parse()
        .with_context(|| format!("invalid listen address '{listen_addr}'"))?;

    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding openvmmsd HTTP server on {addr}"))?;
    let bound_addr = listener.local_addr()?;
    info!(address = %bound_addr, data_dir = %opts.data_dir.display(), "openvmmsd listening");

    let app = server::build_router(supervisor.clone(), token);
    let shutdown_supervisor = supervisor.clone();

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let ctrl_c = async {
                let _ = tokio::signal::ctrl_c().await;
            };
            tokio::select! {
                _ = ctrl_c => {
                    info!("received shutdown signal");
                }
                _ = shutdown_supervisor.wait_for_shutdown_signal() => {
                    info!("received API shutdown request");
                }
            }
            shutdown_supervisor.mark_shutting_down();
            shutdown_supervisor.shutdown_all_vms().await;
        })
        .await
        .context("openvmmsd HTTP server failed")?;

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
        let bound_addr = listener.local_addr()?;
        // Persist actual bound port in host config so ApiClient::from_data_dir works seamlessly.
        let mut cfg = supervisor.host_config();
        cfg.listen_addr = bound_addr.to_string();
        let _ = supervisor.update_host_config(cfg).await?;

        let app = server::build_router(supervisor.clone(), token.clone());
        let shutdown_sup = supervisor.clone();
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    shutdown_sup.wait_for_shutdown_signal().await;
                    shutdown_sup.mark_shutting_down();
                    shutdown_sup.shutdown_all_vms().await;
                })
                .await;
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
