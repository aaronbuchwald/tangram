//! Fine-grained egress: call-level capabilities (docs/design/fine-grained-egress.md).
//!
//! The unit of grant is a **declared call** — a method + host + path-pattern
//! (+ optional name-level query/header constraints and a constrained body
//! matcher) — with the egress credential injection attached *to that call*,
//! not to the host. This module holds the grammar ([`CallSpec`]) and the
//! single CANONICALIZATION SEAM ([`CanonicalRequest::from_request`]) that
//! every match runs against.
//!
//! ## The seam (the §2 SOCKS5 parser-differential lesson)
//!
//! Canonicalize the request ONCE, before any matching:
//!
//! - method upper-cased;
//! - URL parsed to `(host, path, query)`;
//! - host lowercased + trailing-dot stripped, rejected if it carries a null
//!   byte (the `attacker.com\0.good.com` class);
//! - path percent-decoded then dot-segment (`.` / `..`) normalized;
//! - query parameter NAMES collected (never values);
//! - header NAMES lowercased (never values).
//!
//! Matching is on the parsed/normalized components only — NEVER string-suffix
//! checks, NEVER regex, NEVER value-matching on query/header values. This is
//! the same seam the manifest verifier's call-grain arm
//! (manifest-verification-plan CP6) consumes, so the egress enforcer and the
//! verifier can never disagree on what a host/path means.

// The grammar and seam land in EC1 ahead of their consumers: the config
// parser (EC2), the `http_fetch` enforcer (EC3), the body rung (EC4), and the
// describe() channel (EC5) wire these in over the following checkpoints. Until
// then some constructors/fields read as dead from any single checkpoint's
// view; the allow is narrowed/removed as wiring lands.
#![allow(dead_code)]

use std::collections::BTreeSet;

use percent_encoding::percent_decode_str;

use crate::config::{InjectKind, InjectRule};

/// A request canonicalized at the single seam, ready to match against any
/// [`CallSpec`]. Built ONCE per `http-fetch` (the §2 parser-differential
/// lesson) and shared by the host fence and the call match so the two layers
/// can never disagree on what a host/path means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRequest {
    /// Upper-cased HTTP method (`GET`, `POST`, …).
    pub method: String,
    /// Lowercased host, trailing dot stripped, guaranteed null-byte-free and
    /// non-empty.
    pub host: String,
    /// Percent-decoded, dot-segment-normalized absolute path. Always starts
    /// with `/`; a trailing slash (other than the root) is stripped so
    /// `/v1/x` and `/v1/x/` canonicalize identically.
    pub path: String,
    /// Non-empty path segments (the split of [`Self::path`]), precomputed for
    /// template matching.
    pub segments: Vec<String>,
    /// Query parameter NAMES present on the URL (never values).
    pub query_names: BTreeSet<String>,
    /// Header NAMES present on the request, lowercased (never values).
    pub header_names: BTreeSet<String>,
}

impl CanonicalRequest {
    /// Canonicalize a parsed request into the seam value. `method` is the raw
    /// method string (defaulting `GET` upstream), `url` the already-parsed
    /// [`reqwest::Url`] (which is `url::Url`), and `header_names` the raw
    /// header names the component supplied. Returns an error string (the same
    /// `Err(String)` channel `http_fetch` already uses) when the host is
    /// unusable — empty or carrying a null byte.
    pub fn from_request<'a>(
        method: &str,
        url: &reqwest::Url,
        header_names: impl IntoIterator<Item = &'a str>,
    ) -> Result<Self, String> {
        let host = url
            .host_str()
            .ok_or_else(|| format!("url {url:?} has no host"))?;
        let host = canonical_host(host)?;
        let path = canonical_path(url.path());
        let segments: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        // `url::Url::query_pairs()` already percent-decodes each key, so a
        // `%71` (=`q`) is seen as the decoded NAME and cannot smuggle past a
        // name-level constraint.
        let query_names: BTreeSet<String> =
            url.query_pairs().map(|(k, _)| k.into_owned()).collect();
        let header_names: BTreeSet<String> = header_names
            .into_iter()
            .map(|h| h.trim().to_ascii_lowercase())
            .filter(|h| !h.is_empty())
            .collect();
        Ok(Self {
            method: method.trim().to_ascii_uppercase(),
            host,
            path,
            segments,
            query_names,
            header_names,
        })
    }
}

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

/// Whether every element of `sub` appears in `sup` (set containment over the
/// small constraint-name lists). Used by [`CallSpec::covers`].
fn is_subset(sub: &[String], sup: &[String]) -> bool {
    sub.iter().all(|x| sup.contains(x))
}

/// Intersect the operator's effective calls with the component's `describe()`-
/// DECLARED calls (fine-grained-egress §6). The declaration is a REQUEST, never
/// authority: the result keeps only operator calls that some declared call
/// COVERS, so a component declaring MORE than its spec is narrowed to the spec,
/// and declaring FEWER narrows the spec. An empty `declared` list (component
/// declares nothing) leaves the operator spec UNCHANGED — declaring nothing is
/// not "declare zero calls", it is "no declaration to intersect with".
pub fn intersect_with_declared(operator: Vec<CallSpec>, declared: &[CallSpec]) -> Vec<CallSpec> {
    if declared.is_empty() {
        return operator;
    }
    operator
        .into_iter()
        .filter(|op| declared.iter().any(|dec| dec.covers(op)))
        .collect()
}

