//! Nutrition — a Tangram port of Chamber's nutrition tracker prototype.
//!
//! Chamber's design uses a medallion (Bronze/Silver/Gold) SQLite layout:
//! Bronze = raw logged meals + components, Silver = component → ingredient
//! mappings, Gold = nutrient reference data and a per-meal aggregation view.
//! Here the same shape lives in one replicated Tangram model: `meals` is the
//! Bronze layer (the only thing users write at runtime), the mapping and
//! nutrient tables are the Silver/Gold reference data (seeded from Chamber's
//! dataset, extensible via `add_ingredient`), and `meal_nutrition` computes
//! the gold view on demand.

use tangram::prelude::*;

mod api;
pub mod strategy;

pub use strategy::ResolvedNutrient;

#[model]
pub struct Nutrition {
    /// Bronze: raw logged meals.
    meals: Vec<Meal>,
    /// Silver: normalized ingredient entities.
    ingredients: Vec<Ingredient>,
    /// Silver: component string → ingredient mappings.
    component_mappings: Vec<ComponentMapping>,
    /// Gold: nutrient reference data.
    nutrients: Vec<Nutrient>,
    /// Gold: per-100g nutrient amounts per ingredient.
    ingredient_nutrients: Vec<IngredientNutrient>,
}

#[model]
pub struct Meal {
    id: String,
    name: String,
    eaten_at_ms: i64,
    components: Vec<MealComponent>,
}

#[model]
pub struct MealComponent {
    component: String,
    qty_g: f64,
}

#[model]
pub struct Ingredient {
    id: String,
    canonical_name: String,
}

#[model]
pub struct ComponentMapping {
    component: String,
    ingredient_id: String,
    fraction: f64,
}

#[model]
pub struct Nutrient {
    id: String,
    name: String,
    /// "macro" or "micro"
    kind: String,
    unit: String,
}

#[model]
pub struct IngredientNutrient {
    ingredient_id: String,
    nutrient_id: String,
    amount_per_100g: f64,
}

/// One row of the gold view: a nutrient total for a meal.
#[model]
pub struct NutritionRow {
    meal_id: String,
    meal_name: String,
    nutrient: String,
    nutrient_kind: String,
    unit: String,
    amount: f64,
}

