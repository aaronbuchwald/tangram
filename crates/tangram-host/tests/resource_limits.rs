//! Wasmtime resource-limit bounds (marketplace-security-audit M2).
//!
//! The host runs every app as a Wasmtime component instance. Before this slice
//! a single buggy/malicious component could OOM the host (unbounded
//! `memory.grow`) or spin a guest call forever (no CPU bound). These tests
//! prove the two bounds that close M2 — a `StoreLimits` memory cap and an
//! epoch-interruption CPU bound — at the store level, AND that tripping either
//! is a clean per-instance trap/error that leaves the host process alive.
//!
//! They are deliberately store-level (a tiny hand-written core module) rather
//! than a full malicious component fixture: the limiter + epoch callback are
//! installed on the `Store` exactly as `runtime::ComponentHandle::instantiate`
//! installs them (`Store::limiter`, `set_epoch_deadline`,
//! `epoch_deadline_callback` that yields under the budget and traps at the cap),
//! and the host-wide epoch ticker is the same one-tokio-task-incrementing-the-
//! engine-epoch shape as `runtime::EpochTicker`. `tangram-host` is a binary-only
//! crate (no lib target), so the test cannot import the module; the values
//! below MIRROR the `pub const`s in `crates/tangram-host/src/runtime.rs` and the
//! callback logic there — keep them in sync (change both together).

use std::time::Duration;

use wasmtime::{Config, Engine, Module, Store, StoreLimits, StoreLimitsBuilder, UpdateDeadline};

// ── Mirror of the production constants/logic in runtime.rs (M2) ──────────────
const DEFAULT_MAX_MEMORY_BYTES: usize = 512 * 1024 * 1024;
const EPOCH_TICK_INTERVAL: Duration = Duration::from_millis(100);
const EPOCH_DEADLINE_TICKS: u64 = 10;
const MAX_EPOCH_SLICES: u64 = 10;

/// Store data mirroring the relevant `runtime::HostState` fields: the limiter
/// and the per-call epoch-slice counter the callback bumps.
struct Data {
    limits: StoreLimits,
    epoch_slices: u64,
}

/// The host-wide epoch ticker, mirroring `runtime::EpochTicker`: one tokio task
/// that increments the shared engine's epoch on a fixed interval until told to
/// stop via a `watch` channel.
struct EpochTicker {
    shutdown: tokio::sync::watch::Sender<bool>,
}

impl EpochTicker {
    fn spawn(engine: Engine) -> (Self, tokio::task::JoinHandle<()>) {
        let (shutdown, mut rx) = tokio::sync::watch::channel(false);
        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(EPOCH_TICK_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = interval.tick() => engine.increment_epoch(),
                    _ = rx.changed() => {
                        if *rx.borrow() {
                            return;
                        }
                    }
                }
            }
        });
        (Self { shutdown }, handle)
    }

    fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}

struct Harness;

impl Harness {
    /// An engine configured exactly like the host's: epoch interruption on
    /// (the CPU bound). Async support is the default in wasmtime 45.
    fn engine() -> Engine {
        let mut config = Config::new();
        config.epoch_interruption(true);
        Engine::new(&config).expect("engine")
    }

    /// Mirror the production store wiring: install the memory/table limiter via
    /// `Store::limiter`, set the epoch deadline, and install the same
    /// yield-under-budget / trap-at-cap callback as `ComponentHandle`.
    fn store(engine: &Engine) -> Store<Data> {
        let limits = StoreLimitsBuilder::new()
            .memory_size(DEFAULT_MAX_MEMORY_BYTES)
            .build();
        let mut store = Store::new(
            engine,
            Data {
                limits,
                epoch_slices: 0,
            },
        );
        store.limiter(|data| &mut data.limits);
        store.set_epoch_deadline(EPOCH_DEADLINE_TICKS);
        store.epoch_deadline_callback(|mut ctx| {
            let data = ctx.data_mut();
            data.epoch_slices += 1;
            if data.epoch_slices >= MAX_EPOCH_SLICES {
                Err(wasmtime::Error::msg("guest CPU budget exceeded"))
            } else {
                Ok(UpdateDeadline::Yield(EPOCH_DEADLINE_TICKS))
            }
        });
        store
    }
}

