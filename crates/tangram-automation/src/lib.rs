//! Tangram's browser + credential automation substrate (host-side).
//!
//! This crate is the four reusable primitives from
//! `docs/design/task-automation-browser.md`, recombining the host's
//! supervised-child pattern (`gateway.rs`) and host-side secret injection
//! (ADR-0005) into a domain-gated, credential-brokering browser runner that
//! lives BESIDE the Wasmtime engine — never reachable through any WIT import.
//! `tangram-core` and every WASM component stay browser-unaware (ADR-0010).
//!
//! - [`egress`] — Primitive B: the browser egress gate (§5).
//! - [`runner`] — Primitive A: the supervised browser-driver process (§4).
//! - [`broker`] — Primitive C: the credential broker over `op://` (§6).
//! - [`script`] — Primitive D: the record → replay → LLM-fallback engine (§7).
//! - [`request`] — Primitive A.3: the request-not-grant `AutomationRequest`
//!   channel + operator-policy intersection (§4.3).

pub mod broker;
pub mod egress;
pub mod request;
pub mod runner;
pub mod script;
