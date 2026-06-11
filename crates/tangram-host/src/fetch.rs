//! Install-from-URL (RUNTIME_PLAN Phase 8): downloading component artifacts
//! and verifying their pinned sha-256 BEFORE anything instantiates them.
//!
//! Verified artifacts land in an immutable, content-addressed cache under
//! the host data root (`$HOME/.tangram-host/components/<sha256>.wasm`):
//! re-converging on a spec with the same hash — including after a host
//! restart — is a filesystem hit, never a refetch. A failed fetch or a hash
//! mismatch is a converge error (surfaced in the fleet status) and is
//! remembered briefly so the 2-second converge tick doesn't hammer the
//! artifact server while the spec stays broken.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sha2::{Digest as _, Sha256};

/// How long a failed (url, sha256) fetch is remembered before the converge
/// loop is allowed to retry it.
const RETRY_AFTER: Duration = Duration::from_secs(30);

/// The four magic bytes every WebAssembly binary starts with (`\0asm`).
const WASM_MAGIC: &[u8] = b"\0asm";

/// Default cache root: the host data root (the same place the generated
/// agentgateway config lives), `data/tangram-host` without a HOME.
pub fn default_cache_dir() -> PathBuf {
    match std::env::var("HOME") {
        Ok(home) => PathBuf::from(home).join(".tangram-host/components"),
        Err(_) => PathBuf::from("data/tangram-host/components"),
    }
}

/// The immutable cache slot for a digest: `<cache>/<sha256>.wasm`. The file
/// is only ever created by an atomic rename AFTER verification, so a present
/// slot is trusted as-is.
pub fn cache_path(cache_dir: &Path, sha256: &str) -> PathBuf {
    cache_dir.join(format!("{sha256}.wasm"))
}

/// Reject anything that is not a real `wasm32-wasip2` COMPONENT before it is
/// stored: a cheap magic-byte check, then a full wasmtime parse+validate
/// (`Component::new`, which rejects core modules and malformed binaries).
/// This is the type/shape MUST-FIX item for open upload — it stops the
/// content-addressed store from becoming arbitrary-blob storage of garbage.
/// (A deeper closed-world import audit — rejecting `wasi:sockets`/`wasi:http`
/// — is the marketplace's existing displayed audit and is the remaining
/// MUST-FIX content control before public exposure.)
pub fn validate_wasm_component(engine: &wasmtime::Engine, bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 8 || &bytes[..4] != WASM_MAGIC {
        return Err("not a WebAssembly binary (missing the \\0asm magic header)".into());
    }
    // `Component::new` validates the full binary and rejects a bare core
    // module (the wasm-component layer's preamble differs). A core module or
    // corrupt binary fails here with wasmtime's own message.
    wasmtime::component::Component::new(engine, bytes)
        .map(|_| ())
        .map_err(|e| format!("not a valid wasm component: {e}"))
}

/// Downloads + verifies component artifacts into the content-addressed
/// cache. One per host; converge calls [`Fetcher::resolve`] for every
/// URL-sourced spec on every pass, which is a cache stat in steady state.
pub struct Fetcher {
    client: reqwest::Client,
    cache_dir: PathBuf,
    /// Recent failures, keyed `(url, sha256)` → (when, error): converge
    /// re-reports the remembered error instead of refetching until
    /// [`RETRY_AFTER`] has passed.
    failures: tokio::sync::Mutex<HashMap<(String, String), (Instant, String)>>,
}

impl Fetcher {
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            client: reqwest::Client::new(),
            cache_dir,
            failures: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// The content-addressed store root — the same directory the verified
    /// install-by-URL cache lives in. An UPLOADED artifact (the marketplace
    /// upload flow) lands here too, so the existing install pipeline can
    /// fetch it by hash from `GET /artifacts/<sha>.wasm` exactly as it would
    /// any external URL.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// The on-disk path of a stored artifact IFF it exists (a present slot is
    /// trusted: write-once after verification). Backs `GET /artifacts/<sha>.wasm`.
    pub fn artifact_path(&self, sha256: &str) -> Option<PathBuf> {
        let path = cache_path(&self.cache_dir, sha256);
        path.exists().then_some(path)
    }