#[actions]
impl Nutrition {
    /// Log a meal with its components and gram quantities. Component names
    /// are matched (case-insensitively) against known ingredient mappings
    /// when nutrition is computed; unknown components simply contribute no
    /// nutrients until `add_ingredient` registers them. Returns the meal id.
    pub fn log_meal(
        &mut self,
        name: String,
        components: Vec<MealComponent>,
        eaten_at_ms: Option<i64>,
    ) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        self.meals.push(Meal {
            id: id.clone(),
            name,
            eaten_at_ms: eaten_at_ms.unwrap_or_else(now_ms),
            components,
        });
        id
    }

    /// Delete a logged meal by id.
    pub fn delete_meal(&mut self, id: String) -> Result<(), String> {
        let before = self.meals.len();
        self.meals.retain(|m| m.id != id);
        if self.meals.len() == before {
            return Err(format!("no meal with id {id}"));
        }
        Ok(())
    }

    /// List all logged meals, newest first.
    pub fn list_meals(&self) -> Vec<Meal> {
        let mut meals = self.meals.clone();
        meals.sort_by_key(|m| std::cmp::Reverse(m.eaten_at_ms));
        meals
    }

    /// Nutrition totals (the gold view) for one meal: per nutrient, summed
    /// over the meal's components, macros before micros.
    pub fn meal_nutrition(&self, meal_id: String) -> Result<Vec<NutritionRow>, String> {
        let meal = self
            .meals
            .iter()
            .find(|m| m.id == meal_id)
            .ok_or_else(|| format!("no meal with id {meal_id}"))?;

        let mut totals: Vec<(String, f64)> = Vec::new(); // (nutrient_id, amount)
        for comp in &meal.components {
            let Some(mapping) = self
                .component_mappings
                .iter()
                .find(|m| m.component.eq_ignore_ascii_case(&comp.component))
            else {
                continue; // unresolved component — contributes nothing
            };
            for inu in self
                .ingredient_nutrients
                .iter()
                .filter(|inu| inu.ingredient_id == mapping.ingredient_id)
            {
                let amount = inu.amount_per_100g * (comp.qty_g / 100.0) * mapping.fraction;
                match totals.iter_mut().find(|(id, _)| *id == inu.nutrient_id) {
                    Some((_, sum)) => *sum += amount,
                    None => totals.push((inu.nutrient_id.clone(), amount)),
                }
            }
        }

        let mut rows: Vec<NutritionRow> = totals
            .into_iter()
            .filter_map(|(nutrient_id, amount)| {
                let n = self.nutrients.iter().find(|n| n.id == nutrient_id)?;
                Some(NutritionRow {
                    meal_id: meal.id.clone(),
                    meal_name: meal.name.clone(),
                    nutrient: n.name.clone(),
                    nutrient_kind: n.kind.clone(),
                    unit: n.unit.clone(),
                    amount,
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            let kind = |r: &NutritionRow| if r.nutrient_kind == "macro" { 0 } else { 1 };
            kind(a).cmp(&kind(b)).then(a.nutrient.cmp(&b.nutrient))
        });
        Ok(rows)
    }

    /// Register nutrition data for a component (per 100g), so future and past
    /// meals using it resolve. The replicated reference data syncs to every
    /// device, mirroring Chamber's "resolve once, replay forever" caching.
    pub fn add_ingredient(
        &mut self,
        component: String,
        protein_g: f64,
        carbs_g: f64,
        fat_g: f64,
        vitamin_c_mg: f64,
        iron_mg: f64,
    ) -> String {
        let id = format!("ing_{}", uuid::Uuid::new_v4());
        self.ingredients.push(Ingredient {
            id: id.clone(),
            canonical_name: component.to_lowercase(),
        });
        self.component_mappings.push(ComponentMapping {
            component: component.to_lowercase(),
            ingredient_id: id.clone(),
            fraction: 1.0,
        });
        for (nutrient_id, amount) in [
            ("nut_protein", protein_g),
            ("nut_carbs", carbs_g),
            ("nut_fat", fat_g),
            ("nut_vitc", vitamin_c_mg),
            ("nut_iron", iron_mg),
        ] {
            self.ingredient_nutrients.push(IngredientNutrient {
                ingredient_id: id.clone(),
                nutrient_id: nutrient_id.to_string(),
                amount_per_100g: amount,
            });
        }
        id
    }

    /// Cache a full per-100g nutrient panel for a component (any number of
    /// nutrients, e.g. as resolved by an online nutrition strategy):
    /// registers unknown nutrient definitions, an ingredient, the component
    /// mapping, and the per-100g rows. Idempotent — re-adding an
    /// already-mapped component is a no-op — and replicated, so a component
    /// is resolved once and replays everywhere. Returns the ingredient id.
    pub fn add_component_nutrition(
        &mut self,
        component: String,
        nutrients: Vec<ResolvedNutrient>,
    ) -> Result<String, String> {
        let component = component.trim().to_lowercase();
        if component.is_empty() {
            return Err("component must not be empty".into());
        }
        // Idempotency: a component that already resolves keeps its data
        // ("resolve once, replay forever" — never overwrite cached rows).
        if let Some(mapping) = self
            .component_mappings
            .iter()
            .find(|m| m.component == component)
        {
            return Ok(mapping.ingredient_id.clone());
        }

        // Deterministic ingredient id from the slugified component, so two
        // peers caching the same component converge instead of duplicating.
        let ingredient_id = format!("ing_{}", slug(&component));
        if !self.ingredients.iter().any(|i| i.id == ingredient_id) {
            self.ingredients.push(Ingredient {
                id: ingredient_id.clone(),
                canonical_name: component.clone(),
            });
        }
        self.component_mappings.push(ComponentMapping {
            component,
            ingredient_id: ingredient_id.clone(),
            fraction: 1.0,
        });

        for n in nutrients {
            let name = n.name.trim();
            if name.is_empty() || !n.amount_per_100g.is_finite() {
                continue;
            }
            // Reuse an existing nutrient definition by display name (the
            // genesis seed uses short ids like nut_vitc for "Vitamin C");
            // otherwise register one under a deterministic slugified id.
            let nutrient_id = match self
                .nutrients
                .iter()
                .find(|d| d.name.eq_ignore_ascii_case(name))
            {
                Some(def) => def.id.clone(),
                None => {
                    let id = format!("nut_{}", slug(name));
                    if !self.nutrients.iter().any(|d| d.id == id) {
                        self.nutrients.push(Nutrient {
                            id: id.clone(),
                            name: name.to_string(),
                            kind: if n.kind == "micro" { "micro" } else { "macro" }.into(),
                            unit: n.unit.clone(),
                        });
                    }
                    id
                }
            };
            if !self
                .ingredient_nutrients
                .iter()
                .any(|r| r.ingredient_id == ingredient_id && r.nutrient_id == nutrient_id)
            {
                self.ingredient_nutrients.push(IngredientNutrient {
                    ingredient_id: ingredient_id.clone(),
                    nutrient_id,
                    amount_per_100g: n.amount_per_100g,
                });
            }
        }
        Ok(ingredient_id)
    }
}

/// Slugify a name into a deterministic id fragment: lowercase alphanumerics
/// with single underscores ("Saturated Fat" → "saturated_fat").
fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_sep = true; // suppress a leading separator
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_sep = false;
        } else if !last_sep {
            out.push('_');
            last_sep = true;
        }
    }
    if out.ends_with('_') {
        out.pop();
    }
    out
}

