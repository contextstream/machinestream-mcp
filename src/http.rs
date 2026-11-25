//! The Streamable HTTP transport: the same server, for container marketplaces and cloud
//! agents. Each request carries its account's key in `Authorization: Bearer`, so one
//! process serves several accounts; the folder a session works in comes from an allowlist.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use crate::server::{MachineStream, Roots};

/// Where the MCP endpoint is served.
pub const MCP_PATH: &str = "/mcp";
/// A plain liveness answer for load balancers and container platforms.
pub const HEALTH_PATH: &str = "/healthz";

/// How `serve --http` is set up.
#[derive(Debug, Clone)]
pub struct HttpOptions {
    pub addr: SocketAddr,
    /// Folders sessions may work in. The first is the default.
    pub roots: Vec<PathBuf>,
    /// `Host` values accepted beside loopback (a public deployment names its own).
    pub allowed_hosts: Vec<String>,
    /// Accept any `Host` (for a deployment whose proxy already checks it).
    pub any_host: bool,
}

/// The router: the MCP endpoint and the health answer.
pub fn router(options: &HttpOptions) -> axum::Router {
    let server = MachineStream::new(Roots::Allowed(options.roots.clone()));
    let mut config = StreamableHttpServerConfig::default();
    if options.any_host {
        config = config.disable_allowed_hosts();
    } else if !options.allowed_hosts.is_empty() {
        let mut hosts = config.allowed_hosts.clone();
        hosts.extend(options.allowed_hosts.iter().cloned());
        config = config.with_allowed_hosts(hosts);
    }
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        config,
    );
    axum::Router::new()
        .route(
            HEALTH_PATH,
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({
                    "status": "ok",
                    "server": crate::server::SERVER_NAME,
                    "version": env!("CARGO_PKG_VERSION"),
                }))
            }),
        )
        .nest_service(MCP_PATH, service)
}

/// Serve until the process is told to stop.
pub async fn serve(options: HttpOptions) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(options.addr).await?;
    let bound = listener.local_addr()?;
    eprintln!(
        "machinestream-mcp: serving MCP at http://{bound}{MCP_PATH} (health: {HEALTH_PATH}); folders: {}",
        options
            .roots
            .iter()
            .map(|root| root.display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    if !bound.ip().is_loopback() && options.allowed_hosts.is_empty() && !options.any_host {
        eprintln!(
            "machinestream-mcp: only loopback Host names are accepted; pass --allowed-host <name> for the name clients use"
        );
    }
    axum::serve(listener, router(&options))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

/// The folder a request works in: the `X-MachineStream-Root` header when it names a folder
/// inside an allowed root, else the first allowed root.
pub fn resolve_root(allowed: &[PathBuf], asked: Option<&str>) -> Result<PathBuf, String> {
    let default = allowed
        .first()
        .ok_or_else(|| "this server has no working folder (start it with --root)".to_string())?;
    let Some(asked) = asked else {
        return Ok(default.clone());
    };
    let asked_path = Path::new(asked);
    let candidate = if asked_path.is_absolute() {
        asked_path.to_path_buf()
    } else {
        default.join(asked_path)
    };
    // Resolved on disk, so `..` and symlinks cannot step outside an allowed root.
    let resolved = std::fs::canonicalize(&candidate)
        .map_err(|_| format!("X-MachineStream-Root names no folder on this server: {asked}"))?;
    if !resolved.is_dir() {
        return Err(format!("X-MachineStream-Root is not a folder: {asked}"));
    }
    let inside = allowed
        .iter()
        .any(|root| std::fs::canonicalize(root).is_ok_and(|root| resolved.starts_with(root)));
    if inside {
        Ok(resolved)
    } else {
        Err(format!(
            "X-MachineStream-Root is outside the folders this server allows: {asked}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_must_be_inside_the_allowlist() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("work");
        let project = allowed.join("project");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let roots = vec![allowed.clone()];

        assert_eq!(resolve_root(&roots, None).unwrap(), allowed);
        let canonical = std::fs::canonicalize(&project).unwrap();
        assert_eq!(resolve_root(&roots, Some("project")).unwrap(), canonical);
        assert_eq!(
            resolve_root(&roots, Some(project.to_str().unwrap())).unwrap(),
            canonical
        );
        assert!(resolve_root(&roots, Some("../outside")).is_err());
        assert!(resolve_root(&roots, Some(outside.to_str().unwrap())).is_err());
        assert!(resolve_root(&roots, Some("missing")).is_err());
        assert!(resolve_root(&[], None).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_allowlist_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let allowed = dir.path().join("work");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, allowed.join("escape")).unwrap();
        assert!(resolve_root(&[allowed], Some("escape")).is_err());
    }
}
