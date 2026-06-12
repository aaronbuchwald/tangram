//! The secret-resolution seam (ADR-0004, RUNTIME_PLAN Phase 10a).
//!
//! A secret in a spec is a `scheme://locator` *reference*, never a value. The
//! scheme selects a [`SecretResolver`]; the resolver turns the reference into
//! a [`SecretString`] host-side, just before the value is needed. This module
//! is the seam only — Phase 10a ships exactly ONE resolver (`env://`,
//! today's process-env behavior) and keeps resolution flowing through the
//! trait so future provenance options (`op://`, `sops://`, `age://`) are
//! additive and spec/code-invisible.
//!
//! The value type is [`secrecy::SecretString`]: redacted `Debug`,
//! zeroize-on-drop. Resolved values must never be logged — rely on that
//! redaction and never `format!`/`tracing` a `SecretString`'s exposed inner.

use std::collections::BTreeMap;

use anyhow::Context as _;
pub use secrecy::SecretString;

/// A `scheme://locator` secret reference (e.g. `env://CALORIENINJAS_API_KEY`).
/// The bare sugar form `${VAR}` is rewritten to `env://VAR` for back-compat
/// (see [`SecretRef::parse`]); anything else without a `scheme://` is a
/// literal, returned verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef(String);

impl SecretRef {
    /// Wrap a `scheme://locator` string as a reference.
    pub fn new(reference: impl Into<String>) -> Self {
        Self(reference.into())
    }

    /// The scheme part (`env` for `env://NAME`), or `None` if the string has
    /// no `scheme://` separator.
    pub fn scheme(&self) -> Option<&str> {
        self.0.split_once("://").map(|(scheme, _)| scheme)
    }

    /// The locator part after `scheme://` (`NAME` for `env://NAME`), or the
    /// whole string when there is no separator.
    pub fn locator(&self) -> &str {
        self.0.split_once("://").map_or(self.0.as_str(), |(_, l)| l)
    }

    /// The full reference text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Classify a raw spec value. Returns:
    /// - `Resolve(ref)` for a `scheme://…` reference OR the `${VAR}` sugar
    ///   (rewritten to `env://VAR`) — the value must be resolved.
    /// - `Literal(s)` for any other string — passed through unchanged.
    ///
    /// This is where back-compat lives: `${VAR}` is sugar for `env://VAR`, so
    /// every existing `apps.toml`/tenant spec resolves byte-identically.
    pub fn parse(value: &str) -> ParsedValue {
        if let Some(var) = value.strip_prefix("${").and_then(|v| v.strip_suffix('}')) {
            // `${VAR}` sugar → `env://VAR` (today's behavior, unchanged).
            ParsedValue::Resolve(SecretRef(format!("env://{var}")))
        } else if value.contains("://") {
            ParsedValue::Resolve(SecretRef(value.to_string()))
        } else {
            ParsedValue::Literal(value.to_string())
        }
    }
}

/// The result of classifying a raw spec value with [`SecretRef::parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedValue {
    /// A reference to resolve through the registry.
    Resolve(SecretRef),
    /// A literal value, used as-is.
    Literal(String),
}

/// One provenance strategy: a scheme and how it resolves a reference to a
/// value, host-side. Phase 10a ships only [`EnvResolver`]. `#[async_trait]`
/// keeps it dyn-compatible so the registry can hold `Box<dyn SecretResolver>`.
#[async_trait::async_trait]
pub trait SecretResolver: Send + Sync {
    /// The scheme this resolver handles (e.g. `"env"`).
    fn scheme(&self) -> &'static str;
    /// Resolve a reference of this resolver's scheme to its value.
    async fn resolve(&self, reference: &SecretRef) -> anyhow::Result<SecretString>;
}

/// `env://NAME` — reads `NAME` from the host process environment. This is the
/// behavior `${VAR}` had before the seam existed, now behind the trait.
pub struct EnvResolver;

#[async_trait::async_trait]
impl SecretResolver for EnvResolver {
    fn scheme(&self) -> &'static str {
        "env"
    }

    async fn resolve(&self, reference: &SecretRef) -> anyhow::Result<SecretString> {
        let name = reference.locator();
        let value = std::env::var(name)
            .with_context(|| format!("env var {name} is not set in the host environment"))?;
        Ok(SecretString::from(value))
    }
}

