//! The WASM guest adapter: wires any `Model + Actions` into the
//! `tangram:app` component world (defined in `crates/tangram-host/wit/`).
//!
//! A Tangram app becomes a component by adding a `cdylib` crate-type and one
//! line:
//!
//! ```ignore
//! #[cfg(target_family = "wasm")]
//! tangram::export_component!(Notes { name: "notes", instructions: INSTRUCTIONS });
//! ```
//!
//! The component contains ONLY app logic. Each `dispatch` is doc-in/doc-out:
//! the host hands in the current document bytes, the guest loads them into an
//! in-memory [`Store`](crate::store::Store), runs the action through the SAME
//! registry/dispatch path the native host uses (so every surface keeps one
//! contract), and hands back the mutated save. Async actions run under a
//! tiny single-pass executor: every await point in the guest (the
//! `http-fetch` import) completes synchronously from the guest's view, so
//! futures never park.

use std::sync::Arc;

use serde_json::json;

use tangram_core::Store;

use crate::action::Actions;
use crate::{ActionError, Model};

/// The generated `tangram:app` bindings (public so the `export_component!`
/// expansion in app crates can reach the export macro and trait, and so the
/// `http`/`time` facades can call the imports).
pub mod wit {
    wit_bindgen::generate!({
        path: "../tangram-host/wit",
        world: "app",
        pub_export_macro: true,
        default_bindings_module: "tangram::guest::wit",
    });
}

pub use wit::exports::tangram::app::guest::DispatchResult;

/// Drive a future to completion on the guest's single thread. Every await
/// point inside a component resolves synchronously (host imports return
/// ready), so a `Pending` here means someone awaited a primitive that needs
/// a real runtime — fail loudly rather than spin.
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = std::pin::pin!(fut);
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    match fut.as_mut().poll(&mut cx) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!(
            "a guest action awaited a future that did not complete synchronously; \
             only tangram::http / tangram::time I/O is available inside a component"
        ),
    }
}

/// One-time guest process setup: route `tracing` events and panics to the
/// host's `log` import so component logs land in the host's output.
fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = tracing::subscriber::set_global_default(HostLogSubscriber);
        std::panic::set_hook(Box::new(|info| {
            wit::tangram::app::host::log("error", &format!("guest panic: {info}"));
        }));
    });
}

/// `describe()` export: the app manifest the host derives its routes and MCP
/// tools from — name, MCP instructions, the full action registry, and an
/// optional `capabilities` object.
///
/// `capabilities` runs at instantiation (the host calls `describe()` once
/// per instance) so an app can compute it from its granted environment —
/// e.g. nutrition reports its active strategy exactly as its native
/// `GET /api/capabilities` route does. `None` omits the key entirely: the
/// host then serves no `/api/capabilities` route for the app (404), matching
/// a native app without the custom probe.
pub fn describe<M: Model + Actions>(
    name: &str,
    instructions: &str,
    capabilities: impl FnOnce() -> Option<serde_json::Value>,
) -> String {
    init();
    let actions: Vec<_> = M::actions()
        .iter()
        .map(|a| {
            json!({
                "name": a.name,
                "description": a.description,
                "mutates": a.mutates,
                "input_schema": (a.input_schema)(),
            })
        })
        .collect();
    let mut manifest = json!({
        "name": name,
        "instructions": instructions,
        "actions": actions,
    });
    if let Some(caps) = capabilities() {
        manifest["capabilities"] = caps;
    }
    manifest.to_string()
}

/// `genesis()` export: the deterministic genesis bytes — same function the
/// native store runs, so guest and native genesis are byte-identical by
/// construction.
pub fn genesis<M: Model>() -> Vec<u8> {
    tangram_core::genesis_bytes::<M>().expect("model Default reconciles into genesis")
}

