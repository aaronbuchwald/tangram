//! Mechanical capability-manifest verification (design:
//! `docs/design/manifest-verification-plan.md`).
//!
//! The host enforces a three-link subset chain at every install/converge path:
//!
//! ```text
//! granted (spec)  ⊆  declared (manifest)  ⊆  audited (component's real imports)
//! ```
//!
//! - **audited** — what the component's bytes can even *name*: its actual WIT
//!   imports, read programmatically from the compiled component
//!   ([`AuditedImports`]). This is the ground truth.
//! - **declared** — the manifest the app says it needs
//!   ([`DeclaredCapabilities`]); for first-party apps the spec IS the
//!   declaration.
//! - **granted** — what the operator's EFFECTIVE spec hands the running
//!   component ([`GrantedCapabilities`]), after the tenant ceiling
//!   intersection.
//!
//! Two violations, two dispositions (plan §1, §2.3):
//!
//! 1. `granted ⊄ declared` → converge ERROR (hard fail). The app does not run.
//! 2. `declared ⊄ audited` → FLAGGED (`verified: false`). The app still runs
//!    (the grant gates reach) but is surfaced as unverified.
//!
//! ALL wasmtime component-model introspection is isolated behind
//! [`AuditedImports::from_component`] so a future wasmtime API change is a
//! one-file edit (plan §3.1). The wasmparser-as-a-library fallback (plan §2.2)
//! is the documented alternative if that API churns.

use std::collections::BTreeSet;

use wasmtime::Engine;
use wasmtime::component::Component;
use wasmtime::component::types::ComponentItem;

use crate::config::{DeclaredManifest, NetworkClaim};

/// The capability interface the host implements for components
/// (`crates/tangram-host/wit/tangram.wit`). Its `http-fetch` *function* — not
/// the interface itself — is the network-reach predicate (plan §1.1).
const HOST_INTERFACE: &str = "tangram:app/host";
/// The one host function that grants outbound network reach.
const HTTP_FETCH: &str = "http-fetch";

/// The set of import-interface PREFIXES the closed world permits: the host
/// capability interface and the inert wasip2 std plumbing (env, clocks,
/// random, stdio — data, not reach; see `runtime.rs`). Anything else
/// (`wasi:sockets`, `wasi:http`, …) breaks the closed world and lands in
/// [`AuditedImports::foreign`].
fn is_known_safe_interface(id: &str) -> bool {
    id == HOST_INTERFACE
        || id.starts_with("wasi:cli/")
        || id.starts_with("wasi:io/")
        || id.starts_with("wasi:clocks/")
        || id.starts_with("wasi:filesystem/")
        || id.starts_with("wasi:random/")
}

/// The audited ground truth: what a compiled component's bytes can actually
/// name, reduced to the small capability-relevant value the subset chain
/// consumes (plan §2.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditedImports {
    /// Interface ids the component imports (e.g. `tangram:app/host`).
    pub interfaces: BTreeSet<String>,
    /// Functions imported from `tangram:app/host` — the capability surface.
    /// A subset of `{http-fetch, log, now-ms}`.
    pub host_funcs: BTreeSet<String>,
    /// Any import OUTSIDE the known-safe set (wasi std + `tangram:app/host`) —
    /// e.g. `wasi:sockets`, `wasi:http`. Non-empty ⇒ the closed world is
    /// broken.
    pub foreign: BTreeSet<String>,
}

impl AuditedImports {
    /// Read the function-level import graph of an already-compiled component.
    ///
    /// This drills into the imported `tangram:app/host` INSTANCE and records
    /// which of its functions the component imports — the granularity §1.1
    /// requires (a world-level dump makes notes and nutrition look identical;
    /// the difference is whether `http-fetch` specifically is imported).
    ///
    /// `Component::component_type() -> types::Component`;
    /// `types::Component::imports(&Engine) -> (&str, ComponentItem)`;
    /// drilling `ComponentItem::ComponentInstance(inst).exports(&Engine)`
    /// enumerates the imported instance's functions (the functions the
    /// component imports FROM that instance). Confirmed against wasmtime
    /// 45.0.1.
    pub fn from_component(engine: &Engine, component: &Component) -> Self {
        let mut audit = Self::default();
        let ct = component.component_type();
        for (import_name, item) in ct.imports(engine) {
            audit.interfaces.insert(import_name.to_string());
            if !is_known_safe_interface(import_name) {
                audit.foreign.insert(import_name.to_string());
            }
            if import_name == HOST_INTERFACE
                && let ComponentItem::ComponentInstance(inst) = item
            {
                // Exports of an *imported* instance = the functions the
                // component imports from it.
                for (func_name, _func_item) in inst.exports(engine) {
                    audit.host_funcs.insert(func_name.to_string());
                }
            }
        }
        audit
    }