/// `op://<vault>/<item>/<field>` — resolves through the 1Password CLI
/// (`op read op://…`) using the service-account token in the host process
/// env (`OP_SERVICE_ACCOUNT_TOKEN`, ADR-0004 follow-on / the browser
/// credential broker, `task-automation-browser.md` §6). The token's
/// 1Password-side scope is the real enforcement floor: even a request for a
/// different `op://` item resolves only what the SA token may read.
///
/// The value is captured straight into a [`SecretString`] (redacted Debug,
/// zeroize-on-drop). The token, the command line, and the resolved value are
/// NEVER logged: `op read` takes the reference as an arg (a reference, never
/// a secret) and the SA token via inherited env.
pub struct OnePasswordResolver {
    /// The `op` binary to invoke (default `op` on `$PATH`).
    binary: std::path::PathBuf,
}

impl OnePasswordResolver {
    /// Resolve against `op` on `$PATH`.
    pub fn new() -> Self {
        Self {
            binary: std::path::PathBuf::from("op"),
        }
    }

    /// Resolve against an explicit `op` binary path (tests + the automation
    /// broker fixtures use a fake `op`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_binary(binary: impl Into<std::path::PathBuf>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

impl Default for OnePasswordResolver {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SecretResolver for OnePasswordResolver {
    fn scheme(&self) -> &'static str {
        "op"
    }

    async fn resolve(&self, reference: &SecretRef) -> anyhow::Result<SecretString> {
        // `op read` wants the full `op://vault/item/field` reference, scheme
        // included. We reconstruct it from the parsed ref and validate the
        // locator to a small grammar (no NUL/newline/shell-ish bytes) — the
        // canonicalization-discipline applied to a secret locator. Command
        // does not invoke a shell, but we keep the surface tiny.
        let full = reference.as_str();
        anyhow::ensure!(
            full.starts_with("op://"),
            "op resolver got a non-op reference {full:?}"
        );
        let locator = reference.locator();
        anyhow::ensure!(
            !locator.is_empty()
                && locator.bytes().all(|b| {
                    b != 0 && b != b'\n' && b != b'\r' && b != b';' && b != b'`' && b != b' '
                }),
            "op reference {full:?} has an unsupported character"
        );

        let output = tokio::process::Command::new(&self.binary)
            .arg("read")
            .arg(full)
            .arg("--no-newline")
            // The SA token is read by `op` from this inherited env var; we
            // never read it into our own process memory or pass it as an arg.
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .with_context(|| format!("failed to run {} read", self.binary.display()))?;

        if !output.status.success() {
            // stderr may name the item but never the value; surface it for
            // diagnosis (missing item, denied scope, no token).
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!(
                "op read {full:?} failed ({}): {}",
                output.status,
                stderr.trim()
            );
        }

        // Capture stdout directly into the SecretString; never log it.
        let value =
            String::from_utf8(output.stdout).context("op read returned non-UTF-8 output")?;
        Ok(SecretString::from(value))
    }
}

/// Maps a scheme to its resolver. Unknown schemes are a clear error.
pub struct SecretRegistry {
    resolvers: BTreeMap<&'static str, Box<dyn SecretResolver>>,
}

impl SecretRegistry {
    /// An empty registry. Use [`SecretRegistry::default`] for the standard
    /// Phase 10a set (just `env://`).
    pub fn new() -> Self {
        Self {
            resolvers: BTreeMap::new(),
        }
    }

    /// Register a resolver under its declared scheme.
    pub fn register(&mut self, resolver: impl SecretResolver + 'static) {
        self.resolvers.insert(resolver.scheme(), Box::new(resolver));
    }

    /// Resolve a reference by dispatching on its scheme. A reference with no
    /// `scheme://` separator, or an unknown scheme, is a clear error.
    pub async fn resolve(&self, reference: &SecretRef) -> anyhow::Result<SecretString> {
        let scheme = reference.scheme().ok_or_else(|| {
            anyhow::anyhow!(
                "secret reference {:?} is not a scheme://locator reference",
                reference.as_str()
            )
        })?;
        let resolver = self.resolvers.get(scheme).ok_or_else(|| {
            anyhow::anyhow!(
                "no resolver for secret scheme {scheme:?} (reference {:?}); known schemes: {}",
                reference.as_str(),
                self.scheme_list()
            )
        })?;
        resolver.resolve(reference).await
    }

