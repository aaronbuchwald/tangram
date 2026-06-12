//! Primitive B — the browser egress gate (`task-automation-browser.md` §5).
//!
//! Every request the browser context issues — top-level navigations,
//! redirects, XHR/fetch, sub-resources — is gated *before* it leaves the
//! box. The gate is the browser-session analog of the host's `http_fetch`
//! fence, enforced on **parsed** request components, never string suffixes.
//!
//! ## The single canonicalization seam
//!
//! The SOCKS5 parser-differential lesson (`attacker.com\x00.google.com`):
//! canonicalize once, match on parsed components. We do that here for the
//! browser gate.
//!
//! TODO: unify with `egress.rs` canonicalization seam when PR #1 merges. PR #1
//! (fine-grained egress) introduces THE shared canonicalizer for the whole
//! host so the browser fence and the component fence can never disagree on
//! what a host/URL means. That crate is not on `main` yet, so this module
//! implements a focused, self-contained host/path canonicalizer with the same
//! discipline; when PR #1 lands, delete `canonicalize_host`/`canonicalize_path`
//! here and consume PR #1's seam (the gate logic and tests stay).

use std::collections::BTreeSet;

/// The decision the gate returns for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Let the request continue (host on the allowlist, not denied).
    Allow,
    /// Abort the request (`route.abort('blockedbyclient')`); the reason is a
    /// short machine label for the security log, never request content.
    Deny(DenyReason),
}

/// Why a request was denied — logged as a security event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DenyReason {
    /// The host could not be parsed/canonicalized from the URL.
    Unparseable,
    /// The host is not in the session allowlist.
    NotAllowlisted,
    /// The host is explicitly on the denylist (denylist wins over allowlist).
    Denied,
    /// A call-level rule denied this method+path even on an allowed host
    /// (the network-layer backstop for stop-gated endpoints, §5.1 / §8 T4).
    PathDenied,
}

/// One call-level deny rule: on `host`, deny `method` requests to a path that
/// starts with `path_prefix` (canonicalized). This is the network-layer
/// backstop that makes "place order" undeniable even if the UI stop-gate is
/// somehow bypassed (§8 T4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathDeny {
    pub host: String,
    /// Upper-cased method, or `*` to match any method.
    pub method: String,
    /// Canonicalized path prefix (e.g. `/gp/buy/`).
    pub path_prefix: String,
}

/// The browser session's egress policy. Default-deny: an empty `allow` list
/// permits no navigation (today's `allow_hosts` posture — kept).
#[derive(Debug, Clone, Default)]
pub struct BrowserEgressGate {
    allow: BTreeSet<String>,
    deny: BTreeSet<String>,
    path_denies: Vec<PathDeny>,
}

impl BrowserEgressGate {
    /// Build a gate from an allowlist (and optional denylist). Hosts are
    /// canonicalized on the way in so the stored set matches canonicalized
    /// request hosts byte-for-byte.
    pub fn new(
        allow: impl IntoIterator<Item = impl Into<String>>,
        deny: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let allow = allow
            .into_iter()
            .filter_map(|h| canonicalize_host(&h.into()))
            .collect();
        let deny = deny
            .into_iter()
            .filter_map(|h| canonicalize_host(&h.into()))
            .collect();
        Self {
            allow,
            deny,
            path_denies: Vec::new(),
        }
    }

    /// Add a call-level deny rule (the order-submit backstop, §8 T4).
    pub fn deny_path(mut self, host: &str, method: &str, path_prefix: &str) -> Self {
        if let Some(host) = canonicalize_host(host) {
            self.path_denies.push(PathDeny {
                host,
                method: method.to_ascii_uppercase(),
                path_prefix: canonicalize_path(path_prefix),
            });
        }
        self
    }

    /// The canonicalized allowlist (for replay-gate comparison / logging).
    pub fn allowed_hosts(&self) -> Vec<String> {
        self.allow.iter().cloned().collect()
    }

    /// Decide on a request by its raw method + URL, exactly as the browser
    /// route hook sees it. Canonicalizes both, then:
    ///   1. unparseable host → Deny(Unparseable)  (fail closed)
    ///   2. host on denylist → Deny(Denied)       (denylist wins)
    ///   3. host not on allowlist → Deny(NotAllowlisted)
    ///   4. a call-level path-deny matches → Deny(PathDenied)
    ///   5. otherwise Allow
    pub fn decide(&self, method: &str, url: &str) -> Decision {
        let Some((host, path)) = canonicalize_url(url) else {
            return Decision::Deny(DenyReason::Unparseable);
        };
        if self.deny.contains(&host) {
            return Decision::Deny(DenyReason::Denied);
        }
        if !self.allow.contains(&host) {
            return Decision::Deny(DenyReason::NotAllowlisted);
        }
        let method = method.to_ascii_uppercase();
        for rule in &self.path_denies {
            if rule.host == host
                && (rule.method == "*" || rule.method == method)
                && path.starts_with(&rule.path_prefix)
            {
                return Decision::Deny(DenyReason::PathDenied);
            }
        }
        Decision::Allow
    }
}