/// Generalize a canonical path's segments into a template string for the
/// observe-mode generator (fine-grained-egress §5.2 / EC6): a segment that
/// looks like an opaque identifier — all-numeric, or a UUID — becomes `{id}`
/// so the generated `[[calls]]` declares the SHAPE of the call rather than one
/// specific resource. Other segments are kept literal. Re-parsing the result
/// with [`PathPattern::parse`] yields a template that matches the observed
/// call (the round-trip EC6 asserts).
pub fn templatize_path(segments: &[String]) -> String {
    if segments.is_empty() {
        return "/".to_string();
    }
    let parts: Vec<String> = segments
        .iter()
        .map(|seg| {
            if is_identifier_segment(seg) {
                "{id}".to_string()
            } else {
                seg.clone()
            }
        })
        .collect();
    format!("/{}", parts.join("/"))
}

/// The observe-mode `[[calls]]` generator (fine-grained-egress §5.2 / EC6).
/// Turns the bare `http-fetch`es an app actually made — captured as canonical
/// requests in observe mode — into a paste-ready `[[calls]]` block: one entry
/// per distinct (method, host, templatized-path), sorted and de-duplicated.
/// Numeric/uuid path segments are parameterized to `{id}` ([`templatize_path`])
/// so the declaration generalizes. Re-parsing the result accepts exactly the
/// observed calls (the round-trip EC6 asserts).
///
/// Header/query NAMES seen on the request are emitted as `required` constraints
/// only when present, so the generated declaration is no broader than observed.
pub fn generate_calls_block(observed: &[CanonicalRequest]) -> String {
    // Distinct (method, host, templated-path), each carrying the union of the
    // query/header NAMES seen across the observations that map to it.
    type CallKey = (String, String, String);
    type SeenNames = (BTreeSet<String>, BTreeSet<String>);
    let mut entries: std::collections::BTreeMap<CallKey, SeenNames> =
        std::collections::BTreeMap::new();
    for req in observed {
        let path = templatize_path(&req.segments);
        let key = (req.method.clone(), req.host.clone(), path);
        let slot = entries.entry(key).or_default();
        slot.0.extend(req.query_names.iter().cloned());
        slot.1.extend(req.header_names.iter().cloned());
    }
    let mut out = String::new();
    for ((method, host, path), (queries, headers)) in entries {
        out.push_str("[[calls]]\n");
        out.push_str(&format!("method = {method:?}\n"));
        out.push_str(&format!("host   = {host:?}\n"));
        out.push_str(&format!("path   = {path:?}\n"));
        if !queries.is_empty() {
            let names: Vec<String> = queries.iter().map(|q| format!("{q:?}")).collect();
            out.push_str(&format!(
                "query  = {{ required = [{}] }}\n",
                names.join(", ")
            ));
        }
        if !headers.is_empty() {
            let names: Vec<String> = headers.iter().map(|h| format!("{h:?}")).collect();
            out.push_str(&format!(
                "headers = {{ required = [{}] }}\n",
                names.join(", ")
            ));
        }
        out.push('\n');
    }
    out
}

/// The in-Rust authoring helper (fine-grained-egress §5.2): build a declared
/// call next to the code that makes the fetch, so the fetch and its capability
/// can't drift. `Call::get("api.x.com", "/v1/items/{id}").query_required(["q"])`
/// builds the canonical [`CallSpec`] (no inject — the credential is the
/// operator's), and [`Call::to_toml`] renders the paste-ready `[[calls]]`
/// entry. A component carries these out via `describe()` (EC5), where the host
/// intersects them with the operator spec.
#[derive(Debug, Clone)]
pub struct Call {
    method: MethodMatch,
    host: String,
    path: PathPattern,
    query: QueryConstraint,
    headers: HeaderConstraint,
    body: Option<BodyMatch>,
    max_body_bytes: Option<usize>,
}

impl Call {
    fn new(method: MethodMatch, host: &str, path: &str) -> Self {
        Self {
            method,
            host: host.trim().trim_end_matches('.').to_ascii_lowercase(),
            path: PathPattern::parse(path).unwrap_or(PathPattern::Subtree),
            query: QueryConstraint::default(),
            headers: HeaderConstraint::default(),
            body: None,
            max_body_bytes: None,
        }
    }

    pub fn get(host: &str, path: &str) -> Self {
        Self::new(MethodMatch::Exact("GET".into()), host, path)
    }
    pub fn post(host: &str, path: &str) -> Self {
        Self::new(MethodMatch::Exact("POST".into()), host, path)
    }
    pub fn method(method: &str, host: &str, path: &str) -> Self {
        let m = match method.trim() {
            "*" | "" => MethodMatch::Any,
            other => MethodMatch::Exact(other.to_ascii_uppercase()),
        };
        Self::new(m, host, path)
    }

