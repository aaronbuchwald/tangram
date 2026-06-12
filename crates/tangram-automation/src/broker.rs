//! Primitive C — the credential broker (`task-automation-browser.md` §6).
//!
//! The broker resolves a credential *reference* (`op://vault/item/field`) to
//! a [`SecretString`] host-side, at the point of use, and hands it to a
//! `fill`-style sink that injects it into the browser field. The value:
//!
//! - never enters the LLM snapshot (the field is masked, `script.rs`),
//! - never appears in the recorded script (it stores the reference),
//! - never is logged (it's a [`SecretString`]: redacted Debug, zeroized),
//! - lives only for the duration of one injection, then drops.
//!
//! This crate keeps its OWN minimal resolver so it stays free of a
//! `tangram-host` dependency (the host depends on this crate, not the
//! reverse). It mirrors `tangram-host`'s `op://` resolver (AC0) byte-for-byte
//! in behavior; the host wires its `SecretRegistry` in via [`Resolver`].

use std::path::PathBuf;

use secrecy::{ExposeSecret, SecretString};

/// Resolve a `scheme://locator` reference to a secret value, host-side. The
/// host implements this over its `SecretRegistry`; tests use [`OpCliResolver`]
/// with a fake `op`.
#[async_trait::async_trait]
pub trait Resolver: Send + Sync {
    async fn resolve(&self, reference: &str) -> anyhow::Result<SecretString>;
}

/// The `op://` resolver as used standalone by this crate (mirrors
/// `tangram-host::secrets::OnePasswordResolver`). Validates the reference to a
/// small grammar and shells `op read … --no-newline`; the SA token is read by
/// `op` from inherited env, never an argument here.
pub struct OpCliResolver {
    binary: PathBuf,
}

impl OpCliResolver {
    pub fn new() -> Self {
        Self {
            binary: PathBuf::from("op"),
        }
    }
    pub fn with_binary(binary: impl Into<PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

impl Default for OpCliResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Resolver for OpCliResolver {
    async fn resolve(&self, reference: &str) -> anyhow::Result<SecretString> {
        anyhow::ensure!(
            reference.starts_with("op://"),
            "op resolver got a non-op reference {reference:?}"
        );
        let locator = reference.trim_start_matches("op://");
        anyhow::ensure!(
            !locator.is_empty()
                && locator.bytes().all(|b| {
                    b != 0 && b != b'\n' && b != b'\r' && b != b';' && b != b'`' && b != b' '
                }),
            "op reference {reference:?} has an unsupported character"
        );
        let output = tokio::process::Command::new(&self.binary)
            .arg("read")
            .arg(reference)
            .arg("--no-newline")
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run {} read: {e}", self.binary.display()))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "op read {reference:?} failed ({}): {}",
                output.status,
                stderr.trim()
            );
        }
        let value = String::from_utf8(output.stdout)
            .map_err(|_| anyhow::anyhow!("op read returned non-UTF-8"))?;
        Ok(SecretString::from(value))
    }
}

/// A sink that injects a resolved secret into a browser field. Implemented by
/// the runner over Playwright's `locator.fill`; tests record into a buffer to
/// prove what reached the page. The secret is borrowed for the call only.
#[async_trait::async_trait]
pub trait FillSink: Send {
    /// Fill `target` (an a11y ref / role+name handle) with `value`. MUST NOT
    /// log `value`.
    async fn fill(&mut self, target: &str, value: &str) -> anyhow::Result<()>;
}

/// The credential broker: resolve a reference and inject it, never returning
/// the value to the caller. This is the only place the secret is exposed, and
/// only across a single `fill` call.
pub struct CredentialBroker<R: Resolver> {
    resolver: R,
}

impl<R: Resolver> CredentialBroker<R> {
    pub fn new(resolver: R) -> Self {
        Self { resolver }
    }

    /// Resolve `secret_ref` and `fill` it into `target` via `sink`. The value
    /// exists only between resolve and fill, then drops. Returns no secret.
    pub async fn inject(
        &self,
        secret_ref: &str,
        target: &str,
        sink: &mut impl FillSink,
    ) -> anyhow::Result<()> {
        let secret = self.resolver.resolve(secret_ref).await?;
        // expose_secret() is the single, scoped exposure; the &str lives only
        // for the fill call and the SecretString zeroizes on drop.
        sink.fill(target, secret.expose_secret()).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn fake_op(dir: &std::path::Path, body: &str) -> PathBuf {
        let path = dir.join("op");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// A sink that records (target, value) so the test can assert what the
    /// page received — and that no value escaped anywhere else.
    #[derive(Default)]
    struct RecordingSink {
        filled: Vec<(String, String)>,
    }
    #[async_trait::async_trait]
    impl FillSink for RecordingSink {
        async fn fill(&mut self, target: &str, value: &str) -> anyhow::Result<()> {
            self.filled.push((target.to_string(), value.to_string()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn broker_injects_resolved_value_into_field() {
        let dir = tempfile::tempdir().unwrap();
        let op = fake_op(dir.path(), r#"printf 's3cr3t-pw'"#);
        let broker = CredentialBroker::new(OpCliResolver::with_binary(&op));
        let mut sink = RecordingSink::default();
        broker
            .inject(
                "op://Private/Amazon/password",
                "ref-password-field",
                &mut sink,
            )
            .await
            .unwrap();
        // The value reached exactly the intended field…
        assert_eq!(sink.filled.len(), 1);
        assert_eq!(sink.filled[0].0, "ref-password-field");
        assert_eq!(sink.filled[0].1, "s3cr3t-pw");
    }

    #[tokio::test]
    async fn broker_returns_no_secret_and_errors_on_missing_item() {
        let dir = tempfile::tempdir().unwrap();
        let op = fake_op(dir.path(), r#"echo "no such item" >&2; exit 1"#);
        let broker = CredentialBroker::new(OpCliResolver::with_binary(&op));
        let mut sink = RecordingSink::default();
        let err = broker
            .inject("op://Private/Nope/field", "ref", &mut sink)
            .await
            .unwrap_err();
        // The error names the failure but the sink was never touched.
        assert!(format!("{err:#}").contains("op read"));
        assert!(sink.filled.is_empty());
    }

    #[tokio::test]
    async fn broker_rejects_shellish_reference_before_invoking_op() {
        let dir = tempfile::tempdir().unwrap();
        let op = fake_op(dir.path(), r#"printf pwned"#);
        let broker = CredentialBroker::new(OpCliResolver::with_binary(&op));
        let mut sink = RecordingSink::default();
        let err = broker
            .inject("op://Private/Item/field;evil", "ref", &mut sink)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("unsupported character"));
        assert!(sink.filled.is_empty());
    }
}
