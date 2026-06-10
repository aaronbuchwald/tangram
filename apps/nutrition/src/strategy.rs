//! The injectable nutrition-strategy seam, ported from Chamber's
//! `strategies.ts`.
//!
//! A [`Strategy`] is *how* a meal component gets its per-100g nutrient
//! values. Selection is by the `NUTRITION_STRATEGY` env var
//! (`offline | calorieninjas | llm`, default `offline`):
//!
//! - **Offline** (default) — deterministic and keyless. It has NO dynamic
//!   `resolve`; components fall back to whatever the reference data covers.
//!   Divergence from Chamber: Chamber pre-seeds a `component_nutrients`
//!   table from the strategy's `seed` (and creates it EMPTY for online
//!   strategies), while in Tangram the seed lives in the model's
//!   deterministic genesis document (`Nutrition::default()`), which every
//!   instance derives identically so their histories merge. The genesis seed
//!   is therefore always present; what the strategies vary is how NEW
//!   components get resolved.
//! - **CalorieNinjas** / **Llm** — online: `resolve` looks a component up
//!   over the network, OUTSIDE the store's synchronous action transaction.
//!   The resolved rows are then cached via the `add_component_nutrition`
//!   action, so they replicate to every peer and each novel component is
//!   resolved at most once ("resolve once, replay forever").

use tangram::prelude::*;

pub mod calorieninjas;
pub mod llm;

/// One resolved per-100g nutrient value for a component — the shape the
/// `add_component_nutrition` action caches into the replicated reference
/// tables.
#[model]
pub struct ResolvedNutrient {
    /// Display name, e.g. "Protein", "Sodium".
    pub name: String,
    /// "macro" or "micro".
    pub kind: String,
    /// Unit, e.g. "g", "mg", "mcg", "kcal".
    pub unit: String,
    pub amount_per_100g: f64,
}

/// A pluggable way to populate nutrition reference data for novel meal
/// components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    Offline,
    CalorieNinjas,
    Llm,
}

impl Strategy {
    /// Select a strategy from the `NUTRITION_STRATEGY` env var. Defaults to
    /// offline (deterministic, keyless) for an unset or unknown value.
    pub fn from_env() -> Self {
        match std::env::var("NUTRITION_STRATEGY").as_deref() {
            Ok("calorieninjas") => Self::CalorieNinjas,
            Ok("llm") => Self::Llm,
            _ => Self::Offline,
        }
    }

    /// Stable name (matches the NUTRITION_STRATEGY values).
    pub fn name(self) -> &'static str {
        match self {
            Self::Offline => "offline",
            Self::CalorieNinjas => "calorieninjas",
            Self::Llm => "llm",
        }
    }

    /// Whether this strategy can resolve novel components dynamically. The
    /// offline strategy cannot — it relies entirely on the genesis seed plus
    /// explicitly registered data.
    pub fn can_resolve(self) -> bool {
        !matches!(self, Self::Offline)
    }

    /// Dynamic per-100g lookup for a component not yet covered by the
    /// reference data. Returns `Ok(None)` when the strategy can't make sense
    /// of the food (the component just won't contribute nutrition — it does
    /// not fail the meal). Runs over the network, so it must be called
    /// OUTSIDE any action (actions are synchronous and hold the store lock).
    pub async fn resolve(self, component: &str) -> anyhow::Result<Option<Vec<ResolvedNutrient>>> {
        match self {
            Self::Offline => anyhow::bail!(
                "the offline strategy cannot resolve components dynamically; \
                 provide explicit components or register data via add_component_nutrition"
            ),
            Self::CalorieNinjas => calorieninjas::resolve(component).await,
            Self::Llm => llm::resolve(component).await,
        }
    }
}
