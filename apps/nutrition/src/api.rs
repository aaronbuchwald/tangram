//! Strategy-backed HTTP routes layered on top of the derived Tangram
//! surface — the async/network half of Chamber's two-phase logMeal handler.
//!
//! Tangram actions are synchronous and run under the store lock, so network
//! I/O can never happen inside one. Description-based logging is therefore
//! two-phase: this module resolves components via the active
//! [`Strategy`](crate::strategy::Strategy) OUTSIDE the transaction (async,
//! over the network), then caches the rows and logs the meal through plain
//! actions (`add_component_nutrition` + `log_meal`) on the [`Handle`]. The
//! cached reference rows are ordinary replicated changes, so they sync to
//! every peer ("resolve once, replay forever").

use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tangram::Handle;

use crate::strategy::Strategy;
use crate::{MealComponent, Nutrition};

#[derive(Clone)]
struct ApiState {
    handle: Handle<Nutrition>,
    strategy: Strategy,
}

/// The custom routes: description-first meal logging and the capabilities
/// probe the UI uses to decide whether to offer the description path.
pub fn routes(handle: Handle<Nutrition>) -> Router {
    let strategy = Strategy::from_env();
    tracing::info!("nutrition strategy: {}", strategy.name());
    Router::new()
        .route("/api/log", post(log))
        .route("/api/capabilities", get(capabilities))
        .with_state(ApiState { handle, strategy })
}

async fn capabilities(
    axum::extract::State(state): axum::extract::State<ApiState>,
) -> Json<serde_json::Value> {
    Json(json!({
        "strategy": state.strategy.name(),
        "description_input": state.strategy.can_resolve(),
    }))
}

#[derive(Deserialize)]
struct LogRequest {
    name: Option<String>,
    description: Option<String>,
    #[serde(default)]
    components: Vec<ComponentSpec>,
    eaten_at_ms: Option<i64>,
}

#[derive(Deserialize, Clone)]
struct ComponentSpec {
    component: String,
    qty_g: f64,
}

type ApiError = (StatusCode, Json<serde_json::Value>);

fn err(status: StatusCode, msg: impl std::fmt::Display) -> ApiError {
    (status, Json(json!({ "error": msg.to_string() })))
}

/// Meal parsing, mirroring Chamber's `parseMeal` contract: explicit
/// `components` win; otherwise the whole description is treated as a single
/// 100g component (CalorieNinjas-style natural-language queries handle the
/// quantities inside it, e.g. "1 cup rice and 200g chicken").
async fn log(
    axum::extract::State(state): axum::extract::State<ApiState>,
    Json(req): Json<LogRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let description = req.description.as_deref().unwrap_or("").trim().to_string();
    let name = req
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(String::from)
        .unwrap_or_else(|| {
            if description.is_empty() {
                "meal".into()
            } else {
                description.clone()
            }
        });

    let explicit: Vec<MealComponent> = req
        .components
        .iter()
        .map(|c| MealComponent {
            component: c.component.trim().to_string(),
            qty_g: c.qty_g,
        })
        .filter(|c| !c.component.is_empty() && c.qty_g.is_finite() && c.qty_g > 0.0)
        .collect();

    let components = if !explicit.is_empty() {
        explicit
    } else if !description.is_empty() {
        // Description path: nutrition has to come from a dynamic lookup, so
        // the offline strategy can't serve it — tell the caller plainly.
        if !state.strategy.can_resolve() {
            return Err(err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "the offline nutrition strategy cannot resolve a description; provide \
                 explicit components, or run with NUTRITION_STRATEGY=calorieninjas or llm",
            ));
        }
        vec![MealComponent {
            component: description,
            qty_g: 100.0,
        }]
    } else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "provide a description or at least one component with positive grams",
        ));
    };

    // Phase 1 (async, outside the store lock): resolve components the
    // reference data doesn't cover yet, via the active strategy.
    // Description-only meals resolve in the foreground (their nutrition IS
    // the lookup); explicit-component meals log immediately and backfill in
    // the background (Chamber's fillNutrition behavior).
    let unknown = unknown_components(&state.handle, &components);
    let mut resolved = Vec::new();
    if state.strategy.can_resolve() && !unknown.is_empty() {
        if req.components.is_empty() {
            for component in unknown {
                match state.strategy.resolve(&component).await {
                    Ok(Some(nutrients)) => {
                        cache_component(&state.handle, &component, &nutrients)?;
                        resolved.push(component);
                    }
                    Ok(None) => {
                        tracing::info!("strategy could not resolve {component:?}; logging anyway")
                    }
                    Err(e) => return Err(err(StatusCode::BAD_GATEWAY, e)),
                }
            }
        } else {
            let handle = state.handle.clone();
            let strategy = state.strategy;
            tokio::spawn(async move {
                for component in unknown {
                    match strategy.resolve(&component).await {
                        Ok(Some(nutrients)) => {
                            if let Err((_, Json(e))) =
                                cache_component(&handle, &component, &nutrients)
                            {
                                tracing::warn!("failed to cache {component:?}: {e}");
                            }
                        }
                        Ok(None) => tracing::info!("strategy could not resolve {component:?}"),
                        Err(e) => tracing::warn!("background resolve of {component:?} failed: {e}"),
                    }
                }
            });
        }
    }

    // Phase 2 (sync action): log the meal as an ordinary replicated change.
    let meal_id = state
        .handle
        .apply(
            "log_meal",
            json!({ "name": name, "components": components, "eaten_at_ms": req.eaten_at_ms }),
        )
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    Ok(Json(json!({ "meal_id": meal_id, "resolved": resolved })))
}

/// The distinct components of a meal with no component mapping yet
/// (case-insensitive, matching `meal_nutrition`'s lookup rule).
fn unknown_components(handle: &Handle<Nutrition>, components: &[MealComponent]) -> Vec<String> {
    let state: Nutrition = match serde_json::from_value(handle.state_json()) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("could not hydrate state for component lookup: {e}");
            return Vec::new();
        }
    };
    let mut unknown: Vec<String> = Vec::new();
    for c in components {
        let known = state
            .component_mappings
            .iter()
            .any(|m| m.component.eq_ignore_ascii_case(&c.component));
        if !known && !unknown.iter().any(|u| u.eq_ignore_ascii_case(&c.component)) {
            unknown.push(c.component.clone());
        }
    }
    unknown
}

/// Cache freshly-resolved rows through the idempotent action (phase 2 of a
/// resolution — a plain replicated change).
fn cache_component(
    handle: &Handle<Nutrition>,
    component: &str,
    nutrients: &[crate::ResolvedNutrient],
) -> Result<(), ApiError> {
    handle
        .apply(
            "add_component_nutrition",
            json!({ "component": component, "nutrients": nutrients }),
        )
        .map(|_| ())
        .map_err(|e| err(StatusCode::INTERNAL_SERVER_ERROR, e))
}
