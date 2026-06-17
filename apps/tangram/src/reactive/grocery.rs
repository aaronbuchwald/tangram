//! The Smart Objects **SO3 recipe golden-path** derive kinds
//! (`docs/design/smart-objects.md` §6): the reactive
//! `recipe → grocery-list → cart-preview` chain — the "document recalculates
//! itself" moment. Both kinds are pure, deterministic `fn(deps) -> data`
//! computations driven by SO2's reactivity engine
//! ([`super::compute_derived`]): on any object mutation the engine recomputes the
//! affected derived subgraph in topological order, so toggling/adding/removing a
//! recipe recomputes the grocery-list AND the cart-preview live, in the document.
//!
//! ## `recipe` (a definition type — not derived)
//!
//! A `recipe`'s `data` is JSON (manual entry for SO3; URL ingestion is SO4):
//!
//! ```json
//! {
//!   "name": "Tomato Pasta",
//!   "servings": 2,
//!   "ingredients": [
//!     { "canonicalName": "olive oil", "quantity": 2, "unit": "tbsp", "category": "Oils" },
//!     { "canonicalName": "onion", "quantity": 1, "unit": "ct", "category": "Produce" }
//!   ],
//!   "source": "https://…"   // optional
//! }
//! ```
//!
//! ## `grocery-list` (derived over its included `recipe` deps)
//!
//! Groups every ingredient line across the included recipes by
//! `canonicalName + reconciled-unit`, **sums quantity**, and collects the source
//! recipe names. **Unit reconciliation** is the core risk: units of the same
//! dimension (volume tsp→tbsp→cup, mass g→kg) are converted to a canonical unit
//! of that dimension and merged; units of *different* dimensions (or unknown
//! units) for the same ingredient are kept as **separate rows** (no guessing).
//! Output: `{ "rows": [{ "name", "quantity", "unit", "sources": [...] }] }`,
//! sorted by `(name, unit)` for determinism.
//!
//! ## `cart-preview` (derived over its `grocery-list` dep)
//!
//! Groups the grocery-list rows by `category` (the aisle), carrying each item's
//! quantity/unit. Output: `{ "aisles": [{ "category", "items": [...] }] }`,
//! sorted by category then item name. The terminus of Build-1.
//!
//! Determinism: stable sort orders + canonical-unit conversion + JSON object
//! construction in a fixed key sequence ⇒ byte-identical across replicas (the
//! engine runs at genesis too, so the seeded cache is part of the shared commit).

use std::collections::BTreeMap;

use serde::Deserialize;

use super::DepSnapshot;

/// The "no category" bucket label used when a recipe ingredient line carries no
/// (or a blank) `category` — so a cart-preview still places it on a shelf.
const UNCATEGORIZED: &str = "Other";

/// One ingredient line on a `recipe` (the manual-entry shape; SO3). `category` is
/// the aisle the cart-preview groups by; a blank/absent category falls back to
/// [`UNCATEGORIZED`].
#[derive(Deserialize)]
struct Ingredient {
    #[serde(default, rename = "canonicalName", alias = "name")]
    canonical_name: String,
    #[serde(default)]
    quantity: f64,
    #[serde(default)]
    unit: String,
    #[serde(default)]
    category: String,
}

/// The parsed `recipe` `data` payload (only the fields the derives read).
#[derive(Deserialize)]
struct Recipe {
    #[serde(default)]
    name: String,
    #[serde(default)]
    ingredients: Vec<Ingredient>,
}

/// One aggregated grocery row (the `grocery-list` output rows).
#[derive(Clone)]
struct GroceryRow {
    name: String,
    unit: String,
    quantity: f64,
    /// The (canonical) ingredient category carried through for the cart-preview.
    category: String,
    /// Source recipe names that contributed to this row (deduped, sorted).
    sources: Vec<String>,
}