    pub fn query_required<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.query
            .required
            .extend(names.into_iter().map(Into::into));
        self
    }
    pub fn query_forbidden<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.query
            .forbidden
            .extend(names.into_iter().map(Into::into));
        self
    }
    pub fn header_required<I, S>(mut self, names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.headers
            .required
            .extend(names.into_iter().map(|n| n.into().to_ascii_lowercase()));
        self
    }
    pub fn json_method<I, S>(mut self, methods: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.body = Some(BodyMatch {
            pointer: "/method".into(),
            allowed: methods.into_iter().map(Into::into).collect(),
        });
        self
    }
    pub fn max_body_bytes(mut self, max: usize) -> Self {
        self.max_body_bytes = Some(max);
        self
    }

    /// The canonical [`CallSpec`] this authoring call denotes (no inject).
    pub fn to_spec(&self) -> CallSpec {
        CallSpec {
            method: self.method.clone(),
            host: self.host.clone(),
            path: self.path.clone(),
            query: self.query.clone(),
            headers: self.headers.clone(),
            max_body_bytes: self.max_body_bytes,
            body: self.body.clone(),
            inject: None,
            inject_kind: None,
        }
    }

    /// Render this call as a paste-ready `[[calls]]` TOML entry.
    pub fn to_toml(&self) -> String {
        let mut out = String::from("[[calls]]\n");
        out.push_str(&format!("method = {:?}\n", self.method_str()));
        out.push_str(&format!("host   = {:?}\n", self.host));
        out.push_str(&format!("path   = {:?}\n", self.path.render()));
        if !self.query.required.is_empty() || !self.query.forbidden.is_empty() {
            out.push_str("query  = {");
            if !self.query.required.is_empty() {
                let r: Vec<String> = self
                    .query
                    .required
                    .iter()
                    .map(|q| format!("{q:?}"))
                    .collect();
                out.push_str(&format!(" required = [{}]", r.join(", ")));
            }
            out.push_str(" }\n");
        }
        out
    }

    fn method_str(&self) -> &str {
        match &self.method {
            MethodMatch::Exact(m) => m,
            MethodMatch::Any => "*",
        }
    }
}

/// Whether a path segment looks like an opaque identifier (parameterized to
/// `{id}` by the generator): all-digits, or a canonical 8-4-4-4-12 UUID.
fn is_identifier_segment(seg: &str) -> bool {
    if !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    is_uuid(seg)
}

fn is_uuid(seg: &str) -> bool {
    let groups = [8, 4, 4, 4, 12];
    let parts: Vec<&str> = seg.split('-').collect();
    parts.len() == groups.len()
        && parts
            .iter()
            .zip(groups)
            .all(|(p, n)| p.len() == n && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// How a [`CallSpec`] constrains the method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MethodMatch {
    /// Exactly this (upper-cased) method.
    Exact(String),
    /// Any method — the maximally-broad implicit grant (the legacy compat
    /// shim; discouraged in authored specs).
    Any,
}

impl MethodMatch {
    fn matches(&self, method: &str) -> bool {
        match self {
            Self::Exact(m) => m == method,
            Self::Any => true,
        }
    }
}

/// One segment of a path template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    /// A literal segment that must match exactly.
    Literal(String),
    /// A named placeholder (`{id}`) matching exactly one non-`/` segment.
    Param,
}

/// How a [`CallSpec`] constrains the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathPattern {
    /// An exact (canonical) path.
    Exact(String),
    /// An RFC-6570-style template: literal + `{name}` segments, each matching
    /// exactly one path segment, and the segment count must match.
    Template(Vec<Seg>),
    /// `**` subtree match — any path (the maximally-broad implicit grant).
    Subtree,
}

impl PathPattern {
    /// Parse a path-pattern string. `**` (or `/**`) → [`Self::Subtree`]; a
    /// path containing `{name}` segments → [`Self::Template`]; otherwise an
    /// exact (canonicalized) path. An explicit trailing `/**` also yields a
    /// subtree (a templated prefix subtree is NOT supported in v1 — kept
    /// small, the §3(b) lesson).
    pub fn parse(pattern: &str) -> Result<Self, String> {
        let trimmed = pattern.trim();
        if trimmed == "**" || trimmed == "/**" {
            return Ok(Self::Subtree);
        }
        if trimmed.contains("**") {
            return Err(format!(
                "path pattern {pattern:?}: `**` is only allowed as the whole pattern \
                 (`/**`, the subtree wildcard); embedded `**` is not supported"
            ));
        }
        let canon = canonical_path(trimmed);
        if !canon.contains('{') && !canon.contains('}') {
            return Ok(Self::Exact(canon));
        }
        let mut segs = Vec::new();
        for seg in canon.split('/').filter(|s| !s.is_empty()) {
            if let Some(inner) = seg.strip_prefix('{').and_then(|s| s.strip_suffix('}')) {
                if inner.is_empty() || inner.contains('{') || inner.contains('}') {
                    return Err(format!(
                        "path template {pattern:?}: a `{{name}}` segment must name a \
                         non-empty placeholder"
                    ));
                }
                segs.push(Seg::Param);
            } else if seg.contains('{') || seg.contains('}') {
                return Err(format!(
                    "path template {pattern:?}: a `{{` / `}}` must delimit a WHOLE \
                     segment (`/items/{{id}}`), not part of one"
                ));
            } else {
                segs.push(Seg::Literal(seg.to_string()));
            }
        }
        Ok(Self::Template(segs))
    }

    /// Render the pattern back to its declared form (for diagnostics): an
    /// exact path verbatim, a template with `{}` placeholders, `/**` subtree.
    pub fn render(&self) -> String {
        match self {
            Self::Subtree => "/**".to_string(),
            Self::Exact(p) => p.clone(),
            Self::Template(segs) => {
                let parts: Vec<&str> = segs
                    .iter()
                    .map(|s| match s {
                        Seg::Literal(lit) => lit.as_str(),
                        Seg::Param => "{}",
                    })
                    .collect();
                format!("/{}", parts.join("/"))
            }
        }
    }

