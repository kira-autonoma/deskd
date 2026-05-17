//! Tailscale preflight + tailnet identity check (#462).
//!
//! On hub startup we verify that the local `tailscale` daemon is reachable and
//! healthy by running `tailscale status --json`. The same JSON is used at
//! connection-accept time to verify the source IP belongs to our tailnet —
//! traffic from a non-tailnet IP is rejected with a `diag::warn_event`.
//!
//! Both behaviors share a single trait — `TailnetIdentity` — so tests can stub
//! the shell-out with canned JSON.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;
use std::collections::HashSet;
use std::process::Stdio;
use std::sync::Arc;

/// Source of tailscale identity information. The real implementation shells
/// out to `tailscale status --json`. Tests can implement this trait directly.
#[async_trait]
pub trait TailnetIdentity: Send + Sync {
    /// Return the parsed `tailscale status --json` output. Implementations
    /// should fail fast (and surface a clear error) when the daemon is
    /// unreachable or returns non-zero.
    async fn status(&self) -> Result<TailnetStatus>;
}

/// Subset of `tailscale status --json` we care about. The real CLI emits many
/// more fields; we deserialize liberally with `serde(default)` so it remains
/// forward-compatible.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TailnetStatus {
    /// State of the local node: "Running", "NeedsLogin", "Stopped", etc.
    #[serde(rename = "BackendState", default)]
    pub backend_state: String,
    /// IPs assigned to the local node (typically the tailnet 100.x.x.x and an
    /// IPv6 address).
    #[serde(rename = "TailscaleIPs", default)]
    pub tailscale_ips: Vec<String>,
    /// Peer table keyed by node public key — we only care about the
    /// `TailscaleIPs` of each peer.
    #[serde(rename = "Peer", default)]
    pub peer: std::collections::HashMap<String, TailnetPeer>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TailnetPeer {
    #[serde(rename = "TailscaleIPs", default)]
    pub tailscale_ips: Vec<String>,
}

impl TailnetStatus {
    /// Collect the full set of tailnet IPs (local node + all peers) we should
    /// accept connections from.
    pub fn allowed_ips(&self) -> HashSet<String> {
        let mut s = HashSet::new();
        for ip in &self.tailscale_ips {
            s.insert(ip.clone());
        }
        for peer in self.peer.values() {
            for ip in &peer.tailscale_ips {
                s.insert(ip.clone());
            }
        }
        s
    }

    /// True iff the daemon reports a healthy "Running" state.
    pub fn is_healthy(&self) -> bool {
        self.backend_state == "Running"
    }
}

/// Production implementation — shells out to `tailscale status --json`.
pub struct CliTailnetIdentity {
    /// Binary to invoke. Defaults to `"tailscale"`.
    pub binary: String,
}

impl CliTailnetIdentity {
    pub fn new() -> Self {
        Self {
            binary: "tailscale".into(),
        }
    }
}

impl Default for CliTailnetIdentity {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TailnetIdentity for CliTailnetIdentity {
    async fn status(&self) -> Result<TailnetStatus> {
        let output = tokio::process::Command::new(&self.binary)
            .args(["status", "--json"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .with_context(|| {
                format!(
                    "failed to run `{} status --json` — is the tailscale CLI on PATH?",
                    self.binary
                )
            })?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "`{} status --json` exited with status {:?}: {}",
                self.binary,
                output.status.code(),
                stderr.trim()
            );
        }

        let parsed: TailnetStatus =
            serde_json::from_slice(&output.stdout).context("parse tailscale status JSON")?;
        Ok(parsed)
    }
}

/// Run hub preflight: verify tailscale is healthy. Fails with a clear,
/// operator-actionable error otherwise.
pub async fn preflight_or_error<I: TailnetIdentity + ?Sized>(
    identity: &I,
) -> Result<TailnetStatus> {
    let status = identity.status().await.context(
        "tailscale preflight: status query failed — install tailscale and run `tailscale up`",
    )?;
    if !status.is_healthy() {
        bail!(
            "tailscale preflight: backend not Running (state={:?}) — run `tailscale up` and re-check",
            status.backend_state
        );
    }
    Ok(status)
}

/// Resolve a `bind` spec to a concrete `host:port`. Accepted forms:
///
/// - `<ipv4>:<port>` / `<ipv6>:<port>` / `<host>:<port>` — passed through.
/// - `<interface>:<port>` — resolved via `tailscale ip -4` when interface is
///   `tailscale0`. For other interface names we leave the bind verbatim and
///   let the OS report a clear error; explicit IP/host overrides are the
///   recommended path.
///
/// Returns the resolved address string suitable for `TcpListener::bind`.
pub async fn resolve_bind(bind: &str, tailscale_binary: &str) -> Result<String> {
    let (left, port) = bind
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("federation.hub.bind missing port: {bind}"))?;

    if left.parse::<std::net::IpAddr>().is_ok() {
        return Ok(bind.to_string());
    }

    // Allow plain hostnames (DNS) — TcpListener will resolve.
    // The one special case we explicitly handle is the canonical
    // `tailscale0` interface name.
    if left == "tailscale0" {
        let output = tokio::process::Command::new(tailscale_binary)
            .args(["ip", "-4"])
            .output()
            .await
            .with_context(|| format!("run `{tailscale_binary} ip -4` to resolve {bind}"))?;
        if !output.status.success() {
            bail!(
                "`{} ip -4` exited with status {:?}",
                tailscale_binary,
                output.status.code()
            );
        }
        let ip = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string();
        if ip.is_empty() {
            bail!("`{tailscale_binary} ip -4` returned empty output");
        }
        return Ok(format!("{ip}:{port}"));
    }

