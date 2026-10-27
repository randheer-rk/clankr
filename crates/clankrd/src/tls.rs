//! HTTPS serving with optional client certificates (mTLS for nodes). The verified leaf
//! certificate of each connection is attached to its requests as [`PeerCert`].

use std::sync::Arc;

use axum::{Extension, Router};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder;
use hyper_util::service::TowerToHyperService;
use tokio::net::TcpListener;

/// Client certificate presented on this connection (already verified against the cluster CA).
#[derive(Debug, Clone)]
pub struct PeerCert {
    pub fingerprint: String,
    pub common_name: Option<String>,
}

pub async fn serve_tls(listener: TcpListener, app: Router, config: Arc<rustls::ServerConfig>) -> std::io::Result<()> {
    let acceptor = tokio_rustls::TlsAcceptor::from(config);
    loop {
        let (tcp, addr) = listener.accept().await?;
        let (acceptor, app) = (acceptor.clone(), app.clone());
        tokio::spawn(async move {
            let tls = match tokio::time::timeout(std::time::Duration::from_secs(10), acceptor.accept(tcp)).await {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => return tracing::debug!(%addr, error = %e, "tls handshake failed"),
                Err(_) => return tracing::debug!(%addr, "tls handshake timed out"),
            };
            let peer = tls.get_ref().1.peer_certificates().and_then(|c| c.first()).map(|leaf| PeerCert {
                fingerprint: clankr_tls::fingerprint(leaf),
                common_name: clankr_tls::common_name(leaf),
            });
            let svc = TowerToHyperService::new(app.layer(Extension(peer)));
            if let Err(e) = Builder::new(TokioExecutor::new()).serve_connection_with_upgrades(TokioIo::new(tls), svc).await {
                tracing::debug!(%addr, error = %e, "connection ended");
            }
        });
    }
}
