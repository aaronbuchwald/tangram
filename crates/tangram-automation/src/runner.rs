//! Primitive A — the host-side automation runner
//! (`task-automation-browser.md` §4).
//!
//! Playwright (or any browser driver) runs OUTSIDE the WASM sandbox,
//! supervised by the host exactly the way `gateway.rs` supervises
//! agentgateway: spawn → forward output → restart on crash with [`Backoff`] →
//! kill on shutdown (`kill_on_drop`). A missing driver binary is an
//! enhancement-disabled warning, never a hard dependency — the same posture
//! as the gateway.
//!
//! The `Backoff` ladder is intentionally identical to `gateway.rs`'s (it is
//! the unit-tested pattern the spec says to reuse). When this crate joins the
//! workspace it could be hoisted into a shared `supervisor` module; kept local
//! here so the crate builds standalone.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::AsyncBufReadExt;
use tokio::sync::watch;

/// `[automation]` in `apps.toml`. Read once at startup (like `[gateway]`).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutomationSettings {
    /// Turn the browser automation runner on.
    #[serde(default)]
    pub enabled: bool,
    /// Path to the browser-driver binary (e.g. a Playwright driver / the
    /// `playwright` MCP server). Default: `playwright` on `$PATH`.
    #[serde(default)]
    pub driver: Option<PathBuf>,
    /// Run the browser headless (replay) vs headed (attended record).
    #[serde(default = "default_true")]
    pub headless: bool,
    /// Per-run wall-clock timeout, seconds. Default 300.
    #[serde(default = "default_timeout")]
    pub run_timeout_secs: u64,
    /// The absolute maximum domains any automation may touch on this host —
    /// the `browser_domains_ceiling` (§5.3). Default-deny: empty ⇒ none.
    #[serde(default)]
    pub browser_domains_ceiling: Vec<String>,
    /// Pre-approved, review-approved automation template ids (§4.3). An
    /// [`crate::request::AutomationRequest`] naming a template NOT in this set is
    /// denied outright (default-deny: empty ⇒ no template may run). This is the
    /// operator gate the request→runner dispatch loop intersects against (e.g.
    /// the grocery cart-fill server's `wholefoods-cart`).
    #[serde(default)]
    pub approved_templates: Vec<String>,
    /// Per-app `op://` credential grants: which credential references each app
    /// may request (app name → allowed refs). A request's `credential_refs` are
    /// intersected with the grant for its app (ungranted refs are dropped, never
    /// honored). Absent app ⇒ no credential grant.
    #[serde(default)]
    pub credential_grants: std::collections::BTreeMap<String, Vec<String>>,
    /// Canonical request PATHS that the automation egress ceiling DENIES for
    /// every template — the never-checkout rail. The order-submit path
    /// (`/gp/buy/`) lives here so the browser egress gate (and, in a live run,
    /// the template's `StopGate`) fails closed before checkout. Path PREFIXES:
    /// a request path under a denied prefix is refused.
    #[serde(default)]
    pub denied_paths: Vec<String>,
}

fn default_true() -> bool {
    true
}
fn default_timeout() -> u64 {
    300
}