    fn matches(&self, req: &CanonicalRequest) -> bool {
        match self {
            Self::Subtree => true,
            Self::Exact(p) => *p == req.path,
            Self::Template(segs) => {
                segs.len() == req.segments.len()
                    && segs.iter().zip(&req.segments).all(|(seg, got)| match seg {
                        Seg::Literal(lit) => lit == got,
                        Seg::Param => true,
                    })
            }
        }
    }
}

/// Name-level query constraint (never matches on values — values may carry
/// data and matching on them invites the parser-differential class).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryConstraint {
    /// These parameter names MUST be present.
    pub required: Vec<String>,
    /// These parameter names MUST be absent.
    pub forbidden: Vec<String>,
}

impl QueryConstraint {
    fn matches(&self, req: &CanonicalRequest) -> bool {
        self.required.iter().all(|r| req.query_names.contains(r))
            && self.forbidden.iter().all(|f| !req.query_names.contains(f))
    }
}

/// Name-level header constraint. Required is a list of header names that must
/// be present (NAMES only — v1 deliberately does not match on header VALUES,
/// per the §4.1 grammar: values may carry data). Forbidden names must be
/// absent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderConstraint {
    /// These header names (lowercased) MUST be present.
    pub required: Vec<String>,
    /// These header names (lowercased) MUST be absent.
    pub forbidden: Vec<String>,
}

impl HeaderConstraint {
    fn matches(&self, req: &CanonicalRequest) -> bool {
        self.required.iter().all(|r| req.header_names.contains(r))
            && self.forbidden.iter().all(|f| !req.header_names.contains(f))
    }
}

/// The constrained JSON-RPC-method body rung (§9.1): a FIXED JSON-pointer
/// selector plus equality/membership against a literal set. Nothing more — no
/// operators, no regex, no value-matching on arbitrary fields. The body is
/// parsed ONLY when a call declares this matcher (and only up to
/// `max_body_bytes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyMatch {
    /// The JSON pointer to select (e.g. `/method`).
    pub pointer: String,
    /// The literal set the selected value must be a member of (string
    /// equality). Empty set never matches (a no-op declaration is rejected at
    /// validation).
    pub allowed: Vec<String>,
}

/// Why a body match failed — distinguishes "the body is malformed/oversized"
/// (which is a deny regardless of the value) from "the value is not allowed".
#[derive(Debug, PartialEq, Eq)]
pub enum BodyVerdict {
    /// The selected value is in the allowed set.
    Match,
    /// The selected value is present but not allowed.
    NotAllowed,
    /// The body was missing, oversized, non-JSON, or the pointer is absent.
    Unusable(&'static str),
}

impl BodyMatch {
    /// Evaluate the matcher against the raw request body bytes. `max_body`, if
    /// set, caps the bytes parsed: an oversized body is `Unusable` BEFORE any
    /// parse (the adversarial requirement). The value at the pointer must be a
    /// JSON string in the allowed set.
    pub fn evaluate(&self, body: &[u8], max_body: Option<usize>) -> BodyVerdict {
        if let Some(max) = max_body
            && body.len() > max
        {
            return BodyVerdict::Unusable("body exceeds max_body_bytes (rejected before parse)");
        }
        let value: serde_json::Value = match serde_json::from_slice(body) {
            Ok(v) => v,
            Err(_) => return BodyVerdict::Unusable("body is not valid JSON"),
        };
        let selected = match value.pointer(&self.pointer) {
            Some(v) => v,
            None => return BodyVerdict::Unusable("body has no value at the declared json pointer"),
        };
        match selected.as_str() {
            Some(s) if self.allowed.iter().any(|a| a == s) => BodyVerdict::Match,
            _ => BodyVerdict::NotAllowed,
        }
    }
}

/// A declared call: the call-level capability. The host picks the FIRST
/// matching call (declaration order is precedence, like a firewall rule list)
/// and injects ONLY that call's credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSpec {
    pub method: MethodMatch,
    /// Lowercased host; MUST also be in the app's `allow_hosts`.
    pub host: String,
    pub path: PathPattern,
    pub query: QueryConstraint,
    pub headers: HeaderConstraint,
    /// Optional body-byte cap. `Some(0)` forbids a body entirely. Also bounds
    /// the body parsed for [`BodyMatch`].
    pub max_body_bytes: Option<usize>,
    /// The constrained JSON-RPC-method body rung (§9.1); body is parsed only
    /// when this is `Some`.
    pub body: Option<BodyMatch>,
    /// The credential injection scoped to THIS call (not the host). A call
    /// with no inject goes out un-credentialed (still allowed).
    pub inject: Option<InjectRule>,
    /// The resolved injection kind, classified once at construction (mirrors
    /// `resolved_inject`'s lift of `InjectRule::kind`). `None` ⇔ `inject`
    /// is `None`.
    pub inject_kind: Option<InjectKind>,
}

