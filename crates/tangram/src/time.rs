//! Wall-clock time with one signature on every target. Natively this is
//! `SystemTime`; inside a WASM component it is the `tangram:app/host.now-ms`
//! import, keeping the component's world self-contained (time is granted by
//! the host, not ambient).

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> i64 {
    #[cfg(not(target_family = "wasm"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
    #[cfg(target_family = "wasm")]
    {
        crate::guest::wit::tangram::app::host::now_ms()
    }
}