impl AutomationSettings {
    /// Build the [`crate::request::OperatorPolicy`] this `[automation]` config
    /// expresses, for the given set of apps allowed to request automations. The
    /// `domains_ceiling` is `browser_domains_ceiling`, the approved templates and
    /// per-app credential grants come from the config, and every named app is
    /// allowed to request. This is the single seam the request→runner dispatch
    /// loop intersects each [`crate::request::AutomationRequest`] against.
    pub fn operator_policy<'a>(
        &self,
        allowed_apps: impl IntoIterator<Item = &'a str>,
    ) -> crate::request::OperatorPolicy {
        let mut policy = crate::request::OperatorPolicy::new()
            .ceiling(self.browser_domains_ceiling.iter().cloned());
        for app in allowed_apps {
            policy = policy.allow_app(app);
        }
        for template in &self.approved_templates {
            policy = policy.approve_template(template);
        }
        for (app, refs) in &self.credential_grants {
            policy = policy.grant_credentials(app, refs.iter().cloned());
        }
        policy
    }

    /// Whether `path` falls under any configured `denied_paths` prefix — the
    /// never-checkout rail (the order-submit path is denied for every template).
    /// `path` is compared as a prefix (a request under a denied subtree is
    /// refused). Empty `denied_paths` ⇒ nothing is denied here.
    #[must_use]
    pub fn path_is_denied(&self, path: &str) -> bool {
        self.denied_paths.iter().any(|denied| {
            let denied = denied.trim_end_matches('/');
            path == denied || path.starts_with(&format!("{denied}/"))
        })
    }

    /// The driver to run: the configured path (if executable), else
    /// `playwright` on `$PATH`. `None` ⇒ the disabled-with-warning fallback.
    pub fn resolve_driver(&self) -> Option<PathBuf> {
        match &self.driver {
            Some(path) => is_executable(path).then(|| path.clone()),
            None => find_on_path("playwright"),
        }
    }
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| is_executable(p))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file() && std::fs::metadata(path).is_ok_and(|m| m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Restart backoff for the supervised driver: doubles on quick crashes
/// (500 ms → 10 s cap), resets once a run survives 30 s. Identical ladder to
/// `gateway::Backoff` (the spec's reuse target).
#[derive(Debug)]
pub struct Backoff {
    next: Duration,
}

impl Backoff {
    const MIN: Duration = Duration::from_millis(500);
    const MAX: Duration = Duration::from_secs(10);
    const STABLE: Duration = Duration::from_secs(30);

    pub fn new() -> Self {
        Self { next: Self::MIN }
    }

    pub fn after_exit(&mut self, uptime: Duration) -> Duration {
        if uptime >= Self::STABLE {
            self.next = Self::MIN;
        }
        let delay = self.next;
        self.next = (delay * 2).min(Self::MAX);
        delay
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// A host-managed browser-driver child: supervised process + lifecycle flags.
/// Mirrors `gateway::Gateway`'s supervisor shape (spawn/restart/kill).
pub struct DriverProcess {
    binary: PathBuf,
    args: Vec<String>,
    running: AtomicBool,
    child_pid: AtomicU32,
    shutdown: watch::Sender<bool>,
}

impl DriverProcess {
    pub fn new(binary: PathBuf, args: Vec<String>) -> Self {
        Self {
            binary,
            args,
            running: AtomicBool::new(false),
            child_pid: AtomicU32::new(0),
            shutdown: watch::Sender::new(false),
        }
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    pub fn child_pid(&self) -> u32 {
        self.child_pid.load(Ordering::Relaxed)
    }

    /// Supervise the child: spawn, forward output to tracing, restart on crash
    /// with [`Backoff`], kill on shutdown. Same structure as
    /// `gateway::Gateway::spawn_supervisor`.
    pub fn spawn_supervisor(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let proc = self.clone();
        tokio::spawn(async move {
            let mut shutdown = proc.shutdown.subscribe();
            let mut backoff = Backoff::new();
            loop {
                if *shutdown.borrow() {
                    return;
                }
                let mut child = match tokio::process::Command::new(&proc.binary)
                    .args(&proc.args)
                    .stdin(Stdio::null())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()
                {
                    Ok(child) => child,
                    Err(e) => {
                        tracing::error!(
                            "automation: failed to spawn driver {}: {e}",
                            proc.binary.display()
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(backoff.after_exit(Duration::ZERO)) => continue,
                            _ = shutdown.changed() => return,
                        }
                    }
                };
                let pid = child.id().unwrap_or(0);
                proc.child_pid.store(pid, Ordering::Relaxed);
                proc.running.store(true, Ordering::Relaxed);
                tracing::info!("automation: browser driver running (pid {pid})");
                forward_output(&mut child);

                let started = std::time::Instant::now();
                tokio::select! {
                    status = child.wait() => {
                        proc.running.store(false, Ordering::Relaxed);
                        proc.child_pid.store(0, Ordering::Relaxed);
                        let delay = backoff.after_exit(started.elapsed());
                        tracing::warn!(
                            "automation: browser driver exited ({status:?}) — restarting in {delay:?}"
                        );
                        tokio::select! {
                            _ = tokio::time::sleep(delay) => {}
                            _ = shutdown.changed() => return,
                        }
                    }
                    _ = shutdown.changed() => {
                        proc.running.store(false, Ordering::Relaxed);
                        proc.child_pid.store(0, Ordering::Relaxed);
                        let _ = child.kill().await;
                        tracing::info!("automation: browser driver stopped");
                        return;
                    }
                }
            }
        })
    }

    /// Stop the supervisor and kill the child (also `kill_on_drop`).
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

fn forward_output(child: &mut tokio::process::Child) {
    if let Some(stdout) = child.stdout.take() {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "browser-driver", "{line}");
            }
        });
    }
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::warn!(target: "browser-driver", "{line}");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_parse_and_default_off() {
        let s: AutomationSettings = toml::from_str("enabled = true").unwrap();
        assert!(s.enabled);
        assert!(s.headless, "headless defaults true");
        assert_eq!(s.run_timeout_secs, 300);
        assert!(s.browser_domains_ceiling.is_empty(), "default-deny");
        assert!(s.approved_templates.is_empty(), "default-deny templates");
        assert!(s.credential_grants.is_empty(), "default-deny credentials");
        assert!(s.denied_paths.is_empty());
        assert!(!AutomationSettings::default().enabled, "default off");

        let s: AutomationSettings = toml::from_str(
            "enabled = true\ndriver = \"/x/playwright\"\nheadless = false\n\
             run_timeout_secs = 60\nbrowser_domains_ceiling = [\"www.amazon.com\"]",
        )
        .unwrap();
        assert_eq!(s.driver, Some(PathBuf::from("/x/playwright")));
        assert!(!s.headless);
        assert_eq!(s.run_timeout_secs, 60);
        assert_eq!(s.browser_domains_ceiling, vec!["www.amazon.com"]);
    }

    #[test]
    fn resolve_driver_missing_is_none() {
        let s = AutomationSettings {
            enabled: true,
            driver: Some(PathBuf::from("/nonexistent/playwright-xyz")),
            ..Default::default()
        };
        assert_eq!(s.resolve_driver(), None, "missing binary disables (warn)");
    }

    #[test]
    fn backoff_doubles_and_resets_like_gateway() {
        let mut b = Backoff::new();
        let crash = Duration::from_millis(10);
        assert_eq!(b.after_exit(crash), Duration::from_millis(500));
        assert_eq!(b.after_exit(crash), Duration::from_secs(1));
        assert_eq!(b.after_exit(crash), Duration::from_secs(2));
        assert_eq!(b.after_exit(crash), Duration::from_secs(4));
        assert_eq!(b.after_exit(crash), Duration::from_secs(8));
        assert_eq!(b.after_exit(crash), Duration::from_secs(10), "capped");
        assert_eq!(b.after_exit(crash), Duration::from_secs(10), "stays");
        assert_eq!(
            b.after_exit(Duration::from_secs(31)),
            Duration::from_millis(500),
            "stable run resets"
        );
    }

    #[test]
    fn operator_policy_is_built_from_settings() {
        use crate::request::{AutomationRequest, authorize};
        let s: AutomationSettings = toml::from_str(
            "enabled = true\n\
             browser_domains_ceiling = [\"www.amazon.com\", \"www.wholefoodsmarket.com\"]\n\
             approved_templates = [\"wholefoods-cart\"]\n\
             denied_paths = [\"/gp/buy/\"]\n\
             [credential_grants]\n\
             \"grocery-cart\" = [\"op://Private/Amazon/password\"]",
        )
        .unwrap();
        let policy = s.operator_policy(["grocery-cart"]);

        // An in-policy request authorizes, narrowed to the ceiling + grant.
        let req = AutomationRequest {
            app: "grocery-cart".into(),
            template_id: "wholefoods-cart".into(),
            params: vec![],
            domains: vec!["www.amazon.com".into(), "attacker.com".into()],
            credential_refs: vec![
                "op://Private/Amazon/password".into(),
                "op://Private/Bank/pw".into(),
            ],
        };
        let auth = authorize(&req, &policy).unwrap();
        assert_eq!(auth.domains, vec!["www.amazon.com"]); // off-ceiling trimmed
        assert_eq!(auth.credential_refs, vec!["op://Private/Amazon/password"]); // ungranted dropped

        // An unapproved template is denied outright (default-deny).
        let mut bad = req.clone();
        bad.template_id = "place-order".into();
        assert!(authorize(&bad, &policy).is_err());
    }

    #[test]
    fn never_checkout_path_deny() {
        let s: AutomationSettings =
            toml::from_str("enabled = true\ndenied_paths = [\"/gp/buy/\"]").unwrap();
        // The order-submit subtree is denied; an unrelated path is not.
        assert!(s.path_is_denied("/gp/buy/spc/handlers/display.html"));
        assert!(s.path_is_denied("/gp/buy/"));
        assert!(s.path_is_denied("/gp/buy"));
        assert!(!s.path_is_denied("/gp/cart/view.html"));
        assert!(!s.path_is_denied("/s")); // search
    }

    #[tokio::test]
    async fn supervisor_spawns_restarts_and_kills() {
        // A child that exits immediately should be restarted (running flips
        // true at least once), and shutdown must stop the loop.
        let proc = Arc::new(DriverProcess::new(
            PathBuf::from("/bin/sh"),
            vec!["-c".into(), "sleep 0.05".into()],
        ));
        let handle = proc.spawn_supervisor();
        // Give it time to spawn at least once.
        tokio::time::sleep(Duration::from_millis(120)).await;
        proc.shutdown();
        // The supervisor task should finish promptly after shutdown.
        tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("supervisor stops on shutdown")
            .expect("task joined");
        assert!(!proc.running());
    }

    #[tokio::test]
    async fn supervisor_handles_missing_binary_without_crashing() {
        let proc = Arc::new(DriverProcess::new(
            PathBuf::from("/nonexistent/driver-binary"),
            vec![],
        ));
        let handle = proc.spawn_supervisor();
        tokio::time::sleep(Duration::from_millis(50)).await;
        proc.shutdown();
        tokio::time::timeout(Duration::from_secs(3), handle)
            .await
            .expect("stops")
            .expect("joined");
        assert!(!proc.running());
    }
}