impl CallSpec {
    /// Whether this call matches the canonicalized request, given the request
    /// body length (for the `max_body_bytes` / `Some(0)` checks) and the raw
    /// body bytes (for the optional [`BodyMatch`]). Host equality is the
    /// caller's responsibility for the cheap pre-filter, but is re-checked
    /// here so a single call list can mix hosts safely.
    ///
    /// Returns `true` iff every declared constraint matches. The body matcher,
    /// when present, must return [`BodyVerdict::Match`]; an
    /// oversized/malformed/missing-pointer body is NOT a match (deny).
    pub fn matches(&self, req: &CanonicalRequest, body: &[u8]) -> bool {
        if self.host != req.host {
            return false;
        }
        if !self.method.matches(&req.method) {
            return false;
        }
        if !self.path.matches(req) {
            return false;
        }
        if !self.query.matches(req) {
            return false;
        }
        if !self.headers.matches(req) {
            return false;
        }
        // max_body_bytes caps the body length (Some(0) forbids a body). This
        // gate runs BEFORE the body matcher's parse.
        if let Some(max) = self.max_body_bytes
            && body.len() > max
        {
            return false;
        }
        if let Some(body_match) = &self.body {
            return body_match.evaluate(body, self.max_body_bytes) == BodyVerdict::Match;
        }
        true
    }

    /// Whether `self` (a component-DECLARED upper bound) COVERS `operator` (a
    /// call from the operator spec) — fine-grained-egress §6: the component's
    /// declared calls are a request that can only NARROW the operator grant,
    /// never widen it. `self` covers `operator` when it permits at least every
    /// request `operator` does:
    ///
    /// - same canonical host;
    /// - method: `self` is `Any`, or both name the same exact method;
    /// - path: `self` is `Subtree`, or the patterns are structurally equal;
    /// - constraints: `self` imposes no MORE than `operator` (a declared call
    ///   that adds its own required/forbidden names or a body matcher is
    ///   narrower, so it does not cover a less-constrained operator call).
    ///
    /// This is the same regex-free structural containment manifest CP6 will
    /// consume; kept deliberately small (the §3(b) lesson).
    pub fn covers(&self, operator: &CallSpec) -> bool {
        if self.host != operator.host {
            return false;
        }
        let method_ok = match (&self.method, &operator.method) {
            (MethodMatch::Any, _) => true,
            (MethodMatch::Exact(a), MethodMatch::Exact(b)) => a == b,
            (MethodMatch::Exact(_), MethodMatch::Any) => false,
        };
        if !method_ok {
            return false;
        }
        let path_ok = match (&self.path, &operator.path) {
            (PathPattern::Subtree, _) => true,
            (a, b) => a == b,
        };
        if !path_ok {
            return false;
        }
        // `self` must not impose constraints `operator` lacks (that would make
        // `self` narrower than `operator`, so it cannot cover it). Subset on
        // the constraint NAME sets, plus: a declared body matcher only covers
        // an operator call that declares the same matcher.
        is_subset(&self.query.required, &operator.query.required)
            && is_subset(&self.query.forbidden, &operator.query.forbidden)
            && is_subset(&self.headers.required, &operator.headers.required)
            && is_subset(&self.headers.forbidden, &operator.headers.forbidden)
            && self.body == operator.body
    }

    /// The method rendered for diagnostics (`*` for `Any`).
    pub fn method_str(&self) -> &str {
        match &self.method {
            MethodMatch::Exact(m) => m,
            MethodMatch::Any => "*",
        }
    }

    /// The path pattern rendered for diagnostics (the operator-facing denial
    /// message and the candidate-call generator).
    pub fn path_str(&self) -> String {
        self.path.render()
    }

    /// The maximally-broad implicit call for the legacy compat shim (§4.2/§7):
    /// `{ method = Any, host, path = Subtree }`, optionally carrying the
    /// host-keyed inject rule moved onto it. This is what a bare `allow_hosts`
    /// host (or a host-keyed `[apps.X.inject]`) desugars to, so existing
    /// configs behave byte-identically.
    pub fn implicit_subtree(host: &str, inject: Option<(InjectKind, InjectRule)>) -> Self {
        let (inject_kind, inject) = match inject {
            Some((k, r)) => (Some(k), Some(r)),
            None => (None, None),
        };
        Self {
            method: MethodMatch::Any,
            host: host.to_ascii_lowercase(),
            path: PathPattern::Subtree,
            query: QueryConstraint::default(),
            headers: HeaderConstraint::default(),
            max_body_bytes: None,
            body: None,
            inject,
            inject_kind,
        }
    }
}

// The test module is named `callspec` so the build plan's documented filter
// `cargo test -p tangram-host callspec` selects exactly the EC1 suite.
#[cfg(test)]
mod callspec {
    use super::*;

    fn req(method: &str, url: &str) -> CanonicalRequest {
        let parsed = reqwest::Url::parse(url).expect("parse url");
        CanonicalRequest::from_request(method, &parsed, std::iter::empty()).expect("canonicalize")
    }

    fn req_headers(method: &str, url: &str, headers: &[&str]) -> CanonicalRequest {
        let parsed = reqwest::Url::parse(url).expect("parse url");
        CanonicalRequest::from_request(method, &parsed, headers.iter().copied())
            .expect("canonicalize")
    }

    // ── The single canonicalization seam: adversarial coverage (the §2 SOCKS5
    //    parser-differential lesson; the make-or-break checkpoint). ──────────

    #[test]
    fn host_is_lowercased() {
        assert_eq!(
            req("GET", "https://API.Vendor.COM/x").host,
            "api.vendor.com"
        );
    }

    #[test]
    fn host_trailing_dot_is_stripped() {
        // The fully-qualified `good.com.` form must canonicalize identically
        // to `good.com` so it cannot dodge an exact host match.
        assert_eq!(req("GET", "https://good.com./x").host, "good.com");
        // And combined with case.
        assert_eq!(req("GET", "https://Good.Com./x").host, "good.com");
    }

