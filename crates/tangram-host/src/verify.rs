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

// CP1 introduces the audit reader as a standalone module; CP2+ wire its
// predicates (`imports_http_fetch`, `closed_world_ok`) and the chain types
// into `AppRuntime::build`. Until that wiring lands they are exercised only by
// the unit tests, which `dead_code` does not count for a binary crate.
#![allow(dead_code)]

use std::collections::BTreeSet;

use wasmtime::Engine;
use wasmtime::component::Component;
use wasmtime::component::types::ComponentItem;

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