/// The **`grocery-list`** derive (Smart Objects SO3): aggregate the ingredient
/// lines across the included `recipe` deps into deduped, unit-reconciled rows.
/// Pure + deterministic. A dep whose `data` is empty or not a recipe simply
/// contributes nothing (a freshly-created, not-yet-filled recipe should not
/// break the whole list).
pub fn grocery_list(deps: &[DepSnapshot]) -> Result<String, String> {
    // Group key: (canonicalName, reconciled-unit). A BTreeMap keeps the merge
    // order deterministic regardless of dep order.
    let mut groups: BTreeMap<(String, String), GroceryRow> = BTreeMap::new();

    for dep in deps {
        let raw = dep.data.trim();
        if raw.is_empty() {
            continue; // an unfilled recipe contributes nothing
        }
        let recipe: Recipe = match serde_json::from_str(raw) {
            Ok(r) => r,
            // A dep that isn't a recipe (bad/opaque data) is skipped, not fatal —
            // the grocery-list stays computable over the recipes that ARE valid.
            Err(_) => continue,
        };
        let source = recipe.name.trim();
        let source = if source.is_empty() {
            dep.id.clone()
        } else {
            source.to_string()
        };

        for ing in &recipe.ingredients {
            let name = ing.canonical_name.trim().to_string();
            if name.is_empty() {
                continue; // an unnamed ingredient line is dropped
            }
            // Reconcile the unit to the canonical unit of its dimension; an
            // unknown/blank unit stays as-is (lower-cased, trimmed) so it lands
            // on its own row (no guessing).
            let (unit, factor) = reconcile_unit(&ing.unit);
            let qty = ing.quantity * factor;
            let category = {
                let c = ing.category.trim();
                if c.is_empty() {
                    UNCATEGORIZED.to_string()
                } else {
                    c.to_string()
                }
            };

            let key = (name.clone(), unit.clone());
            let row = groups.entry(key).or_insert_with(|| GroceryRow {
                name,
                unit,
                quantity: 0.0,
                category: category.clone(),
                sources: Vec::new(),
            });
            row.quantity += qty;
            if !row.sources.contains(&source) {
                row.sources.push(source.clone());
            }
            // First non-`Other` category wins (so a categorized line names the
            // aisle even if another line for the same item omitted it).
            if row.category == UNCATEGORIZED && category != UNCATEGORIZED {
                row.category = category;
            }
        }
    }

    let mut rows: Vec<&GroceryRow> = groups.values().collect();
    // (name, unit) order — the BTreeMap already iterates in that order, but make
    // it explicit so the output ordering is independent of the map internals.
    rows.sort_by(|a, b| a.name.cmp(&b.name).then(a.unit.cmp(&b.unit)));

    let out_rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let mut sources = r.sources.clone();
            sources.sort();
            serde_json::json!({
                "name": r.name,
                "quantity": super::number_value(r.quantity),
                "unit": r.unit,
                "category": r.category,
                "sources": sources,
            })
        })
        .collect();

    Ok(serde_json::json!({ "rows": out_rows }).to_string())
}

/// The grocery-list output shape the `cart-preview` reads (only the fields it
/// needs). The grocery-list caches `{ "rows": [...] }`; cart-preview re-groups
/// those rows by aisle.
#[derive(Deserialize)]
struct GroceryListData {
    #[serde(default)]
    rows: Vec<GroceryListRow>,
}

#[derive(Deserialize, Clone)]
struct GroceryListRow {
    #[serde(default)]
    name: String,
    #[serde(default)]
    quantity: serde_json::Value,
    #[serde(default)]
    unit: String,
    #[serde(default)]
    category: String,
}