/// A component instance that tries to grow linear memory past the cap must be
/// stopped by the limiter (a clean failure), and the host process must survive.
///
/// `memory.grow` returns -1 on a denied request rather than trapping, so the
/// module checks the result and traps with `unreachable` — proving the limiter
/// DENIED the growth (had it been allowed, `grow` would have returned the old
/// size and the function would have returned 0 cleanly). Either way the failure
/// is contained in this store; the engine/process keeps running.
#[tokio::test]
async fn memory_growth_past_cap_is_limited_host_survives() {
    let engine = Harness::engine();
    // 9000 wasm pages = ~590 MiB, over the 512 MiB cap. The module grows in one
    // shot and traps if growth was denied (grow == -1).
    let wat = r#"
        (module
          (memory 1)
          (func (export "grow_past_cap") (result i32)
            (if (i32.eq (memory.grow (i32.const 9000)) (i32.const -1))
              (then (unreachable)))
            (i32.const 0)))
    "#;
    let module = Module::new(&engine, wat).expect("compile grow module");
    let mut store = Harness::store(&engine);
    let instance = wasmtime::Instance::new_async(&mut store, &module, &[])
        .await
        .expect("instantiate");
    let func = instance
        .get_typed_func::<(), i32>(&mut store, "grow_past_cap")
        .expect("grow_past_cap export");

    let result = func.call_async(&mut store, ()).await;
    assert!(
        result.is_err(),
        "growing memory past the {DEFAULT_MAX_MEMORY_BYTES}-byte cap must be denied (got Ok)"
    );

    // Host survival: a brand-new store on the SAME engine still works after the
    // limited instance trapped.
    let mut store2 = Harness::store(&engine);
    let inst2 = wasmtime::Instance::new_async(&mut store2, &module, &[])
        .await
        .expect("engine still usable after a limited instance");
    let _ = inst2.get_memory(&mut store2, "memory");
}

/// A growth request that stays UNDER the cap must succeed — proving the cap is
/// a ceiling, not a blanket denial that would break real apps (whose working
/// set is well under 512 MiB).
#[tokio::test]
async fn memory_growth_under_cap_succeeds() {
    let engine = Harness::engine();
    // 100 pages = ~6.5 MiB, comfortably under the cap.
    let wat = r#"
        (module
          (memory 1)
          (func (export "grow_a_bit") (result i32)
            (memory.grow (i32.const 100))))
    "#;
    let module = Module::new(&engine, wat).expect("compile");
    let mut store = Harness::store(&engine);
    let instance = wasmtime::Instance::new_async(&mut store, &module, &[])
        .await
        .expect("instantiate");
    let func = instance
        .get_typed_func::<(), i32>(&mut store, "grow_a_bit")
        .expect("export");
    let prev = func
        .call_async(&mut store, ())
        .await
        .expect("grow succeeds");
    assert_eq!(
        prev, 1,
        "grow returns the previous page count, not -1 (denied)"
    );
}