    #[test]
    fn null_byte_host_is_refused() {
        // `attacker.com\0.good.com` — the SOCKS5 null-byte differential. The
        // canonicalizer REFUSES rather than letting an ambiguous host reach a
        // comparison. url::Url percent-encodes a literal NUL in the authority,
        // so feed canonical_host directly (the seam's host gate).
        let err = canonical_host("attacker.com\0.good.com").unwrap_err();
        assert!(err.contains("null byte"), "{err}");
        // Also via a percent-encoded NUL that decodes in a host position is
        // not representable through url::Url's authority, so the direct gate
        // is the authoritative check and is exercised here.
    }

    #[test]
    fn empty_host_is_refused() {
        assert!(canonical_host("  ").is_err());
        assert!(canonical_host(".").is_err());
    }

    #[test]
    fn method_is_uppercased() {
        assert_eq!(req("get", "https://h.com/x").method, "GET");
        assert_eq!(req("Post", "https://h.com/x").method, "POST");
    }

    #[test]
    fn percent_encoded_dot_path_is_decoded_then_normalized() {
        // `%2e` is `.`; `%2e%2e` is `..`. Both must decode BEFORE dot-segment
        // normalization, so an encoded traversal cannot dodge a path match.
        assert_eq!(canonical_path("/v1/%2e/nutrition"), "/v1/nutrition");
        assert_eq!(canonical_path("/v1/a/%2e%2e/nutrition"), "/v1/nutrition");
        // Mixed case percent-encoding.
        assert_eq!(canonical_path("/v1/%2E/x"), "/v1/x");
    }

    #[test]
    fn dot_segments_are_normalized() {
        assert_eq!(canonical_path("/v1/./x"), "/v1/x");
        assert_eq!(canonical_path("/v1/a/../x"), "/v1/x");
        // `..` can never escape above root.
        assert_eq!(canonical_path("/../../x"), "/x");
        assert_eq!(canonical_path("/.."), "/");
        // empty segments collapse; trailing slash stripped.
        assert_eq!(canonical_path("/v1//x/"), "/v1/x");
        assert_eq!(canonical_path("/"), "/");
        assert_eq!(canonical_path(""), "/");
    }

    #[test]
    fn encoded_path_traversal_through_a_real_url_is_normalized() {
        // Through the FULL seam (url::Url then canonical_path): an attacker
        // encoding `..` as `%2e%2e` to reach `/v1/accounts/9/import` from a
        // declared `/v1/me/contacts` host must not slip past.
        let r = req(
            "GET",
            "https://api.vendor.com/v1/me/%2e%2e/accounts/9/import",
        );
        assert_eq!(r.path, "/v1/accounts/9/import");
        assert_eq!(r.segments, ["v1", "accounts", "9", "import"]);
    }

    #[test]
    fn duplicate_query_keys_collapse_to_one_name() {
        // Duplicate keys must not let a forbidden/required NAME check be
        // fooled — we match on the SET of names, never values or counts.
        let r = req("GET", "https://h.com/x?q=a&q=b&callback=evil");
        assert!(r.query_names.contains("q"));
        assert!(r.query_names.contains("callback"));
        assert_eq!(r.query_names.len(), 2);
    }

    #[test]
    fn percent_encoded_query_name_is_decoded() {
        // `%71` is `q` — a name-level constraint must see the decoded name so
        // an encoded key cannot smuggle past a `forbidden` list.
        let r = req("GET", "https://h.com/x?%71=v");
        assert!(r.query_names.contains("q"), "{:?}", r.query_names);
    }

    #[test]
    fn header_names_are_lowercased_values_ignored() {
        let r = req_headers(
            "POST",
            "https://h.com/x",
            &["Content-Type", "X-Custom", "  "],
        );
        assert!(r.header_names.contains("content-type"));
        assert!(r.header_names.contains("x-custom"));
        assert_eq!(r.header_names.len(), 2, "blank header dropped");
    }

    // ── Matching on the canonicalized components. ────────────────────────────

    fn call(method: MethodMatch, host: &str, path: PathPattern) -> CallSpec {
        CallSpec {
            method,
            host: host.to_string(),
            path,
            query: QueryConstraint::default(),
            headers: HeaderConstraint::default(),
            max_body_bytes: None,
            body: None,
            inject: None,
            inject_kind: None,
        }
    }

    #[test]
    fn exact_method_host_path_matches() {
        let c = call(
            MethodMatch::Exact("GET".into()),
            "api.vendor.com",
            PathPattern::Exact("/v1/me/contacts".into()),
        );
        assert!(c.matches(&req("GET", "https://api.vendor.com/v1/me/contacts"), b""));
        // mixed-case host + trailing dot still matches (canonicalized).
        assert!(c.matches(&req("get", "https://API.Vendor.com./v1/me/contacts"), b""));
        // wrong method, wrong path, wrong host all miss.
        assert!(!c.matches(&req("POST", "https://api.vendor.com/v1/me/contacts"), b""));
        assert!(!c.matches(&req("GET", "https://api.vendor.com/v1/me/other"), b""));
        assert!(!c.matches(&req("GET", "https://evil.com/v1/me/contacts"), b""));
    }