/// The **`cart-preview`** derive (Smart Objects SO3 — the terminus of Build-1):
/// group the grocery-list rows by `category` (aisle). It reads its
/// **grocery-list** dependency's cached `data` (computed earlier in the same
/// topological pass, so it is already fresh). Multiple grocery-list deps are
/// merged (defensive — the meal-plan wires exactly one). Pure + deterministic.
pub fn cart_preview(deps: &[DepSnapshot]) -> Result<String, String> {
    // Aisle → rows, BTreeMap-ordered (category name) for determinism.
    let mut aisles: BTreeMap<String, Vec<GroceryListRow>> = BTreeMap::new();

    for dep in deps {
        let raw = dep.data.trim();
        if raw.is_empty() {
            continue;
        }
        let list: GroceryListData = match serde_json::from_str(raw) {
            Ok(l) => l,
            // A dep that is not a grocery-list (wrong wiring) contributes nothing.
            Err(_) => continue,
        };
        for row in list.rows {
            let category = {
                let c = row.category.trim();
                if c.is_empty() {
                    UNCATEGORIZED.to_string()
                } else {
                    c.to_string()
                }
            };
            aisles.entry(category).or_default().push(row);
        }
    }

    let out_aisles: Vec<serde_json::Value> = aisles
        .into_iter()
        .map(|(category, mut items)| {
            items.sort_by(|a, b| a.name.cmp(&b.name).then(a.unit.cmp(&b.unit)));
            let items: Vec<serde_json::Value> = items
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "name": r.name,
                        "quantity": r.quantity,
                        "unit": r.unit,
                    })
                })
                .collect();
            serde_json::json!({ "category": category, "items": items })
        })
        .collect();

    Ok(serde_json::json!({ "aisles": out_aisles }).to_string())
}