/// Canonicalize a full URL to `(host, path)`. Returns `None` (fail closed)
/// when the host can't be cleanly determined. This is the seam the gate
/// matches on — never the raw string.
pub fn canonicalize_url(url: &str) -> Option<(String, String)> {
    // Reject control bytes outright — a NUL or newline in a URL is a
    // parser-differential lever (the SOCKS5 lesson), never legitimate.
    if url.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r') {
        return None;
    }
    // Split scheme://rest. We only gate http(s)/ws(s); anything else (data:,
    // blob:, about:, file:) is not a network egress to an allowlisted host
    // and must be decided by the caller, so we fail closed here.
    let rest = url
        .split_once("://")
        .map(|(_scheme, rest)| rest)
        .unwrap_or(url);
    // Authority ends at the first '/', '?' or '#'.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    // Strip userinfo (user:pass@host) — only the host past the LAST '@'
    // matters; a `@` is another parser-differential lever.
    let hostport = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    // Strip the port. IPv6 literals are bracketed: [::1]:8080.
    let host_raw = if let Some(stripped) = hostport.strip_prefix('[') {
        // [v6]:port — take up to the closing bracket.
        stripped.split(']').next().unwrap_or(stripped)
    } else {
        hostport.split(':').next().unwrap_or(hostport)
    };
    let host = canonicalize_host(host_raw)?;
    let path_raw = &rest[authority_end..];
    // Drop query/fragment for the path component.
    let path_only = path_raw.split(['?', '#']).next().unwrap_or(path_raw);
    let path = canonicalize_path(if path_only.is_empty() { "/" } else { path_only });
    Some((host, path))
}

/// Canonicalize a host: lowercased, trailing dot stripped, validated to a
/// conservative host grammar. Returns `None` for anything that doesn't look
/// like a clean DNS name or IP literal — fail closed. (IDNA/punycode
/// normalization is part of PR #1's shared seam; ASCII hosts are handled
/// here and non-ASCII fails closed for now, which is the safe direction.)
pub fn canonicalize_host(host: &str) -> Option<String> {
    if host.is_empty() {
        return None;
    }
    // Lowercase ASCII, strip a single trailing dot (the FQDN root: `a.com.`
    // and `a.com` are the same host — a classic suffix-match bypass).
    let lowered = host.trim_end_matches('.').to_ascii_lowercase();
    if lowered.is_empty() {
        return None;
    }
    // Conservative allowed byte set: DNS labels, IPv4 dots, and the IPv6
    // hex/colon set. Reject everything else (NUL handled earlier; this also
    // rejects '@', '/', whitespace, '%', non-ASCII — all bypass levers).
    let ok = lowered
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b':');
    if !ok {
        return None;
    }
    // No empty labels (a `..` or leading/trailing-before-trim dot is a
    // malformed host used to confuse suffix matchers).
    if lowered.split('.').any(|label| label.is_empty()) && !lowered.contains(':') {
        return None;
    }
    Some(lowered)
}

/// Canonicalize a path: percent-decode the unreserved set, normalize `.`/`..`
/// dot-segments, collapse to an absolute path. Matching path-prefixes on the
/// canonical form stops `/%2e%2e/` and `/a/../` style bypasses of a
/// call-level deny.
pub fn canonicalize_path(path: &str) -> String {
    let decoded = percent_decode(path);
    let mut out: Vec<&str> = Vec::new();
    for seg in decoded.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut joined = String::from("/");
    joined.push_str(&out.join("/"));
    // Preserve a trailing slash if the (decoded) input had one and we have
    // segments — prefix rules like `/gp/buy/` rely on it.
    if decoded.ends_with('/') && !out.is_empty() {
        joined.push('/');
    }
    joined
}

