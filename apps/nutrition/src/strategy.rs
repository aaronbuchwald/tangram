//! The injectable nutrition-strategy seam.
//!
//! A [`Strategy`] is *how* a meal component gets its per-100g nutrient
//! values. Selection: an explicit `NUTRITION_STRATEGY` env var
//! (`offline | calorieninjas | llm`) wins; when unset, the presence of
//! `CALORIENINJAS_API_KEY` auto-enables CalorieNinjas, else offline:
//!
//! - **Offline** (keyless default) — deterministic and keyless. It has NO dynamic
//!   `resolve`; components fall back to whatever the reference data covers.
//!   The seed lives in the model's deterministic genesis document
//!   (`Nutrition::default()`), which every instance derives identically so
//!   their histories merge. The genesis seed is therefore always present
//!   regardless of strategy; what the strategies vary is how NEW components
//!   get resolved.
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
    /// Select a strategy from the environment: an explicit
    /// `NUTRITION_STRATEGY` wins; when it is unset, the presence of
    /// `CALORIENINJAS_API_KEY` auto-enables CalorieNinjas (online resolution
    /// is the default expectation, not an exception); otherwise offline
    /// (deterministic, keyless).
    pub fn from_env() -> Self {
        Self::from_env_with_reason().0
    }

    /// [`from_env`](Self::from_env), plus a human-readable reason for the
    /// choice (for startup logging).
    pub fn from_env_with_reason() -> (Self, &'static str) {
        match std::env::var("NUTRITION_STRATEGY").as_deref() {
            Ok("calorieninjas") => (Self::CalorieNinjas, "NUTRITION_STRATEGY=calorieninjas"),
            Ok("llm") => (Self::Llm, "NUTRITION_STRATEGY=llm"),
            Ok("offline") => (Self::Offline, "NUTRITION_STRATEGY=offline"),
            Ok(_) => (
                Self::Offline,
                "unknown NUTRITION_STRATEGY value; falling back to offline",
            ),
            Err(_) => {
                if std::env::var("CALORIENINJAS_API_KEY").is_ok_and(|k| !k.trim().is_empty()) {
                    (
                        Self::CalorieNinjas,
                        "NUTRITION_STRATEGY unset; CALORIENINJAS_API_KEY present, auto-enabling calorieninjas",
                    )
                } else {
                    (
                        Self::Offline,
                        "NUTRITION_STRATEGY unset and no CALORIENINJAS_API_KEY; offline",
                    )
                }
            }
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
    /// not fail the meal). Runs over the network, so call it from async
    /// actions or background tasks — never while holding the store lock.
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