/// Seed with Chamber's reference dataset. This is the genesis state, so it
/// must be deterministic (every instance derives the identical genesis
/// change; that shared root is what lets instances merge).
impl Default for Nutrition {
    fn default() -> Self {
        let ing = |id: &str, name: &str| Ingredient {
            id: id.into(),
            canonical_name: name.into(),
        };
        let map = |component: &str, ingredient_id: &str| ComponentMapping {
            component: component.into(),
            ingredient_id: ingredient_id.into(),
            fraction: 1.0,
        };
        let nut = |id: &str, name: &str, kind: &str, unit: &str| Nutrient {
            id: id.into(),
            name: name.into(),
            kind: kind.into(),
            unit: unit.into(),
        };

        // ingredient → (protein, carbs, fat, vitamin C, iron) per 100g
        let amounts: &[(&str, [f64; 5])] = &[
            ("ing_chicken", [31.0, 0.0, 3.6, 0.0, 1.0]),
            ("ing_rice", [2.6, 23.0, 0.9, 0.0, 0.5]),
            ("ing_olive", [0.0, 0.0, 100.0, 0.0, 0.6]),
            ("ing_broccoli", [2.8, 6.6, 0.4, 89.2, 0.7]),
            ("ing_egg", [13.0, 1.1, 11.0, 0.0, 1.8]),
            ("ing_oats", [17.0, 66.0, 7.0, 0.0, 4.7]),
        ];
        let nutrient_ids = [
            "nut_protein",
            "nut_carbs",
            "nut_fat",
            "nut_vitc",
            "nut_iron",
        ];
        let ingredient_nutrients = amounts
            .iter()
            .flat_map(|(ing_id, per_100g)| {
                nutrient_ids
                    .iter()
                    .zip(per_100g)
                    .map(|(nut_id, amount)| IngredientNutrient {
                        ingredient_id: (*ing_id).into(),
                        nutrient_id: (*nut_id).into(),
                        amount_per_100g: *amount,
                    })
            })
            .collect();

        Self {
            meals: Vec::new(),
            ingredients: vec![
                ing("ing_chicken", "grilled chicken"),
                ing("ing_rice", "brown rice"),
                ing("ing_olive", "olive oil"),
                ing("ing_broccoli", "broccoli"),
                ing("ing_egg", "egg"),
                ing("ing_oats", "rolled oats"),
            ],
            component_mappings: vec![
                map("grilled chicken", "ing_chicken"),
                map("brown rice", "ing_rice"),
                map("olive oil", "ing_olive"),
                map("broccoli", "ing_broccoli"),
                map("egg", "ing_egg"),
                map("scrambled eggs", "ing_egg"),
                map("oatmeal", "ing_oats"),
                map("rolled oats", "ing_oats"),
            ],
            nutrients: vec![
                nut("nut_protein", "Protein", "macro", "g"),
                nut("nut_carbs", "Carbs", "macro", "g"),
                nut("nut_fat", "Fat", "macro", "g"),
                nut("nut_vitc", "Vitamin C", "micro", "mg"),
                nut("nut_iron", "Iron", "micro", "mg"),
            ],
            ingredient_nutrients,
        }
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The nutrition app, fully configured. Serve it with
/// `app().serve_with(with_api)` (standalone) or mount
/// `with_api(...app().build_parts()?)` in a multi-app host — `with_api` adds
/// the strategy-backed HTTP routes (`POST /api/log`, `GET /api/capabilities`)
/// on top of the derived surface.
pub fn app() -> App<Nutrition> {
    App::<Nutrition>::new("nutrition")
        .instructions(
            "A replicated nutrition tracker. Log meals with gram-quantified \
             components via log_meal; read totals with meal_nutrition. If a \
             component is unknown, register per-100g data with \
             add_component_nutrition (full panel) or add_ingredient (core \
             five) and past meals resolve too. Meals can also be logged from \
             a plain-language description via POST /api/log \
             {\"description\": \"1 cup rice and 200g chicken\"} when an \
             online NUTRITION_STRATEGY (calorieninjas | llm) is active. \
             Humans see every change live in the web UI on all synced \
             devices.",
        )
        .ui_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/ui"))
}

/// Merge the nutrition app's custom routes (description-based logging via
/// the active strategy + the capabilities probe) into the derived router.
/// Shaped to pass straight to [`App::serve_with`].
pub fn with_api(router: axum::Router, handle: Handle<Nutrition>) -> axum::Router {
    router.merge(api::routes(handle))
}