/// `dispatch()` export: run one action against the given document bytes.
pub fn dispatch<M: Model + Actions>(
    action: &str,
    args_json: &str,
    doc: Vec<u8>,
) -> Result<DispatchResult, String> {
    init();
    let args: serde_json::Value = if args_json.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(args_json)
            .map_err(|e| ActionError::bad_args(format!("args must be JSON: {e}")).to_string())?
    };
    let store =
        Arc::new(Store::<M>::in_memory(&doc).map_err(|e| ActionError::internal(e).to_string())?);
    let heads_before = store.heads();
    let result = block_on(store.dispatch(action, args)).map_err(|e| e.to_string())?;
    let doc_out = (store.heads() != heads_before).then(|| store.save());
    Ok(DispatchResult {
        doc: doc_out,
        result_json: result.to_string(),
    })
}

/// `state-json()` export: hydrate the model from document bytes and render
/// it as JSON (same shape as the native `/api/state`).
pub fn state_json<M: Model + Actions>(doc: Vec<u8>) -> String {
    init();
    match Store::<M>::in_memory(&doc) {
        Ok(store) => store.state_json().to_string(),
        Err(e) => json!({ "error": e.to_string() }).to_string(),
    }
}

/// Export a model as a Tangram component: implements the `tangram:app/guest`
/// interface for `$model` and registers it as the component's export.
///
/// ```ignore
/// tangram::export_component!(Notes { name: "notes", instructions: "..." });
/// ```
///
/// An optional `capabilities` field (any expression callable as
/// `FnOnce() -> Option<serde_json::Value>`) lets the app publish a
/// capabilities object in its `describe()` manifest, computed from its
/// granted environment at instantiation; the host serves it at
/// `GET /<app>/api/capabilities`:
///
/// ```ignore
/// tangram::export_component!(Nutrition {
///     name: "nutrition",
///     instructions: "...",
///     capabilities: || Some(serde_json::json!({ "description_input": true })),
/// });
/// ```
#[macro_export]
macro_rules! export_component {
    ($model:ty { name: $name:expr, instructions: $instructions:expr $(,)? }) => {
        $crate::export_component!($model {
            name: $name,
            instructions: $instructions,
            capabilities: || ::std::option::Option::None,
        });
    };
    ($model:ty { name: $name:expr, instructions: $instructions:expr,
                 capabilities: $capabilities:expr $(,)? }) => {
        const _: () = {
            struct TangramComponent;
            impl $crate::guest::wit::exports::tangram::app::guest::Guest for TangramComponent {
                fn describe() -> ::std::string::String {
                    $crate::guest::describe::<$model>($name, $instructions, $capabilities)
                }
                fn genesis() -> ::std::vec::Vec<u8> {
                    $crate::guest::genesis::<$model>()
                }
                fn dispatch(
                    action: ::std::string::String,
                    args_json: ::std::string::String,
                    doc: ::std::vec::Vec<u8>,
                ) -> ::std::result::Result<$crate::guest::DispatchResult, ::std::string::String>
                {
                    $crate::guest::dispatch::<$model>(&action, &args_json, doc)
                }
                fn state_json(doc: ::std::vec::Vec<u8>) -> ::std::string::String {
                    $crate::guest::state_json::<$model>(doc)
                }
            }
            $crate::guest::wit::export!(TangramComponent);
        };
    };
}

// ── tracing → host log ───────────────────────────────────────────────────────

/// A minimal tracing subscriber forwarding every event's message (plus its
/// other fields as `key=value`) to the host's `log` import. Spans are
/// accepted but not tracked — component dispatches are short, synchronous
/// calls.
struct HostLogSubscriber;

impl tracing::Subscriber for HostLogSubscriber {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        let mut text = String::new();
        event.record(&mut MessageVisitor(&mut text));
        let level = match *event.metadata().level() {
            tracing::Level::ERROR => "error",
            tracing::Level::WARN => "warn",
            tracing::Level::INFO => "info",
            tracing::Level::DEBUG => "debug",
            tracing::Level::TRACE => "trace",
        };
        wit::tangram::app::host::log(level, text.trim());
    }
}

struct MessageVisitor<'a>(&'a mut String);

impl tracing::field::Visit for MessageVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        use std::fmt::Write as _;
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?} ");
        } else {
            let _ = write!(self.0, "{}={value:?} ", field.name());
        }
    }
}