    #[test]
    fn template_matches_one_segment_each() {
        let c = call(
            MethodMatch::Exact("GET".into()),
            "h.com",
            PathPattern::parse("/v1/items/{id}").unwrap(),
        );
        assert!(c.matches(&req("GET", "https://h.com/v1/items/42"), b""));
        assert!(c.matches(&req("GET", "https://h.com/v1/items/abc-uuid"), b""));
        // too few / too many segments miss (a template segment is exactly one)
        assert!(!c.matches(&req("GET", "https://h.com/v1/items"), b""));
        assert!(!c.matches(&req("GET", "https://h.com/v1/items/42/extra"), b""));
    }

    #[test]
    fn subtree_matches_any_path() {
        let c = call(MethodMatch::Any, "h.com", PathPattern::Subtree);
        assert!(c.matches(&req("DELETE", "https://h.com/anything/at/all"), b""));
        assert!(c.matches(&req("GET", "https://h.com/"), b""));
        assert!(!c.matches(&req("GET", "https://other.com/x"), b""));
    }

    #[test]
    fn path_pattern_parse_rejects_embedded_wildcard_and_partial_braces() {
        assert!(PathPattern::parse("/v1/**/x").is_err());
        assert!(PathPattern::parse("/v1/item{id}").is_err());
        assert!(PathPattern::parse("/v1/{}").is_err());
        assert_eq!(PathPattern::parse("/**").unwrap(), PathPattern::Subtree);
        assert_eq!(PathPattern::parse("**").unwrap(), PathPattern::Subtree);
    }

    #[test]
    fn query_required_and_forbidden_names() {
        let mut c = call(
            MethodMatch::Exact("GET".into()),
            "h.com",
            PathPattern::Exact("/x".into()),
        );
        c.query = QueryConstraint {
            required: vec!["query".into()],
            forbidden: vec!["callback".into()],
        };
        assert!(c.matches(&req("GET", "https://h.com/x?query=chicken"), b""));
        // missing required name
        assert!(!c.matches(&req("GET", "https://h.com/x?other=1"), b""));
        // forbidden name present
        assert!(!c.matches(&req("GET", "https://h.com/x?query=a&callback=evil"), b""));
    }

    #[test]
    fn header_required_and_forbidden_names() {
        let mut c = call(
            MethodMatch::Exact("POST".into()),
            "h.com",
            PathPattern::Exact("/x".into()),
        );
        c.headers = HeaderConstraint {
            required: vec!["content-type".into()],
            forbidden: vec!["x-evil".into()],
        };
        assert!(c.matches(
            &req_headers("POST", "https://h.com/x", &["Content-Type"]),
            b""
        ));
        assert!(!c.matches(&req_headers("POST", "https://h.com/x", &[]), b""));
        assert!(!c.matches(
            &req_headers("POST", "https://h.com/x", &["Content-Type", "X-Evil"]),
            b""
        ));
    }

    #[test]
    fn max_body_bytes_zero_forbids_a_body() {
        let mut c = call(
            MethodMatch::Exact("GET".into()),
            "h.com",
            PathPattern::Exact("/x".into()),
        );
        c.max_body_bytes = Some(0);
        assert!(c.matches(&req("GET", "https://h.com/x"), b""));
        assert!(!c.matches(&req("GET", "https://h.com/x"), b"some body"));
    }

    // ── The JSON-RPC-method body rung (§9.1). ────────────────────────────────

