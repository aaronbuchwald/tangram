//! The single egress canonicalization seam (the §2 SOCKS5 parser-differential
//! lesson, docs/design/fine-grained-egress.md / ADR-0008).
//!
//! Every egress fence in the fleet canonicalizes a host/path through THESE
//! functions — the host `http_fetch` enforcer (`tangram-host/src/egress.rs`),
//! the manifest verifier's call-grain arm, and the browser egress gate
//! (`tangram-automation/src/egress.rs`). Canonicalize ONCE, match on the
//! parsed/normalized components only — never string-suffix checks, never regex.
//! Sharing one canonicalizer is what makes it impossible for two fences to
//! disagree on what a host/path means.
//!
//! This is a pure leaf crate (only `percent-encoding`) so both the
//! Wasmtime-linking host and the native browser-automation substrate can depend
//! on it without pulling in each other.

use percent_encoding::percent_decode_str;

/// Canonicalize a host: lowercase, strip a single trailing dot (the
/// fully-qualified `good.com.` form), and REJECT a host that is empty or
/// carries an embedded null byte (`attacker.com\0.good.com` — the SOCKS5
/// parser-differential class). The reject is deliberate: a host the
/// canonicalizer cannot make unambiguous must never reach a suffix/segment
/// comparison.
pub fn canonical_host(host: &str) -> Result<String, String> {
    if host.contains('\0') {
        return Err(format!(
            "outbound host {host:?} contains a null byte — refused (ambiguous host, \
             the parser-differential class)"
        ));
    }
    let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        return Err("outbound request has an empty host".to_string());
    }
    Ok(host)
}

/// Canonicalize a path: percent-decode, normalize `.`/`..`/empty segments,
/// re-join with a single leading `/`, and strip a trailing slash (except the
/// root). `..` can never escape above `/` (extra `..` are dropped at the
/// root). This is purely lexical — it does not consult the filesystem.
pub fn canonical_path(raw: &str) -> String {
    let decoded = percent_decode_str(raw).decode_utf8_lossy().into_owned();
    let mut out: Vec<&str> = Vec::new();
    for seg in decoded.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    if out.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", out.join("/"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_lowercases_and_strips_trailing_dot() {
        assert_eq!(
            canonical_host("WWW.Example.COM.").unwrap(),
            "www.example.com"
        );
    }

    #[test]
    fn host_rejects_null_byte_and_empty() {
        assert!(canonical_host("attacker.com\u{0}.good.com").is_err());
        assert!(canonical_host("").is_err());
        assert!(canonical_host(".").is_err());
    }

    #[test]
    fn path_normalizes_dot_segments_and_percent() {
        assert_eq!(canonical_path("/gp/cart/../buy/now"), "/gp/buy/now");
        assert_eq!(canonical_path("/gp/%62uy/now"), "/gp/buy/now");
        assert_eq!(canonical_path("/v1/x/"), "/v1/x");
        assert_eq!(canonical_path("/"), "/");
        assert_eq!(canonical_path("/a/../../b"), "/b");
    }
}