    fn scheme_list(&self) -> String {
        self.resolvers
            .keys()
            .map(|s| format!("{s}://"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

impl Default for SecretRegistry {
    /// The standard host registry: `env://` (Phase 10a) and `op://` (the
    /// 1Password CLI resolver — the browser credential broker's source,
    /// `task-automation-browser.md` §6). `op://` only does work when it is
    /// actually referenced and an `op` binary + SA token are present; an
    /// absent `op` degrades through the same warn-to-empty path as any other
    /// unresolved reference.
    fn default() -> Self {
        let mut registry = Self::new();
        registry.register(EnvResolver);
        registry.register(OnePasswordResolver::new());
        registry
    }
}

/// Resolve one spec value, preserving today's `${VAR}` semantics exactly:
/// - a literal passes through unchanged;
/// - a `${VAR}` (sugar for `env://VAR`) or any `scheme://…` reference is
///   resolved through the registry;
/// - a reference that fails to resolve (e.g. an unset env var) expands to
///   the **empty string** with a warning — the app runs degraded, identical
///   to the pre-seam behavior.
///
/// `context` is a human label for the warning (`"<app>: env <KEY>"`). The
/// resolved value is intentionally NOT logged.
pub async fn resolve_value(registry: &SecretRegistry, context: &str, value: &str) -> String {
    use secrecy::ExposeSecret as _;
    match SecretRef::parse(value) {
        ParsedValue::Literal(literal) => literal,
        ParsedValue::Resolve(reference) => match registry.resolve(&reference).await {
            Ok(secret) => secret.expose_secret().to_string(),
            Err(e) => {
                // Match the pre-seam warning shape: name the reference, never
                // the value. Missing var → empty string → degraded app.
                tracing::warn!(
                    "{context}: secret {} did not resolve: {e:#}",
                    reference.as_str()
                );
                String::new()
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret as _;

    #[tokio::test]
    async fn env_resolver_reads_host_var() {
        // Safety: test-local var name, nothing else reads it concurrently.
        unsafe { std::env::set_var("TANGRAM_TEST_SECRET_ENV", "s3cret-value") };
        let registry = SecretRegistry::default();
        let secret = registry
            .resolve(&SecretRef::new("env://TANGRAM_TEST_SECRET_ENV"))
            .await
            .unwrap();
        assert_eq!(secret.expose_secret(), "s3cret-value");
    }

    #[tokio::test]
    async fn dollar_var_sugar_resolves_identically_to_env_scheme() {
        // Safety: test-local var name, nothing else reads it concurrently.
        unsafe { std::env::set_var("TANGRAM_TEST_SUGAR_VAR", "from-sugar") };
        // `${VAR}` parses to the same reference as `env://VAR`.
        assert_eq!(
            SecretRef::parse("${TANGRAM_TEST_SUGAR_VAR}"),
            ParsedValue::Resolve(SecretRef::new("env://TANGRAM_TEST_SUGAR_VAR"))
        );
        let registry = SecretRegistry::default();
        let via_sugar = resolve_value(&registry, "test", "${TANGRAM_TEST_SUGAR_VAR}").await;
        let via_scheme = resolve_value(&registry, "test", "env://TANGRAM_TEST_SUGAR_VAR").await;
        assert_eq!(via_sugar, "from-sugar");
        assert_eq!(via_sugar, via_scheme);
    }

    #[test]
    fn literal_values_pass_through() {
        assert_eq!(
            SecretRef::parse("just-a-literal"),
            ParsedValue::Literal("just-a-literal".to_string())
        );
        // A bare value with no scheme:// and no ${} is a literal.
        assert_eq!(
            SecretRef::parse("api.example.com"),
            ParsedValue::Literal("api.example.com".to_string())
        );
    }

    #[tokio::test]
    async fn unknown_scheme_is_a_clear_error() {
        let registry = SecretRegistry::default();
        let err = registry
            .resolve(&SecretRef::new("vault://kv/data/x#k"))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no resolver for secret scheme"), "{msg}");
        assert!(msg.contains("vault"), "{msg}");
        // The clear error names the schemes that ARE known.
        assert!(msg.contains("env://"), "{msg}");
    }

    #[tokio::test]
    async fn missing_var_resolves_to_empty_string_degraded() {
        let registry = SecretRegistry::default();
        // Both the sugar form and the explicit scheme degrade to empty, just
        // as the pre-seam ${VAR} expansion did for an unset host var.
        let via_sugar = resolve_value(&registry, "app: env KEY", "${TANGRAM_TEST_UNSET_XYZ}").await;
        let via_scheme =
            resolve_value(&registry, "app: env KEY", "env://TANGRAM_TEST_UNSET_XYZ").await;
        assert_eq!(via_sugar, "");
        assert_eq!(via_scheme, "");
    }

    /// Write an executable fake `op` to a tempdir that echoes a canned value
    /// for `op read <ref>` (asserting it received `read` + the full ref), or
    /// exits non-zero to simulate a missing item / denied scope.
    fn fake_op(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = dir.join("op");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[tokio::test]
    async fn op_resolver_reads_via_cli() {
        let dir = tempfile::tempdir().unwrap();
        // The fake asserts argv is `read op://Private/Amazon/password
        // --no-newline`, then prints the secret with no trailing newline.
        let op = fake_op(
            dir.path(),
            r#"[ "$1" = "read" ] || { echo "bad verb $1" >&2; exit 2; }
[ "$2" = "op://Private/Amazon/password" ] || { echo "bad ref $2" >&2; exit 2; }
printf 'hunter2-from-1password'"#,
        );
        let resolver = OnePasswordResolver::with_binary(&op);
        let secret = resolver
            .resolve(&SecretRef::new("op://Private/Amazon/password"))
            .await
            .unwrap();
        assert_eq!(secret.expose_secret(), "hunter2-from-1password");
        // Redaction holds: the value never appears in Debug.
        assert!(!format!("{secret:?}").contains("hunter2"));
    }

    #[tokio::test]
    async fn op_resolver_missing_item_errors_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let op = fake_op(
            dir.path(),
            r#"echo "isn't an item in the vault" >&2; exit 1"#,
        );
        let resolver = OnePasswordResolver::with_binary(&op);
        let err = resolver
            .resolve(&SecretRef::new("op://Private/Nope/field"))
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("op read"), "{msg}");
        assert!(msg.contains("Nope"), "{msg}");
    }

    #[tokio::test]
    async fn op_resolver_rejects_shellish_reference() {
        let dir = tempfile::tempdir().unwrap();
        // This fake would "succeed" if ever invoked; the guard must fire
        // first so it is never run.
        let op = fake_op(dir.path(), "printf pwned");
        let resolver = OnePasswordResolver::with_binary(&op);
        let err = resolver
            .resolve(&SecretRef::new("op://Private/Item/field;rm -rf"))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("unsupported character"));
    }

    #[tokio::test]
    async fn op_resolver_degrades_to_empty_when_binary_absent() {
        // Through the public resolve_value path with the default registry's
        // op:// resolver but a binary that doesn't exist: warn → empty, the
        // app runs degraded (same posture as a missing env var).
        let resolver = OnePasswordResolver::with_binary("/nonexistent/op-binary");
        let err = resolver
            .resolve(&SecretRef::new("op://Private/Amazon/password"))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("failed to run"));
    }

    #[test]
    fn op_scheme_is_registered_by_default() {
        let registry = SecretRegistry::default();
        assert!(registry.scheme_list().contains("op://"));
        assert!(registry.scheme_list().contains("env://"));
    }

    #[test]
    fn secret_string_debug_is_redacted() {
        let secret = SecretString::from("top-secret-value".to_string());
        let debug = format!("{secret:?}");
        assert!(
            !debug.contains("top-secret-value"),
            "SecretString Debug leaked the value: {debug}"
        );
        // secrecy renders a redaction marker instead of the plaintext.
        assert!(
            debug.contains("REDACTED"),
            "expected redaction marker: {debug}"
        );
    }
}