    /// The network-reach predicate: does the component import the one host
    /// function that can make an outbound request? If false the component
    /// cannot make ANY request and every `allow_hosts`/`inject` grant is
    /// vacuous (plan §2.1).
    pub fn imports_http_fetch(&self) -> bool {
        self.host_funcs.contains(HTTP_FETCH)
    }

    /// The Phase-0 closed-world invariant re-expressed programmatically: no
    /// sockets/fs-beyond-wasi/inbound-http imports (plan §2.1).
    pub fn closed_world_ok(&self) -> bool {
        self.foreign.is_empty()
    }
}

/// A fine-grained-egress call claim (plan §2.6, fine-grained-egress §4.1) —
/// the (host, method, path-template) grain a grant/declaration narrows to. This
/// is the manifest WIRE shape (the simple `{host, method, path}` triple a
/// `CapabilityManifest`/`[apps.<app>.declared]` carries); the structural
/// containment it drives is delegated to THE single egress seam
/// ([`crate::egress::CallSpec::covers`]) so the verifier and the egress enforcer
/// can never disagree on what a host/path means (the SOCKS5 parser-differential
/// lesson — one canonicalizer, one containment relation). Call-level egress
/// itself ships (ADR-0008): the egress *enforcer* matches against declared
/// `[[apps.<app>.calls]]` at the `http_fetch` boundary. This verifier arm is
/// GATED on the *converge* path specifically — registry/`apps.toml` grants are
/// still host-grained there, so `granted.calls` is empty and this arm is a
/// no-op until call-grain grants reach converge.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CallSpec {
    /// Exact host name (lowercased canonical form — shares the egress
    /// canonicalization seam, plan §2.6).
    pub host: String,
    /// HTTP method (uppercased), or `*` for any.
    pub method: String,
    /// Path template (e.g. `/v1/me/contacts`), or a prefix the granted call
    /// must fall under. No regex (fine-grained-egress §4.1).
    pub path: String,
}

impl CallSpec {
    /// Lower this manifest wire claim into the single runtime egress seam
    /// ([`crate::egress::CallSpec`]) via the SAME config lowering the egress
    /// enforcer uses ([`crate::config::CallSpecToml::resolve`]) — so host
    /// canonicalization and path templating are done in exactly one place. A
    /// malformed claim (bad host/path) lowers to `None`; the caller treats an
    /// un-lowerable claim as covering nothing (fail closed).
    fn to_egress(&self) -> Option<crate::egress::CallSpec> {
        crate::config::CallSpecToml {
            method: self.method.clone(),
            host: self.host.clone(),
            path: self.path.clone(),
            query: crate::config::NameConstraintToml::default(),
            headers: crate::config::NameConstraintToml::default(),
            max_body_bytes: None,
            body: None,
            inject: None,
        }
        .resolve()
        .ok()
    }

    /// `self ⊇ other`: does this (declared) call cover that (granted) call?
    /// Delegates to the single egress containment relation
    /// ([`crate::egress::CallSpec::covers`]) after lowering both sides through
    /// the shared canonicalization seam (plan §2.6, the SOCKS5
    /// parser-differential lesson — the verifier never re-implements host/path
    /// matching). A claim that cannot be lowered covers nothing (fail closed).
    pub fn contains(&self, other: &CallSpec) -> bool {
        match (self.to_egress(), other.to_egress()) {
            (Some(declared), Some(granted)) => declared.covers(&granted),
            _ => false,
        }
    }
}

/// The DECLARED capabilities the verifier consumes — the middle link, derived
/// from a [`DeclaredManifest`] or from the granted spec (plan §2.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredCapabilities {
    pub network: DeclaredNetwork,
    pub env_keys: BTreeSet<String>,
}

/// The declared network shape, host-side (the verifier's internal value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclaredNetwork {
    /// No outbound network declared — the component must import no http-fetch.
    None,
    /// A set of exact (lowercased) outbound hosts.
    Hosts(BTreeSet<String>),
    /// Fine-grained call claims (gated on fine-grained-egress; plan §2.6).
    Calls(Vec<CallSpec>),
}