    #[test]
    fn body_match_json_method_membership() {
        let bm = BodyMatch {
            pointer: "/method".into(),
            allowed: vec!["tools/list".into(), "tools/call".into()],
        };
        assert_eq!(
            bm.evaluate(br#"{"method":"tools/list"}"#, None),
            BodyVerdict::Match
        );
        assert_eq!(
            bm.evaluate(br#"{"method":"resources/read"}"#, None),
            BodyVerdict::NotAllowed
        );
    }

    #[test]
    fn body_match_rejects_non_json_missing_pointer_and_oversize() {
        let bm = BodyMatch {
            pointer: "/method".into(),
            allowed: vec!["tools/list".into()],
        };
        // non-JSON
        assert_eq!(
            bm.evaluate(b"not json at all", None),
            BodyVerdict::Unusable("body is not valid JSON")
        );
        // missing pointer
        assert_eq!(
            bm.evaluate(br#"{"other":"x"}"#, None),
            BodyVerdict::Unusable("body has no value at the declared json pointer")
        );
        // oversized — rejected BEFORE parse (even though it IS valid json).
        assert_eq!(
            bm.evaluate(br#"{"method":"tools/list"}"#, Some(4)),
            BodyVerdict::Unusable("body exceeds max_body_bytes (rejected before parse)")
        );
    }

    #[test]
    fn body_match_through_callspec_allows_only_declared_methods() {
        let mut c = call(
            MethodMatch::Exact("POST".into()),
            "api.vendor.com",
            PathPattern::Exact("/rpc".into()),
        );
        c.body = Some(BodyMatch {
            pointer: "/method".into(),
            allowed: vec!["tools/list".into(), "tools/call".into()],
        });
        c.max_body_bytes = Some(64 * 1024);
        let r = req("POST", "https://api.vendor.com/rpc");
        assert!(c.matches(&r, br#"{"method":"tools/call","params":{}}"#));
        assert!(!c.matches(&r, br#"{"method":"resources/read"}"#));
        // non-JSON body → deny (no match), never a parse panic.
        assert!(!c.matches(&r, b"garbage"));
    }

    // ── EC6: observe-mode generator + the Call authoring helper. The nested
    //    module name carries `observe_generate` so the build plan's filter
    //    `cargo test -p tangram-host observe_generate` selects it. ────────────
    mod observe_generate {
        use super::*;
        use crate::config::HostConfig;

        /// The hosts the generated block grants must be in allow_hosts (the
        /// fence composes); wrap the generated `[[calls]]` in a minimal app so
        /// `HostConfig::parse` accepts and lowers it the real way.
        fn reparse(block: &str, hosts: &[&str]) -> Vec<CallSpec> {
            let allow = hosts
                .iter()
                .map(|h| format!("{h:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            // The generator emits bare `[[calls]]` (what an operator pastes
            // UNDER an `[apps.<app>]` table); to parse it standalone here we
            // namespace it to the app table.
            let block = block.replace("[[calls]]", "[[apps.gen.calls]]");
            let toml = format!(
                "[apps.gen]\ncomponent = \"g.wasm\"\nui = \"ui\"\n\
                 allow_hosts = [{allow}]\nenforcement = \"enforce\"\n\n{block}"
            );
            HostConfig::parse(&toml)
                .expect("generated block must parse")
                .apps["gen"]
                .resolved_calls()
        }

        #[test]
        fn generated_block_round_trips_to_the_observed_calls() {
            // Observe three fetches: a templatable id path, a query, a repeat.
            let observed = vec![
                req("GET", "https://api.x.com/v1/items/42?q=a"),
                req("GET", "https://api.x.com/v1/items/99?q=b"), // same shape → one entry
                req("POST", "https://api.x.com/v1/orders"),
            ];
            let block = generate_calls_block(&observed);
            // Numeric segment was parameterized to {id}.
            assert!(block.contains("/v1/items/{id}"), "block:\n{block}");
            assert!(block.contains(r#"method = "GET""#));
            assert!(block.contains(r#"method = "POST""#));
            assert!(block.contains(r#"required = ["q"]"#));

            // Re-parse: exactly two distinct calls (the two GETs collapsed).
            let calls = reparse(&block, &["api.x.com"]);
            assert_eq!(
                calls.len(),
                2,
                "the two same-shape GETs collapse to one entry"
            );

            // Every observed request matches some re-parsed call (round-trip:
            // re-parsing accepts exactly the observed calls).
            for obs in &observed {
                assert!(
                    calls.iter().any(|c| c.matches(obs, b"")),
                    "observed {} {}{} not accepted by the generated block:\n{block}",
                    obs.method,
                    obs.host,
                    obs.path
                );
            }
            // And an UNobserved call (different path) is NOT accepted.
            let other = req("DELETE", "https://api.x.com/v1/items/1");
            assert!(!calls.iter().any(|c| c.matches(&other, b"")));
        }

        #[test]
        fn uuid_segments_are_parameterized() {
            let observed = vec![req(
                "GET",
                "https://api.x.com/v1/users/550e8400-e29b-41d4-a716-446655440000/profile",
            )];
            let block = generate_calls_block(&observed);
            assert!(block.contains("/v1/users/{id}/profile"), "block:\n{block}");
            let calls = reparse(&block, &["api.x.com"]);
            // A DIFFERENT uuid in the same shape is still accepted (generalized).
            let other = req(
                "GET",
                "https://api.x.com/v1/users/00000000-0000-0000-0000-000000000001/profile",
            );
            assert!(calls.iter().any(|c| c.matches(&other, b"")));
        }

        #[test]
        fn call_authoring_helper_builds_the_same_spec() {
            // The Call helper (§5.2): write the capability next to the fetch.
            let spec = Call::get("API.X.com", "/v1/nutrition")
                .query_required(["query"])
                .to_spec();
            assert_eq!(spec.host, "api.x.com", "host canonicalized");
            assert_eq!(spec.method, MethodMatch::Exact("GET".into()));
            assert_eq!(spec.query.required, vec!["query".to_string()]);
            assert!(
                spec.inject.is_none(),
                "authoring declares reach, never a credential"
            );
            // It matches the fetch it describes.
            let r = req("GET", "https://api.x.com/v1/nutrition?query=chicken");
            assert!(spec.matches(&r, b""));
            // The rendered TOML re-parses to an equivalent spec.
            let calls = reparse(
                &Call::get("api.x.com", "/v1/nutrition").to_toml(),
                &["api.x.com"],
            );
            assert_eq!(calls.len(), 1);
            assert!(calls[0].matches(&r, b""));
        }

        #[test]
        fn json_method_authoring_helper() {
            let spec = Call::post("api.x.com", "/rpc")
                .json_method(["tools/list", "tools/call"])
                .to_spec();
            let r = req("POST", "https://api.x.com/rpc");
            assert!(spec.matches(&r, br#"{"method":"tools/list"}"#));
            assert!(!spec.matches(&r, br#"{"method":"resources/read"}"#));
        }
    }

    #[test]
    fn implicit_subtree_is_the_broad_compat_call() {
        let c = CallSpec::implicit_subtree("API.Vendor.com", None);
        assert_eq!(c.method, MethodMatch::Any);
        assert_eq!(c.path, PathPattern::Subtree);
        assert_eq!(c.host, "api.vendor.com");
        assert!(c.inject.is_none());
        // matches literally anything on that host.
        assert!(c.matches(&req("PATCH", "https://api.vendor.com/a/b/c?z=1"), b"body"));
    }
}