/// Reconcile an ingredient `unit` to the **canonical unit of its dimension** plus
/// the multiplier that converts the input quantity into that canonical unit, so
/// compatible units (e.g. `tsp` and `tbsp`) merge into one summed row. An
/// **unknown / blank** unit is its own canonical unit with factor `1.0`, so it
/// lands on a separate row (the no-guessing rule — we never merge units we don't
/// recognise as the same dimension).
///
/// Dimensions + canonical units (chosen as the mid-scale, human unit):
/// - **volume** → `tbsp`: `tsp`=1/3, `tbsp`=1, `cup`=16 (16 tbsp/cup).
/// - **mass** → `g`: `g`=1, `kg`=1000, `mg`=0.001.
/// - **count** → `ct`: `ct`/`count`/`piece`/`pcs`/`pc`=1.
///
/// Unit names are matched case-insensitively and with a trailing `s` (plural)
/// tolerated, so `Tbsp`, `tablespoons`, `Cups` all reconcile.
fn reconcile_unit(unit: &str) -> (String, f64) {
    let u = unit.trim().to_lowercase();
    // Tolerate a trailing plural `s` and a couple of common long forms.
    let key = match u.as_str() {
        "teaspoon" | "teaspoons" | "tsps" => "tsp",
        "tablespoon" | "tablespoons" | "tbsps" | "tbs" => "tbsp",
        "cups" => "cup",
        "grams" | "gram" => "g",
        "kilogram" | "kilograms" | "kgs" => "kg",
        "milligram" | "milligrams" | "mgs" => "mg",
        "count" | "counts" | "piece" | "pieces" | "pcs" | "pc" | "cts" => "ct",
        other => other,
    };
    match key {
        // volume → tbsp
        "tsp" => ("tbsp".to_string(), 1.0 / 3.0),
        "tbsp" => ("tbsp".to_string(), 1.0),
        "cup" => ("tbsp".to_string(), 16.0),
        // mass → g
        "g" => ("g".to_string(), 1.0),
        "kg" => ("g".to_string(), 1000.0),
        "mg" => ("g".to_string(), 0.001),
        // count → ct
        "ct" => ("ct".to_string(), 1.0),
        // unknown / blank → its own row, factor 1 (no guessing).
        "" => (String::new(), 1.0),
        other => (other.to_string(), 1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// A recipe dep snapshot from a name + JSON ingredient lines.
    fn recipe(id: &str, name: &str, ingredients: Value) -> DepSnapshot {
        DepSnapshot {
            id: id.to_string(),
            data: serde_json::json!({ "name": name, "servings": 1, "ingredients": ingredients })
                .to_string(),
        }
    }

    fn ing(name: &str, qty: f64, unit: &str, category: &str) -> Value {
        serde_json::json!({ "canonicalName": name, "quantity": qty, "unit": unit, "category": category })
    }

    /// Find a grocery row by (name, unit) in the computed grocery-list JSON.
    fn row<'a>(v: &'a Value, name: &str, unit: &str) -> &'a Value {
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name && r["unit"] == unit)
            .unwrap_or_else(|| panic!("no row {name}/{unit} in {v}"))
    }

    #[test]
    fn merges_same_ingredient_across_recipes() {
        // olive oil 2tbsp (recipe A) + 2tbsp (recipe B) ⇒ 4tbsp, two sources.
        let deps = vec![
            recipe(
                "ra",
                "Pasta",
                serde_json::json!([ing("olive oil", 2.0, "tbsp", "Oils")]),
            ),
            recipe(
                "rb",
                "Salad",
                serde_json::json!([ing("olive oil", 2.0, "tbsp", "Oils")]),
            ),
        ];
        let out: Value = serde_json::from_str(&grocery_list(&deps).unwrap()).unwrap();
        let oil = row(&out, "olive oil", "tbsp");
        assert_eq!(oil["quantity"], 4);
        assert_eq!(oil["sources"], serde_json::json!(["Pasta", "Salad"]));
    }

    #[test]
    fn onion_and_tomato_merge_across_two_recipes_each() {
        let deps = vec![
            recipe(
                "ra",
                "Pasta",
                serde_json::json!([
                    ing("onion", 1.0, "ct", "Produce"),
                    ing("tomato", 2.0, "ct", "Produce")
                ]),
            ),
            recipe(
                "rb",
                "Soup",
                serde_json::json!([
                    ing("onion", 2.0, "ct", "Produce"),
                    ing("tomato", 1.0, "ct", "Produce")
                ]),
            ),
        ];
        let out: Value = serde_json::from_str(&grocery_list(&deps).unwrap()).unwrap();
        assert_eq!(row(&out, "onion", "ct")["quantity"], 3);
        assert_eq!(row(&out, "tomato", "ct")["quantity"], 3);
        assert_eq!(
            row(&out, "onion", "ct")["sources"],
            serde_json::json!(["Pasta", "Soup"])
        );
    }

    #[test]
    fn compatible_units_reconcile_and_sum_into_one_row() {
        // 1 tbsp + 3 tsp (= 1 tbsp) ⇒ 2 tbsp on a single reconciled row.
        let deps = vec![
            recipe(
                "ra",
                "A",
                serde_json::json!([ing("flour", 1.0, "tbsp", "Baking")]),
            ),
            recipe(
                "rb",
                "B",
                serde_json::json!([ing("flour", 3.0, "tsp", "Baking")]),
            ),
        ];
        let out: Value = serde_json::from_str(&grocery_list(&deps).unwrap()).unwrap();
        // Exactly one flour row, reconciled to tbsp, summing to 2.
        let flour_rows: Vec<&Value> = out["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["name"] == "flour")
            .collect();
        assert_eq!(flour_rows.len(), 1, "tsp+tbsp must reconcile to ONE row");
        assert_eq!(flour_rows[0]["unit"], "tbsp");
        assert_eq!(flour_rows[0]["quantity"], 2);
    }

    #[test]
    fn incompatible_units_stay_separate_rows() {
        // "garlic" in grams vs count — different dimensions ⇒ TWO rows (no guess).
        let deps = vec![
            recipe(
                "ra",
                "A",
                serde_json::json!([ing("garlic", 10.0, "g", "Produce")]),
            ),
            recipe(
                "rb",
                "B",
                serde_json::json!([ing("garlic", 2.0, "ct", "Produce")]),
            ),
        ];
        let out: Value = serde_json::from_str(&grocery_list(&deps).unwrap()).unwrap();
        let garlic_rows: Vec<&Value> = out["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["name"] == "garlic")
            .collect();
        assert_eq!(garlic_rows.len(), 2, "incompatible units stay separate");
        assert_eq!(row(&out, "garlic", "g")["quantity"], 10);
        assert_eq!(row(&out, "garlic", "ct")["quantity"], 2);
    }

    #[test]
    fn cup_reconciles_to_tbsp() {
        let deps = vec![recipe(
            "ra",
            "A",
            serde_json::json!([ing("milk", 1.0, "cup", "Dairy")]),
        )];
        let out: Value = serde_json::from_str(&grocery_list(&deps).unwrap()).unwrap();
        // 1 cup = 16 tbsp.
        assert_eq!(row(&out, "milk", "tbsp")["quantity"], 16);
    }

    #[test]
    fn plural_and_case_units_reconcile() {
        let deps = vec![
            recipe(
                "ra",
                "A",
                serde_json::json!([ing("sugar", 1.0, "Tbsp", "Baking")]),
            ),
            recipe(
                "rb",
                "B",
                serde_json::json!([ing("sugar", 1.0, "tablespoons", "Baking")]),
            ),
        ];
        let out: Value = serde_json::from_str(&grocery_list(&deps).unwrap()).unwrap();
        assert_eq!(row(&out, "sugar", "tbsp")["quantity"], 2);
    }

    #[test]
    fn empty_and_nonrecipe_deps_are_skipped_not_fatal() {
        let deps = vec![
            DepSnapshot {
                id: "blank".into(),
                data: String::new(),
            },
            DepSnapshot {
                id: "junk".into(),
                data: "not json".into(),
            },
            recipe(
                "ra",
                "A",
                serde_json::json!([ing("salt", 1.0, "tsp", "Pantry")]),
            ),
        ];
        let out: Value = serde_json::from_str(&grocery_list(&deps).unwrap()).unwrap();
        assert_eq!(out["rows"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn cart_preview_groups_rows_by_aisle() {
        // Build a grocery-list payload, then group it into aisles.
        let grocery = serde_json::json!({
            "rows": [
                { "name": "onion", "quantity": 3, "unit": "ct", "category": "Produce" },
                { "name": "olive oil", "quantity": 4, "unit": "tbsp", "category": "Oils" },
                { "name": "tomato", "quantity": 3, "unit": "ct", "category": "Produce" },
            ]
        })
        .to_string();
        let deps = vec![DepSnapshot {
            id: "gl".into(),
            data: grocery,
        }];
        let out: Value = serde_json::from_str(&cart_preview(&deps).unwrap()).unwrap();
        let aisles = out["aisles"].as_array().unwrap();
        // Two aisles: Oils, Produce (alphabetical), Produce holds onion+tomato.
        assert_eq!(aisles.len(), 2);
        assert_eq!(aisles[0]["category"], "Oils");
        assert_eq!(aisles[1]["category"], "Produce");
        let produce = aisles[1]["items"].as_array().unwrap();
        assert_eq!(produce.len(), 2);
        // Items sorted by name within the aisle.
        assert_eq!(produce[0]["name"], "onion");
        assert_eq!(produce[1]["name"], "tomato");
    }

    #[test]
    fn uncategorized_ingredients_land_in_other_aisle() {
        let deps = vec![recipe(
            "ra",
            "A",
            serde_json::json!([ing("mystery", 1.0, "ct", "")]),
        )];
        let gl: Value = serde_json::from_str(&grocery_list(&deps).unwrap()).unwrap();
        assert_eq!(row(&gl, "mystery", "ct")["category"], "Other");
        let cart: Value = serde_json::from_str(
            &cart_preview(&[DepSnapshot {
                id: "gl".into(),
                data: gl.to_string(),
            }])
            .unwrap(),
        )
        .unwrap();
        assert_eq!(cart["aisles"][0]["category"], "Other");
    }
}
