//! Host-side agent scheduler (Triggers v1): a supervised interval task that
//! drives the `tangram` shell app's `tick_agents` action on a fixed cadence so
//! cron-triggered agent notes run with NO browser open.
//!
//! This mirrors the gateway supervisor's spawn/shutdown shape
//! (`gateway::Gateway::spawn_supervisor`): a `tokio::spawn`ed loop that
//! `select!`s the interval against a `watch` shutdown signal and stops cleanly
//! on host shutdown. The component decides which agents are DUE and performs
//! the LLM egress; the host just delivers the tick through the ordinary
//! action-dispatch path (`AppRuntime::dispatch`), so all the existing egress
//! enforcement and credential injection apply unchanged.
//!
//! v1 is interval/cron only, DeepSeek only, output appended to the agent's own
//! note. A failing dispatch is logged and the loop continues — a misbehaving
//! agent never crashes the host.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::watch;

use crate::Host;
use crate::tenant::AppKey;

/// The app whose `tick_agents` action the scheduler drives (the Obsidian-style
/// shell, which owns the markdown vault the agent notes live in).
const AGENT_APP: &str = "tangram";

/// The host tick cadence. A small fixed constant for v1 (no config knob): the
/// component's schedule grammar (`@hourly`, `every 5m`, …) sets the real
/// cadence; this is just how often the host asks "is anything due?".
const TICK: Duration = Duration::from_secs(60);

/// The supervised scheduler: an interval loop + a shutdown channel.
pub struct Scheduler {
    host: Arc<Host>,
    tick: Duration,
    shutdown: watch::Sender<bool>,
}

impl Scheduler {
    pub fn new(host: Arc<Host>) -> Self {
        Self {
            host,
            tick: TICK,
            shutdown: watch::Sender::new(false),
        }
    }

    /// Spawn the interval loop. Every `tick`, if the `tangram` app is running,
    /// dispatch its `tick_agents` action; log and continue on any error.
    /// Stops cleanly when [`Scheduler::shutdown`] is signalled.
    pub fn spawn(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let sched = self.clone();
        tokio::spawn(async move {
            let mut shutdown = sched.shutdown.subscribe();
            let mut interval = tokio::time::interval(sched.tick);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // The first immediate tick fires at startup; skip waiting a full
            // period before the first scan.
            loop {
                tokio::select! {
                    _ = interval.tick() => sched.run_once().await,
                    _ = shutdown.changed() => {
                        tracing::info!("scheduler: stopped");
                        return;
                    }
                }
            }
        })
    }

    /// One tick: dispatch `tick_agents` to the running `tangram` app, if any.
    /// Absent app → silent no-op (the shell may simply not be installed); a
    /// dispatch error → logged, loop continues.
    async fn run_once(&self) {
        let entry = {
            let apps = self.host.apps.read().await;
            apps.get(&AppKey::top(AGENT_APP)).map(|e| e.runtime.clone())
        };
        let Some(runtime) = entry else { return };
        match runtime.dispatch("tick_agents", json!({})).await {
            Ok(ran) => {
                // `tick_agents` returns the names that ran (a JSON array); only
                // log when something actually fired (the common case is empty).
                if ran.as_array().is_some_and(|a| !a.is_empty()) {
                    tracing::info!("scheduler: agents ran this tick: {ran}");
                }
            }
            Err(e) => tracing::warn!("scheduler: tick_agents dispatch failed: {e}"),
        }
    }

    /// Stop the loop (used on Ctrl-C, like the gateway supervisor).
    pub fn shutdown(&self) {
        let _ = self.shutdown.send(true);
    }
}