/// A guest that spins on-CPU without yielding must be interrupted by the epoch
/// CPU budget (a clean trap once the per-call slice cap is hit), and the host
/// must survive. The host-wide `EpochTicker` drives the epoch counter exactly
/// as in production.
///
/// Multi-thread runtime ON PURPOSE (matching production's `#[tokio::main]`,
/// which is multi-threaded): the epoch ticker must run on a worker thread that
/// the spinning guest can't starve. On a current-thread runtime the on-CPU
/// guest would block the only thread and the ticker could never fire — which is
/// also why production interruption relies on the multi-threaded host runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cpu_spin_is_interrupted_by_epoch_host_survives() {
    let engine = Harness::engine();
    let (ticker, ticker_task) = EpochTicker::spawn(engine.clone());

    // A tight infinite loop with NO call/yield point — the worst case the
    // epoch bound exists to stop.
    let wat = r#"
        (module
          (func (export "spin") (loop (br 0))))
    "#;
    let module = Module::new(&engine, wat).expect("compile spin module");
    let mut store = Harness::store(&engine);
    let instance = wasmtime::Instance::new_async(&mut store, &module, &[])
        .await
        .expect("instantiate");
    let func = instance
        .get_typed_func::<(), ()>(&mut store, "spin")
        .expect("spin export");

    // Bound the whole test: ~MAX_EPOCH_SLICES slices of EPOCH_DEADLINE_TICKS at
    // the tick interval, plus generous slack. If the epoch bound failed to
    // interrupt, this outer timeout fires and the assertion below fails loudly
    // instead of hanging CI.
    let one_slice = EPOCH_TICK_INTERVAL * (EPOCH_DEADLINE_TICKS as u32);
    let budget = one_slice * (MAX_EPOCH_SLICES as u32) * 4 + Duration::from_secs(10);
    let outcome = tokio::time::timeout(budget, func.call_async(&mut store, ())).await;

    ticker.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(5), ticker_task).await;

    let call_result = outcome.expect("epoch interruption must stop the spin within the budget");
    assert!(
        call_result.is_err(),
        "an infinite on-CPU loop must be trapped by the epoch CPU budget"
    );

    // Host survival: the engine is still usable for a fresh store after the
    // spinning instance was interrupted.
    let mut store2 = Harness::store(&engine);
    wasmtime::Instance::new_async(&mut store2, &module, &[])
        .await
        .expect("engine still usable after an epoch-interrupted instance");
}

/// A normal async workload — a guest that hits a host call (the yield point an
/// injected `http-fetch` would create) that takes LONGER than the on-CPU budget
/// and then returns — must complete, proving the epoch bound does NOT trap
/// legitimate work that yields to the host. This is the critical "don't trap
/// async waits" property: while the guest awaits the host call it is NOT
/// on-CPU, so the epoch deadline never advances toward the cap.
///
/// Multi-thread runtime to match production (and the spin test): the ticker
/// runs on its own worker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_async_workload_completes_under_epoch_bound() {
    let engine = Harness::engine();
    let (ticker, ticker_task) = EpochTicker::spawn(engine.clone());

    let wat = r#"
        (module
          (import "host" "host_wait" (func $wait))
          (func (export "do_work") (result i32)
            (call $wait)
            (i32.const 42)))
    "#;
    let module = Module::new(&engine, wat).expect("compile work module");
    let mut store = Harness::store(&engine);

    let mut linker = wasmtime::Linker::new(&engine);
    // A host import that awaits far longer than a whole on-CPU CPU budget would
    // allow. If the await (wrongly) counted as guest CPU this would be trapped;
    // because it does not, the call completes cleanly.
    let wait_for =
        EPOCH_TICK_INTERVAL * (EPOCH_DEADLINE_TICKS as u32) * (MAX_EPOCH_SLICES as u32) * 2;
    linker
        .func_wrap_async("host", "host_wait", move |_caller, _params: ()| {
            Box::new(async move {
                tokio::time::sleep(wait_for).await;
            })
        })
        .expect("link host_wait");

    let instance = linker
        .instantiate_async(&mut store, &module)
        .await
        .expect("instantiate");
    let func = instance
        .get_typed_func::<(), i32>(&mut store, "do_work")
        .expect("do_work export");

    let result = func
        .call_async(&mut store, ())
        .await
        .expect("a workload that yields at a host call must NOT be epoch-trapped");
    assert_eq!(
        result, 42,
        "the async workload completed with its real result"
    );

    ticker.shutdown();
    let _ = tokio::time::timeout(Duration::from_secs(5), ticker_task).await;
}