/// Percent-decode, but only %XX hex escapes; a malformed escape is left
/// literal (and the byte set in `canonicalize_host` would have rejected a
/// host carrying a stray '%'). Returns an owned String we then normalize.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate() -> BrowserEgressGate {
        BrowserEgressGate::new(["www.amazon.com", "fls-na.amazon.com"], ["evil.example"])
    }

    #[test]
    fn allows_listed_host() {
        assert_eq!(
            gate().decide("GET", "https://www.amazon.com/gp/cart/view.html"),
            Decision::Allow
        );
    }

    #[test]
    fn denies_offlist_host() {
        assert_eq!(
            gate().decide("GET", "https://attacker.com/"),
            Decision::Deny(DenyReason::NotAllowlisted)
        );
    }

    #[test]
    fn denylist_wins_over_allowlist() {
        let g = BrowserEgressGate::new(["evil.example"], ["evil.example"]);
        assert_eq!(
            g.decide("GET", "https://evil.example/"),
            Decision::Deny(DenyReason::Denied)
        );
    }

    // ── adversarial canonicalization (the SOCKS5 parser-differential class) ──

    #[test]
    fn mixed_case_host_canonicalizes() {
        assert_eq!(
            gate().decide("GET", "https://WWW.AmAzOn.CoM/x"),
            Decision::Allow
        );
    }

    #[test]
    fn trailing_dot_host_is_same_host() {
        // www.amazon.com. (FQDN root) must equal www.amazon.com — a classic
        // suffix-match bypass if compared as raw strings.
        assert_eq!(
            gate().decide("GET", "https://www.amazon.com./x"),
            Decision::Allow
        );
        // And a trailing-dot on an OFF-list host is still off-list.
        assert_eq!(
            gate().decide("GET", "https://attacker.com./x"),
            Decision::Deny(DenyReason::NotAllowlisted)
        );
    }

    #[test]
    fn null_byte_host_fails_closed() {
        // attacker.com\x00.www.amazon.com — the SOCKS5 differential. The NUL
        // makes the whole URL unparseable → fail closed.
        let url = "https://attacker.com\u{0}.www.amazon.com/x";
        assert_eq!(
            gate().decide("GET", url),
            Decision::Deny(DenyReason::Unparseable)
        );
    }

    #[test]
    fn userinfo_at_sign_does_not_smuggle_host() {
        // https://www.amazon.com@attacker.com/ — the REAL host is
        // attacker.com (after the last '@'); a naive prefix match sees
        // amazon. Must be denied.
        assert_eq!(
            gate().decide("GET", "https://www.amazon.com@attacker.com/"),
            Decision::Deny(DenyReason::NotAllowlisted)
        );
        // And the inverse: attacker as userinfo, amazon as host → allowed.
        assert_eq!(
            gate().decide("GET", "https://attacker.com@www.amazon.com/"),
            Decision::Allow
        );
    }

    #[test]
    fn embedded_slash_in_authority_truncates_to_host() {
        // https://www.amazon.com.attacker.com/ — a sibling domain, NOT a
        // subdomain match. Off-list.
        assert_eq!(
            gate().decide("GET", "https://www.amazon.com.attacker.com/"),
            Decision::Deny(DenyReason::NotAllowlisted)
        );
    }

    #[test]
    fn port_is_stripped_from_host() {
        assert_eq!(
            gate().decide("GET", "https://www.amazon.com:8443/x"),
            Decision::Allow
        );
    }

    #[test]
    fn non_http_scheme_fails_closed() {
        // data:/blob: have no allowlistable host → fail closed.
        assert_eq!(
            gate().decide("GET", "data:text/html,<h1>hi"),
            Decision::Deny(DenyReason::NotAllowlisted)
        );
    }

    // ── call-level path deny (the order-submit network backstop) ──

    #[test]
    fn path_deny_blocks_order_submit_even_on_allowed_host() {
        let g = gate().deny_path("www.amazon.com", "*", "/gp/buy/");
        // Cart view is fine…
        assert_eq!(
            g.decide("GET", "https://www.amazon.com/gp/cart/view.html"),
            Decision::Allow
        );
        // …but the checkout/place-order path is denied at the network layer.
        assert_eq!(
            g.decide(
                "POST",
                "https://www.amazon.com/gp/buy/spc/handlers/display.html"
            ),
            Decision::Deny(DenyReason::PathDenied)
        );
    }

    #[test]
    fn path_deny_resists_dot_segment_and_percent_bypass() {
        let g = gate().deny_path("www.amazon.com", "*", "/gp/buy/");
        // /gp/cart/../buy/  → normalizes to /gp/buy/ → still denied.
        assert_eq!(
            g.decide("POST", "https://www.amazon.com/gp/cart/../buy/now"),
            Decision::Deny(DenyReason::PathDenied)
        );
        // /gp/%62uy/ (%62 = 'b') → /gp/buy/ → denied.
        assert_eq!(
            g.decide("POST", "https://www.amazon.com/gp/%62uy/now"),
            Decision::Deny(DenyReason::PathDenied)
        );
    }

    #[test]
    fn default_deny_empty_allowlist() {
        let g = BrowserEgressGate::default();
        assert_eq!(
            g.decide("GET", "https://www.amazon.com/"),
            Decision::Deny(DenyReason::NotAllowlisted)
        );
    }

    #[test]
    fn canonicalize_host_rejects_levers() {
        assert_eq!(canonicalize_host("a..b.com"), None); // empty label
        assert_eq!(canonicalize_host("a/b.com"), None); // slash
        assert_eq!(canonicalize_host("a b.com"), None); // space
        assert_eq!(canonicalize_host(""), None);
        assert_eq!(
            canonicalize_host("WWW.Example.COM."),
            Some("www.example.com".into())
        );
    }
}