impl DeclaredNetwork {
    /// The declared host set, for the host-grain subset check. `None` ⇒ ∅;
    /// `Calls` ⇒ the set of hosts named by the call claims (so host-grain
    /// grants still subset-check against a call-grain declaration).
    pub fn hosts(&self) -> BTreeSet<String> {
        match self {
            Self::None => BTreeSet::new(),
            Self::Hosts(hosts) => hosts.clone(),
            Self::Calls(calls) => calls.iter().map(|c| c.host.to_ascii_lowercase()).collect(),
        }
    }

    /// Does the declaration claim ANY outbound network? Drives the
    /// "declares network but can't reach it" flag (plan §2.4).
    pub fn claims_network(&self) -> bool {
        !matches!(self, Self::None)
    }
}

impl DeclaredCapabilities {
    /// Build from an explicit manifest. The manifest's `network` is used as
    /// declared; `env_keys` falls back to the granted env keys when the
    /// manifest omits them (a manifest can declare network without re-listing
    /// env). `allow_hosts` is passed for the omitted-env fallback only.
    pub fn from_manifest(manifest: &DeclaredManifest, _allow_hosts: &[String]) -> Self {
        let network = match &manifest.network {
            NetworkClaim::None => DeclaredNetwork::None,
            NetworkClaim::Hosts { hosts } => {
                DeclaredNetwork::Hosts(hosts.iter().map(|h| h.to_ascii_lowercase()).collect())
            }
            NetworkClaim::Calls { calls } => DeclaredNetwork::Calls(calls.clone()),
        };
        let env_keys = manifest
            .env_keys
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();
        Self { network, env_keys }
    }

    /// Derive the declaration from the granted spec (back-compat, plan §2.4):
    /// an un-annotated app declares exactly what it was granted, so
    /// `granted ⊆ declared` holds by construction and it verifies trivially.
    pub fn derived_from_grant(
        allow_hosts: &[String],
        env_keys: impl Iterator<Item = String>,
    ) -> Self {
        let hosts: BTreeSet<String> = allow_hosts.iter().map(|h| h.to_ascii_lowercase()).collect();
        let network = if hosts.is_empty() {
            DeclaredNetwork::None
        } else {
            DeclaredNetwork::Hosts(hosts)
        };
        Self {
            network,
            env_keys: env_keys.collect(),
        }
    }
}

/// The GRANTED capabilities the operator's effective spec hands the component
/// (plan §2.4) — POST-ceiling (plan §3.1). Hosts are lowercased canonical.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GrantedCapabilities {
    pub allow_hosts: BTreeSet<String>,
    pub inject_hosts: BTreeSet<String>,
    pub env_keys: BTreeSet<String>,
    /// Call-grained grants (DESIGNED-FOR; plan §2.6). Call-level egress ships
    /// (ADR-0008) — the egress *enforcer* matches declared calls at the
    /// `http_fetch` boundary — but this field stays EMPTY on the *converge*
    /// path: registry/`apps.toml` grants are still host-grained there. When the
    /// converge path begins emitting call-grain grants, the enforcer's
    /// effective `CallSpec`s populate this and the chain's call-grain
    /// containment arm (in [`verify`]) becomes load-bearing. Kept here so the
    /// seam is the SAME canonicalization the enforcer uses (the SOCKS5 lesson).
    pub calls: Vec<CallSpec>,
}

/// The verification verdict stamped on a running app and mirrored into the
/// fleet (plan §2.5). `Verified` ⇒ every chain link held; `Unverified`
/// carries the human-readable discrepancies (the SOFT-flag case where the app
/// still runs). The HARD-fail case never produces a verdict — it is an
/// `Err(String)` from the chain that the converge path records as the app's
/// error (and the app does not run), reusing the sha-256 mismatch channel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Verification {
    #[default]
    Verified,
    Unverified {
        reasons: Vec<String>,
    },
}

impl Verification {
    pub fn is_verified(&self) -> bool {
        matches!(self, Self::Verified)
    }

    pub fn reasons(&self) -> &[String] {
        match self {
            Self::Verified => &[],
            Self::Unverified { reasons } => reasons,
        }
    }
}