    /// Store an UPLOADED component blob in the content-addressed cache: the
    /// HOST validates it is a real wasm component (magic bytes + a wasmtime
    /// parse — see [`validate_wasm_component`]), computes the sha-256 ITSELF
    /// (the uploader never asserts it), and atomically commits the bytes to
    /// `<cache>/<sha256>.wasm` (write-once, dedup by hash). Returns the
    /// computed digest, which is immediately installable by URL
    /// `/artifacts/<sha256>.wasm` + that hash.
    ///
    /// This is open-blob storage when the route is enabled — see the
    /// default-off gate and MUST-FIX checklist at the route in `routes.rs`
    /// and in `crates/tangram-host/README.md`. The wasm-validity check here
    /// is the one MUST-FIX item already met; size/rate/quota/abuse controls
    /// are NOT and must be added before public exposure.
    pub fn store_artifact(
        &self,
        engine: &wasmtime::Engine,
        bytes: &[u8],
    ) -> Result<String, String> {
        validate_wasm_component(engine, bytes)?;
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let path = cache_path(&self.cache_dir, &sha256);
        if path.exists() {
            // Identical bytes already stored — dedup, nothing to write.
            return Ok(sha256);
        }
        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| format!("creating cache dir {}: {e}", self.cache_dir.display()))?;
        // Write-then-rename so a crash can't leave a partial artifact at the
        // content address (same discipline as a verified fetch). A `.upload`
        // suffix avoids racing the converge fetcher's `.tmp` slot.
        let tmp = self.cache_dir.join(format!(".{sha256}.upload"));
        std::fs::write(&tmp, bytes).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("committing {}: {e}", path.display()))?;
        Ok(sha256)
    }

    /// Resolve a `component_url` spec to a verified local path: a cache hit
    /// returns immediately, otherwise the artifact is fetched, its sha-256
    /// checked against the pinned digest, and only a verified artifact is
    /// committed to the cache (atomic rename). The error string is what the
    /// fleet status shows.
    pub async fn resolve(&self, app: &str, url: &str, sha256: &str) -> Result<PathBuf, String> {
        let path = cache_path(&self.cache_dir, sha256);
        if path.exists() {
            return Ok(path);
        }

        let key = (url.to_string(), sha256.to_string());
        let mut failures = self.failures.lock().await;
        if let Some((when, error)) = failures.get(&key)
            && when.elapsed() < RETRY_AFTER
        {
            return Err(error.clone());
        }
        // Holding the lock during the fetch also serializes concurrent
        // downloads of the same artifact (converge is single-flight anyway).
        match self.fetch_verified(url, sha256, &path).await {
            Ok(()) => {
                failures.remove(&key);
                tracing::info!(
                    "{app}: fetched {url} → {} (sha-256 verified)",
                    path.display()
                );
                Ok(path)
            }
            Err(e) => {
                let message = format!("component fetch failed: {e:#}");
                tracing::error!("{app}: {message}");
                failures.insert(key, (Instant::now(), message.clone()));
                Err(message)
            }
        }
    }

    async fn fetch_verified(&self, url: &str, sha256: &str, path: &Path) -> anyhow::Result<()> {
        let response = self
            .client
            .get(url)
            .timeout(Duration::from_secs(120))
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("fetching {url}: {e}"))?;
        anyhow::ensure!(
            response.status().is_success(),
            "fetching {url}: HTTP {}",
            response.status()
        );
        let body = response
            .bytes()
            .await
            .map_err(|e| anyhow::anyhow!("reading {url}: {e}"))?;

        // The verification gate: nothing unverified ever reaches the cache
        // (and therefore nothing unverified is ever instantiated).
        let actual = format!("{:x}", Sha256::digest(&body));
        anyhow::ensure!(
            actual == sha256,
            "sha256 mismatch for {url}: expected {sha256}, got {actual} — refusing to \
             install the artifact"
        );

        std::fs::create_dir_all(&self.cache_dir)
            .map_err(|e| anyhow::anyhow!("creating cache dir {}: {e}", self.cache_dir.display()))?;
        // Write-then-rename so a crash can't leave a partial artifact at the
        // content address.
        let tmp = self.cache_dir.join(format!(".{sha256}.tmp"));
        std::fs::write(&tmp, &body)
            .map_err(|e| anyhow::anyhow!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| anyhow::anyhow!("committing {}: {e}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::IntoFuture as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const ARTIFACT: &[u8] = b"not really wasm, but hashable";

    fn artifact_sha() -> String {
        format!("{:x}", Sha256::digest(ARTIFACT))
    }

    /// A scratch artifact server counting hits per request, so the tests can
    /// prove the cache short-circuits the network.
    async fn artifact_server() -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = axum::Router::new().route(
            "/component.wasm",
            axum::routing::get({
                let hits = hits.clone();
                move || {
                    hits.fetch_add(1, Ordering::SeqCst);
                    async { ARTIFACT.to_vec() }
                }
            }),
        );
        tokio::spawn(axum::serve(listener, router).into_future());
        (format!("http://{addr}/component.wasm"), hits)
    }

    #[test]
    fn cache_is_keyed_by_digest() {
        let dir = Path::new("/cache");
        assert_eq!(
            cache_path(dir, "abc123"),
            PathBuf::from("/cache/abc123.wasm")
        );
        // Different digests → different immutable slots.
        assert_ne!(cache_path(dir, "a"), cache_path(dir, "b"));
    }

    #[tokio::test]
    async fn fetch_verifies_caches_and_never_refetches() {
        let (url, hits) = artifact_server().await;
        let scratch = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(scratch.path().join("components"));
        let sha = artifact_sha();

        let path = fetcher
            .resolve("app", &url, &sha)
            .await
            .expect("first fetch");
        assert_eq!(path, cache_path(&fetcher.cache_dir, &sha));
        assert_eq!(std::fs::read(&path).unwrap(), ARTIFACT);
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Same hash again — cache hit, no network. Also true for a brand-new
        // Fetcher over the same cache dir (a host restart).
        fetcher.resolve("app", &url, &sha).await.expect("cache hit");
        let restarted = Fetcher::new(fetcher.cache_dir.clone());
        restarted
            .resolve("app", &url, &sha)
            .await
            .expect("cache hit after restart");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "exactly one fetch ever");
    }

    #[tokio::test]
    async fn mismatched_digest_is_rejected_and_not_cached() {
        let (url, hits) = artifact_server().await;
        let scratch = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(scratch.path().join("components"));
        let wrong = "0".repeat(64);

        let err = fetcher
            .resolve("app", &url, &wrong)
            .await
            .expect_err("wrong digest must fail");
        assert!(err.contains("sha256 mismatch"), "{err}");
        assert!(
            err.contains(&artifact_sha()),
            "names the actual digest: {err}"
        );
        assert!(
            !cache_path(&fetcher.cache_dir, &wrong).exists(),
            "nothing unverified reaches the cache"
        );

        // The failure is remembered: the immediate retry reports the same
        // error without another fetch (the converge tick fires every 2 s).
        let again = fetcher.resolve("app", &url, &wrong).await.unwrap_err();
        assert!(again.contains("sha256 mismatch"), "{again}");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "backoff suppressed the refetch"
        );

        // The right digest still works on the same fetcher.
        fetcher
            .resolve("app", &url, &artifact_sha())
            .await
            .expect("correct digest");
    }

    /// A real wasm32-wasip2 component built by the CI pre-step (the same one
    /// the integration tests use). `None` when the wasm target wasn't built,
    /// so a plain `cargo test` without it still passes.
    fn built_component() -> Option<Vec<u8>> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("target/wasm32-wasip2/release/notes.wasm");
        std::fs::read(path).ok()
    }

    #[test]
    fn rejects_non_wasm_garbage() {
        let engine = wasmtime::Engine::default();
        // Empty, too-short, and wrong-magic blobs are rejected before any
        // wasmtime parse — the cheap content-addressed-store abuse guard.
        for garbage in [&b""[..], b"\0as", b"not a wasm binary at all"] {
            let err = validate_wasm_component(&engine, garbage).unwrap_err();
            assert!(err.contains("WebAssembly binary"), "{err}");
        }
        // Correct magic but not a valid component (a bare/corrupt body) is
        // rejected by the wasmtime parse, not the magic check.
        let mut fake = WASM_MAGIC.to_vec();
        fake.extend_from_slice(&[0x01, 0x00, 0x00, 0x00, 0xff, 0xff]);
        let err = validate_wasm_component(&engine, &fake).unwrap_err();
        assert!(err.contains("valid wasm component"), "{err}");
    }

    #[test]
    fn stores_a_real_component_and_computes_its_sha() {
        let Some(bytes) = built_component() else {
            eprintln!("SKIPPING stores_a_real_component: notes.wasm not built");
            return;
        };
        let engine = wasmtime::Engine::default();
        let scratch = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(scratch.path().join("components"));

        // The HOST computes the sha; the uploader never asserts it.
        let sha = fetcher.store_artifact(&engine, &bytes).expect("store");
        assert_eq!(sha, format!("{:x}", Sha256::digest(&bytes)));
        // It landed at the content address and is now servable by hash.
        let path = fetcher.artifact_path(&sha).expect("stored slot exists");
        assert_eq!(path, cache_path(&fetcher.cache_dir, &sha));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        // Storing the same bytes again is an idempotent dedup (no error).
        assert_eq!(fetcher.store_artifact(&engine, &bytes).unwrap(), sha);

        // Crucially: an UPLOADED artifact is immediately installable by the
        // SAME content-addressed resolve path the URL pipeline uses — a cache
        // hit, no network. (A bogus URL proves the cache short-circuited it.)
        let rt = tokio::runtime::Runtime::new().unwrap();
        let resolved = rt
            .block_on(fetcher.resolve("app", "http://127.0.0.1:1/never", &sha))
            .expect("resolve uses the stored artifact");
        assert_eq!(resolved, path);
    }

    #[test]
    fn rejects_garbage_upload() {
        let engine = wasmtime::Engine::default();
        let scratch = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(scratch.path().join("components"));
        let err = fetcher
            .store_artifact(&engine, b"definitely not wasm")
            .unwrap_err();
        assert!(err.contains("WebAssembly binary"), "{err}");
        // Nothing was written for a rejected upload.
        assert!(
            !scratch.path().join("components").exists() || {
                std::fs::read_dir(scratch.path().join("components"))
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(true)
            }
        );
    }

    #[tokio::test]
    async fn fetch_failure_is_a_clear_error() {
        let (url, _hits) = artifact_server().await;
        let scratch = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::new(scratch.path().join("components"));
        let err = fetcher
            .resolve(
                "app",
                &url.replace("component.wasm", "missing.wasm"),
                &"0".repeat(64),
            )
            .await
            .expect_err("404 must fail");
        assert!(err.contains("HTTP 404"), "{err}");
    }
}
