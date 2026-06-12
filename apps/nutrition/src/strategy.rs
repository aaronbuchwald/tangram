//! The injectable nutrition-strategy seam.
//!
//! A [`Strategy`] is *how* a meal component gets its per-100g nutrient
//! values. Selection: an explicit `NUTRITION_STRATEGY` env var
//! (`calorieninjas | llm`) wins; when unset (or set to an unknown value)
//! the default is **CalorieNinjas**.
//!
//! - **CalorieNinjas** (default) / **Llm** — online: `resolve` looks a
//!   component up over the network, OUTSIDE the store's synchronous action
//!   transaction. The resolved rows are then cached via the
//!   `add_component_nutrition` action, so they replicate to every peer and
//!   each novel component is resolved at most once ("resolve once, replay
//!   forever").
//!
//! Both strategies resolve over the network and require credentials
//! (CalorieNinjas: `CALORIENINJAS_API_KEY`; Llm: see [`llm`]). Selection does
//! not check for the credential — a missing key surfaces as a clear error at
//! `resolve` time, never a panic. The deterministic genesis seed
//! (`Nutrition::default()`) is always present regardless of strategy; what the
//! strategies vary is how NEW components get resolved.

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
    CalorieNinjas,
    Llm,
}

impl Strategy {
    /// Select a strategy from the environment: an explicit
    /// `NUTRITION_STRATEGY` wins; when it is unset (or set to an unknown
    /// value) the default is [`CalorieNinjas`](Self::CalorieNinjas). Selection
    /// does not check for a credential — a missing key surfaces as a clear
    /// error at [`resolve`](Self::resolve) time, never a panic.
    pub fn from_env() -> Self {
        Self::from_env_with_reason().0
    }

    /// [`from_env`](Self::from_env), plus a human-readable reason for the
    /// choice (for startup logging).
    pub fn from_env_with_reason() -> (Self, &'static str) {
        match std::env::var("NUTRITION_STRATEGY").as_deref() {
            Ok("calorieninjas") => (Self::CalorieNinjas, "NUTRITION_STRATEGY=calorieninjas"),
            Ok("llm") => (Self::Llm, "NUTRITION_STRATEGY=llm"),
            Ok(_) => (
                Self::CalorieNinjas,
                "unknown NUTRITION_STRATEGY value; defaulting to calorieninjas",
            ),
            Err(_) => (
                Self::CalorieNinjas,
                "NUTRITION_STRATEGY unset; defaulting to calorieninjas",
            ),
        }
    }

    /// Stable name (matches the NUTRITION_STRATEGY values).
    pub fn name(self) -> &'static str {
        match self {
            Self::CalorieNinjas => "calorieninjas",
            Self::Llm => "llm",
        }
    }

    /// Whether this strategy can resolve novel components dynamically. Every
    /// remaining strategy resolves over the network, so this is always `true`;
    /// it is retained so the capabilities shape stays explicit about the
    /// `description_input` contract.
    pub fn can_resolve(self) -> bool {
        match self {
            Self::CalorieNinjas | Self::Llm => true,
        }
    }

    /// Dynamic per-100g lookup for a component not yet covered by the
    /// reference data. Returns `Ok(None)` when the strategy can't make sense
    /// of the food (the component just won't contribute nutrition — it does
    /// not fail the meal). Runs over the network, so call it from async
    /// actions or background tasks — never while holding the store lock.
    pub async fn resolve(self, component: &str) -> anyhow::Result<Option<Vec<ResolvedNutrient>>> {
        match self {
            Self::CalorieNinjas => calorieninjas::resolve(component).await,
            Self::Llm => llm::resolve(component).await,
        }
    }
}