/// Run the verification chain `granted ⊆ declared ⊆ audited` (plan §1, §2.4).
///
/// Returns:
/// - `Err(msg)` for a HARD FAIL (`granted ⊄ declared`, or a grant of outbound
///   reach to a component that imports no http-fetch). The caller records it
///   as the app's converge error and the app does NOT run.
/// - `Ok(Verification::Unverified { reasons })` for a SOFT FLAG
///   (`declared ⊄ audited` — the manifest under-claims the real imports, or a
///   broken closed world). The app DOES run, stamped unverified.
/// - `Ok(Verification::Verified)` when every link holds.
pub fn verify(
    granted: &GrantedCapabilities,
    declared: &DeclaredCapabilities,
    audited: &AuditedImports,
) -> Result<Verification, String> {
    // ── HARD link: granted ⊆ declared ───────────────────────────────────────
    let declared_hosts = declared.network.hosts();
    for host in &granted.allow_hosts {
        if !declared_hosts.contains(host) {
            return Err(format!(
                "manifest verification failed: spec grants outbound host {host:?} which the \
                 manifest does not declare (granted ⊄ declared)"
            ));
        }
    }
    // inject targets already must be in allow_hosts (validate_inject), so this
    // composes with the check above; assert it explicitly for clarity.
    for host in &granted.inject_hosts {
        if !declared_hosts.contains(host) {
            return Err(format!(
                "manifest verification failed: spec injects a credential for outbound host \
                 {host:?} which the manifest does not declare (granted ⊄ declared)"
            ));
        }
    }
    for key in &granted.env_keys {
        if !declared.env_keys.contains(key) {
            return Err(format!(
                "manifest verification failed: spec grants env key {key:?} which the manifest \
                 does not declare (granted ⊄ declared)"
            ));
        }
    }

    // ── HARD link, call grain: every granted CallSpec must be matched by some
    //    declared CallSpec (`granted ⊆ declared` at the (host, method, path)
    //    grain — plan §2.6, CP6). DESIGNED-FOR but GATED: `granted.calls` is
    //    EMPTY in the live converge path (grants are host-grained today), so
    //    this loop is a no-op until fine-grained-egress populates it. The
    //    `#[ignore]`d `call_grain_subset` test activates it. The audit link is
    //    UNCHANGED by calls — the component still only imports `http-fetch`
    //    (no WIT change); call grain lives entirely in granted ⊆ declared.
    if let DeclaredNetwork::Calls(declared_calls) = &declared.network {
        for call in &granted.calls {
            if !declared_calls.iter().any(|d| d.contains(call)) {
                return Err(format!(
                    "manifest verification failed: spec grants call {} {}{} which no declared \
                     call covers (granted ⊄ declared, call grain)",
                    call.method, call.host, call.path
                ));
            }
        }
    }

    // ── HARD link: a grant of outbound reach to a component that imports no
    //    http-fetch is vacuous — refuse it (plan §2.4). This is the CP3 case
    //    and the trivial-pass converse: a no-network component verifies for
    //    zero hosts automatically.
    let grants_reach = !granted.allow_hosts.is_empty() || !granted.inject_hosts.is_empty();
    if grants_reach && !audited.imports_http_fetch() {
        return Err(format!(
            "manifest verification failed: spec grants outbound reach (hosts {:?}) to a \
             component that imports no http-fetch — the grant is vacuous, refusing",
            granted.allow_hosts
        ));
    }

    // ── SOFT link: declared ⊆ audited. A manifest that under-claims the real
    //    imports is a lie about the app's surface — surface it, but the grant
    //    still gates reach so the app may run (plan §1, §2.3).
    let mut reasons = Vec::new();
    if declared.network.claims_network() && !audited.imports_http_fetch() {
        reasons.push(
            "manifest declares outbound network but the component imports no http-fetch \
             (declared ⊄ audited — a stale or wrong manifest)"
                .to_string(),
        );
    }
    if !declared.network.claims_network() && audited.imports_http_fetch() {
        reasons.push(
            "component imports http-fetch but the manifest declares no network \
             (declared ⊄ audited — the manifest under-claims the app's surface)"
                .to_string(),
        );
    }
    if !audited.closed_world_ok() {
        reasons.push(format!(
            "component imports outside the closed world: {:?} (no wasi:sockets/wasi:http/\
             fs-beyond-wasi is allowed)",
            audited.foreign
        ));
    }

    if reasons.is_empty() {
        Ok(Verification::Verified)
    } else {
        Ok(Verification::Unverified { reasons })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Compile a built wasm32-wasip2 component for introspection. The audit
    /// only needs a typed `Component`; it never instantiates.
    fn audit(name: &str) -> Option<AuditedImports> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join(format!("target/wasm32-wasip2/release/{name}.wasm"));
        if !path.exists() {
            return None;
        }
        let engine = crate::runtime::engine().expect("engine");
        let component = Component::from_file(&engine, &path).expect("compile component");
        Some(AuditedImports::from_component(&engine, &component))
    }

    fn audited_with_fetch() -> AuditedImports {
        AuditedImports {
            interfaces: [HOST_INTERFACE.to_string()].into_iter().collect(),
            host_funcs: ["http-fetch", "log", "now-ms"]
                .iter()
                .map(ToString::to_string)
                .collect(),
            foreign: BTreeSet::new(),
        }
    }

    fn audited_no_fetch() -> AuditedImports {
        AuditedImports {
            interfaces: [HOST_INTERFACE.to_string()].into_iter().collect(),
            host_funcs: ["log", "now-ms"].iter().map(ToString::to_string).collect(),
            foreign: BTreeSet::new(),
        }
    }

    fn granted(hosts: &[&str]) -> GrantedCapabilities {
        GrantedCapabilities {
            allow_hosts: hosts.iter().map(|h| h.to_ascii_lowercase()).collect(),
            ..Default::default()
        }
    }

    fn declared_hosts(hosts: &[&str]) -> DeclaredCapabilities {
        DeclaredCapabilities {
            network: DeclaredNetwork::Hosts(hosts.iter().map(|h| h.to_ascii_lowercase()).collect()),
            env_keys: BTreeSet::new(),
        }
    }

    /// CP2 (unit) — over-granting a host the manifest does not declare is a
    /// HARD FAIL whose message names the offending host.
    #[test]
    fn over_grant_is_hard_fail() {
        let err = verify(
            &granted(&["api.evil.com"]),
            &declared_hosts(&["api.good.com"]),
            &audited_with_fetch(),
        )
        .unwrap_err();
        assert!(err.contains("api.evil.com"), "{err}");
        assert!(err.contains("granted ⊄ declared"), "{err}");
    }

    /// CP3 (unit) — granting any host to a component that imports no
    /// http-fetch is a HARD FAIL (vacuous grant); the no-network trivial pass
    /// verifies true.
    #[test]
    fn vacuous_grant_is_hard_fail_and_no_network_passes() {
        // Even when the manifest "declares" the host, granting reach to a
        // component that cannot reach is refused.
        let err = verify(
            &granted(&["api.good.com"]),
            &declared_hosts(&["api.good.com"]),
            &audited_no_fetch(),
        )
        .unwrap_err();
        assert!(err.contains("no http-fetch"), "{err}");
        // A no-network component with no grant verifies trivially.
        let verdict = verify(
            &granted(&[]),
            &DeclaredCapabilities {
                network: DeclaredNetwork::None,
                env_keys: BTreeSet::new(),
            },
            &audited_no_fetch(),
        )
        .unwrap();
        assert!(verdict.is_verified());
    }

    /// CP4 (unit) — a manifest under-claiming the real imports (declares no
    /// network while the component imports http-fetch) is a SOFT FLAG: the
    /// chain returns Unverified, not Err.
    #[test]
    fn under_claim_is_soft_flag() {
        let verdict = verify(
            &granted(&[]),
            &DeclaredCapabilities {
                network: DeclaredNetwork::None,
                env_keys: BTreeSet::new(),
            },
            &audited_with_fetch(),
        )
        .unwrap();
        assert!(!verdict.is_verified());
        assert!(
            verdict.reasons().iter().any(|r| r.contains("http-fetch")),
            "{:?}",
            verdict.reasons()
        );
    }

    /// An honest grant (granted == declared, component imports http-fetch)
    /// verifies; a broken closed world is flagged.
    #[test]
    fn honest_grant_verifies_and_foreign_import_is_flagged() {
        assert!(
            verify(
                &granted(&["api.good.com"]),
                &declared_hosts(&["api.good.com"]),
                &audited_with_fetch(),
            )
            .unwrap()
            .is_verified()
        );
        let mut broken = audited_with_fetch();
        broken.foreign.insert("wasi:sockets/tcp".into());
        let verdict = verify(
            &granted(&["api.good.com"]),
            &declared_hosts(&["api.good.com"]),
            &broken,
        )
        .unwrap();
        assert!(!verdict.is_verified());
        assert!(
            verdict.reasons().iter().any(|r| r.contains("wasi:sockets")),
            "{:?}",
            verdict.reasons()
        );
    }

    /// CP6 (DEFERRED, designed-for) — the `CallSpec ⊆ CallSpec` containment
    /// arm exists and is correct, and now routes through the single egress seam
    /// ([`crate::egress::CallSpec::covers`]), but `NetworkClaim::Calls` is not
    /// produced or consumed anywhere in the live converge path: converge grants
    /// are host-grained today (the egress *enforcer* already matches call-level
    /// grants at runtime; ADR-0008). This stub activates (drop the `#[ignore]`)
    /// when the host begins emitting call-grain grants/declarations at converge
    /// (plan §2.6, CP6). Until then it is the explicit deferred marker.
    #[test]
    #[ignore = "gated on call-grain grants reaching the converge path (plan \
                §2.6 / CP6); the egress enforcer already matches call-level \
                grants at runtime"]
    fn call_grain_subset() {
        let declared_call = CallSpec {
            host: "api.vendor.com".into(),
            method: "GET".into(),
            path: "/v1/me/contacts".into(),
        };
        // Direct containment relation: same call contained (mixed-case host,
        // case-insensitive method — the shared canonicalization seam, plan
        // §2.6); a broader method/path is not.
        assert!(declared_call.contains(&CallSpec {
            host: "API.Vendor.com".into(),
            method: "get".into(),
            path: "/v1/me/contacts".into(),
        }));
        assert!(!declared_call.contains(&CallSpec {
            host: "api.vendor.com".into(),
            method: "POST".into(),
            path: "/v1/accounts/42/import".into(),
        }));

        // Through the CHAIN: a declaration of `Calls([GET /v1/me/contacts])`.
        let declared = DeclaredCapabilities {
            network: DeclaredNetwork::Calls(vec![declared_call.clone()]),
            env_keys: BTreeSet::new(),
        };
        // A granted GET /v1/me/contacts is covered → verifies.
        let mut granted_ok = GrantedCapabilities {
            allow_hosts: ["api.vendor.com".into()].into_iter().collect(),
            ..Default::default()
        };
        granted_ok.calls = vec![CallSpec {
            host: "api.vendor.com".into(),
            method: "GET".into(),
            path: "/v1/me/contacts".into(),
        }];
        assert!(
            verify(&granted_ok, &declared, &audited_with_fetch())
                .unwrap()
                .is_verified()
        );
        // A granted POST .../import is covered by no declared call → HARD FAIL.
        let mut granted_bad = granted_ok.clone();
        granted_bad.calls = vec![CallSpec {
            host: "api.vendor.com".into(),
            method: "POST".into(),
            path: "/v1/accounts/42/import".into(),
        }];
        let err = verify(&granted_bad, &declared, &audited_with_fetch()).unwrap_err();
        assert!(err.contains("call grain"), "{err}");
    }

    /// CP1 — the host dumps a component's FUNCTION-LEVEL imports, and notes ≠
    /// nutrition. This is the canary for the granularity trap (plan §1.1):
    /// a verifier reading only the world dump cannot make notes≠nutrition pass.
    #[test]
    fn audit_reader_distinguishes_notes_from_nutrition() {
        let (Some(notes), Some(nutrition)) = (audit("notes"), audit("nutrition")) else {
            eprintln!(
                "SKIPPING audit_reader: wasm components missing — build them first \
                 (cargo build -p tangram-notes -p tangram-nutrition --lib \
                 --target wasm32-wasip2 --release)"
            );
            return;
        };

        // The load-bearing distinction: notes makes no outbound request,
        // nutrition calls CalorieNinjas. Both import the `host` interface;
        // only nutrition imports its `http-fetch` function.
        assert!(
            !notes.imports_http_fetch(),
            "notes must NOT import http-fetch (host_funcs: {:?})",
            notes.host_funcs
        );
        assert!(
            nutrition.imports_http_fetch(),
            "nutrition MUST import http-fetch (host_funcs: {:?})",
            nutrition.host_funcs
        );

        // notes still imports the other host functions (log, now-ms) — the
        // host interface is imported, just not its network function.
        assert!(
            notes.host_funcs.contains("log") && notes.host_funcs.contains("now-ms"),
            "notes host_funcs ⊇ {{log, now-ms}}: {:?}",
            notes.host_funcs
        );
        assert!(
            notes.interfaces.contains(HOST_INTERFACE),
            "notes imports the host interface: {:?}",
            notes.interfaces
        );

        // Both are closed worlds — no wasi:sockets / wasi:http escaped.
        assert!(
            notes.closed_world_ok(),
            "notes closed world broken: {:?}",
            notes.foreign
        );
        assert!(
            nutrition.closed_world_ok(),
            "nutrition closed world broken: {:?}",
            nutrition.foreign
        );
    }
}
