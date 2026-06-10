//! CalorieNinjas-backed strategy, ported from Chamber's
//! `calorieninjas_strategy.ts`.
//!
//! `resolve(component)` queries the CalorieNinjas API for a free-text food
//! and maps EVERY nutrient field the API returns for the FIRST matched item
//! into per-100g [`ResolvedNutrient`]s. Nothing is hardcoded: the numeric
//! fields of the response are enumerated dynamically and each row's unit and
//! kind derive from the field-name suffix (`calories`→kcal/macro, `_g`→
//! g/macro, `_mg`→mg/micro, `_mcg`→mcg/micro) — if CalorieNinjas adds a
//! nutrient, it flows through automatically rather than being silently
//! dropped. Requires CALORIENINJAS_API_KEY; select via
//! NUTRITION_STRATEGY=calorieninjas.
//!
//! API: GET https://api.calorieninjas.com/v1/nutrition?query=<food>, header
//! `X-Api-Key: <key>`. Values are reported per `serving_size_g`; we
//! normalize to per-100g.

use anyhow::Context;

use super::ResolvedNutrient;

const CALORIENINJAS_URL: &str = "https://api.calorieninjas.com/v1/nutrition";

/// Fields that are NOT per-serving nutrient amounts and so are excluded from
/// the dynamic enumeration: the item name and the serving basis we normalize
/// against.
const NON_NUTRIENT_FIELDS: &[&str] = &["name", "serving_size_g"];

/// Nicer display names for fields whose raw key humanizes awkwardly;
/// everything else is humanized from the field name.
const DISPLAY_NAMES: &[(&str, &str)] = &[
    ("calories", "Calories"),
    ("protein_g", "Protein"),
    ("carbohydrates_total_g", "Carbs"),
    ("fat_total_g", "Fat"),
    ("fat_saturated_g", "Saturated Fat"),
    ("fiber_g", "Fiber"),
    ("sugar_g", "Sugars"),
    ("sodium_mg", "Sodium"),
    ("potassium_mg", "Potassium"),
    ("cholesterol_mg", "Cholesterol"),
];

/// Derive a (unit, kind) from a CalorieNinjas field name by its suffix.
/// `calories` is special. Unrecognizable fields are skipped.
fn unit_and_kind(field: &str) -> Option<(&'static str, &'static str)> {
    if field == "calories" {
        return Some(("kcal", "macro"));
    }
    if field.ends_with("_mcg") {
        return Some(("mcg", "micro"));
    }
    if field.ends_with("_mg") {
        return Some(("mg", "micro"));
    }
    if field.ends_with("_g") {
        return Some(("g", "macro"));
    }
    None
}

/// Title-case a snake_case field, dropping a trailing unit suffix:
/// `fat_saturated_g` → "Fat Saturated" (Chamber's `humanizeField`).
fn humanize_field(field: &str) -> String {
    let base = field
        .strip_suffix("_mcg")
        .or_else(|| field.strip_suffix("_mg"))
        .or_else(|| field.strip_suffix("_g"))
        .unwrap_or(field);
    base.split('_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn display_name(field: &str) -> String {
    DISPLAY_NAMES
        .iter()
        .find(|(f, _)| *f == field)
        .map(|(_, n)| (*n).to_string())
        .unwrap_or_else(|| humanize_field(field))
}

/// Resolve a free-text food via the live CalorieNinjas API, taking every
/// nutrient field it returns for the first matched item, normalized to
/// per-100g.
pub async fn resolve(component: &str) -> anyhow::Result<Option<Vec<ResolvedNutrient>>> {
    let api_key = std::env::var("CALORIENINJAS_API_KEY").map_err(|_| {
        anyhow::anyhow!("NUTRITION_STRATEGY=calorieninjas requires CALORIENINJAS_API_KEY to be set")
    })?;

    let resp = reqwest::Client::new()
        .get(CALORIENINJAS_URL)
        .query(&[("query", component)])
        .header("X-Api-Key", api_key)
        .send()
        .await
        .with_context(|| format!("CalorieNinjas request failed for {component:?}"))?;
    if !resp.status().is_success() {
        anyhow::bail!(
            "CalorieNinjas lookup failed ({}) for {component:?}",
            resp.status()
        );
    }
    let data: serde_json::Value = resp.json().await.context("CalorieNinjas response body")?;
    let Some(item) = data.get("items").and_then(|i| i.get(0)) else {
        return Ok(None);
    };

    // Normalize the returned serving to per-100g. A non-positive or
    // non-numeric serving_size_g makes the per-100g scale undefined: passing
    // raw per-serving values through unscaled would permanently cache WRONG
    // per-100g rows (and "resolve once" makes that mistake sticky). Treat it
    // as unresolvable instead.
    let serving_g = item.get("serving_size_g").and_then(|v| v.as_f64());
    let Some(serving_g) = serving_g.filter(|g| g.is_finite() && *g > 0.0) else {
        return Ok(None);
    };

    let Some(fields) = item.as_object() else {
        return Ok(None);
    };
    let mut rows = Vec::new();
    for (field, raw) in fields {
        if NON_NUTRIENT_FIELDS.contains(&field.as_str()) {
            continue;
        }
        let Some(value) = raw.as_f64().filter(|v| v.is_finite()) else {
            continue;
        };
        let Some((unit, kind)) = unit_and_kind(field) else {
            continue;
        };
        rows.push(ResolvedNutrient {
            name: display_name(field),
            kind: kind.to_string(),
            unit: unit.to_string(),
            amount_per_100g: (value / serving_g) * 100.0,
        });
    }
    Ok(if rows.is_empty() { None } else { Some(rows) })
}