    // Fall back to the OS resolver — pass through.
    Ok(bind.to_string())
}

/// Type-erased identity provider shared by hub task + per-connection checks.
pub type SharedIdentity = Arc<dyn TailnetIdentity>;

#[doc(hidden)]
pub mod testing {
    //! Test-only helpers — a `TailnetIdentity` impl backed by canned status.
    //!
    //! Public so external integration tests can construct stub identities
    //! without depending on `cfg(test)`-gated symbols.

    use super::*;
    use std::sync::Mutex;

    /// In-memory `TailnetIdentity` for tests.
    pub struct StubIdentity {
        inner: Mutex<Result<TailnetStatus, String>>,
    }

    impl StubIdentity {
        pub fn ok(status: TailnetStatus) -> Self {
            Self {
                inner: Mutex::new(Ok(status)),
            }
        }

        pub fn err(msg: impl Into<String>) -> Self {
            Self {
                inner: Mutex::new(Err(msg.into())),
            }
        }
    }

    #[async_trait]
    impl TailnetIdentity for StubIdentity {
        async fn status(&self) -> Result<TailnetStatus> {
            match &*self.inner.lock().unwrap() {
                Ok(s) => Ok(s.clone()),
                Err(e) => Err(anyhow::anyhow!(e.clone())),
            }
        }
    }

    pub fn status_with(
        backend_state: &str,
        local_ips: &[&str],
        peer_ips: &[&[&str]],
    ) -> TailnetStatus {
        let mut peer = std::collections::HashMap::new();
        for (i, ips) in peer_ips.iter().enumerate() {
            peer.insert(
                format!("peer-{i}"),
                TailnetPeer {
                    tailscale_ips: ips.iter().map(|s| s.to_string()).collect(),
                },
            );
        }
        TailnetStatus {
            backend_state: backend_state.to_string(),
            tailscale_ips: local_ips.iter().map(|s| s.to_string()).collect(),
            peer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn allowed_ips_includes_local_and_peers() {
        let status = status_with(
            "Running",
            &["100.64.0.1"],
            &[&["100.64.0.2"], &["100.64.0.3", "fd7a:115c::3"]],
        );
        let ips = status.allowed_ips();
        assert!(ips.contains("100.64.0.1"));
        assert!(ips.contains("100.64.0.2"));
        assert!(ips.contains("100.64.0.3"));
        assert!(ips.contains("fd7a:115c::3"));
    }

    #[test]
    fn is_healthy_only_for_running() {
        let mut status = TailnetStatus {
            backend_state: "Running".into(),
            ..Default::default()
        };
        assert!(status.is_healthy());
        status.backend_state = "NeedsLogin".into();
        assert!(!status.is_healthy());
        status.backend_state = "Stopped".into();
        assert!(!status.is_healthy());
    }

    #[tokio::test]
    async fn preflight_passes_when_running() {
        let id = StubIdentity::ok(status_with("Running", &["100.64.0.1"], &[]));
        let status = preflight_or_error(&id).await.unwrap();
        assert!(status.is_healthy());
    }

    #[tokio::test]
    async fn preflight_fails_when_not_running() {
        let id = StubIdentity::ok(status_with("NeedsLogin", &[], &[]));
        let err = preflight_or_error(&id).await.unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("NeedsLogin"), "actual: {s}");
    }

    #[tokio::test]
    async fn preflight_fails_when_daemon_unreachable() {
        let id = StubIdentity::err("tailscale binary not found");
        let err = preflight_or_error(&id).await.unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("tailscale binary not found"), "actual: {s}");
    }

    #[test]
    fn parse_real_tailscale_status_subset() {
        // Subset of the real `tailscale status --json` output.
        let json = r#"{
            "BackendState": "Running",
            "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"],
            "Peer": {
                "abc123": { "TailscaleIPs": ["100.64.0.5"] },
                "def456": { "TailscaleIPs": ["100.64.0.6"] }
            }
        }"#;
        let s: TailnetStatus = serde_json::from_str(json).unwrap();
        assert!(s.is_healthy());
        let ips = s.allowed_ips();
        assert!(ips.contains("100.64.0.1"));
        assert!(ips.contains("100.64.0.5"));
        assert!(ips.contains("100.64.0.6"));
    }

    #[test]
    fn parse_tolerates_unknown_fields() {
        let json = r#"{
            "BackendState": "Running",
            "TailscaleIPs": ["100.64.0.1"],
            "Peer": {},
            "Something": "extra",
            "Version": "1.x"
        }"#;
        let s: TailnetStatus = serde_json::from_str(json).unwrap();
        assert!(s.is_healthy());
    }

    #[tokio::test]
    async fn resolve_bind_passes_explicit_ip() {
        let resolved = resolve_bind("127.0.0.1:7770", "true").await.unwrap();
        assert_eq!(resolved, "127.0.0.1:7770");
    }

    #[tokio::test]
    async fn resolve_bind_missing_port_rejected() {
        let err = resolve_bind("tailscale0", "true").await.unwrap_err();
        let s = format!("{err}");
        assert!(s.contains("missing port"), "actual: {s}");
    }
}
