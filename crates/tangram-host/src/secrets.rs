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
    /// The Phase 10a registry: exactly one resolver, `env://`.
    fn default() -> Self {
        let mut registry = Self::new();
        registry.register(EnvResolver);
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
