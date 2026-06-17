//! Smart Objects **SO4 — recipe URL ingestion** (`docs/design/smart-objects.md`
//! §6). Turn a pasted recipe URL into a normalized `recipe` smart object that
//! flows into the SO3 reactive `grocery → cart` chain.
//!
//! ## The pipeline (and where each step runs)
//!
//! ```text
//!  paste URL ─▶ HOST fetch (gated)   ─▶ extract JSON-LD ─▶ LLM normalize ─▶ recipe object
//!              /recipe/fetch                (this mod,        (this mod,       (create_object,
//!              tangram-host/src/recipe.rs    pure)            DeepSeek)         lib.rs)
//! ```
//!
//! * **Fetch** is HOST-mediated (a component cannot fetch arbitrary URLs — closed
//!   egress allow-list): the component GETs `127.0.0.1/recipe/fetch?url=…` over
//!   loopback; the host fetches the page through the `tangram-automation` egress
//!   gate and returns the HTML (ADR-0010; `tangram-host/src/recipe.rs`).
//! * **Extract** ([`extract_recipe_jsonld`]) pulls the schema.org/Recipe JSON-LD
//!   from the page — pure, handles `@graph`, top-level arrays, and multiple
//!   `<script type="application/ld+json">` blocks. The no-JSON-LD fallback
//!   (LLM-parse the visible page) is a documented STUB ([`Fallback`]).
//! * **Normalize** ([`Llm`]) is the core risk: each free-text `recipeIngredient`
//!   ("2 tbsp olive oil") → `{canonicalName, quantity, unit, category, raw}` via
//!   DeepSeek, with a small [`canonicalize`] dictionary keeping `tomato`/
//!   `tomatoes`, `scallion`/`green onion` from fragmenting. The LLM call has a
//!   FIXTURE seam ([`Llm::Fixture`]) so CI is offline + deterministic.
//! * **Cache** by `URL + JSON-LD hash` ([`cache_key`]) so a re-import is free.
//!
//! Everything here except [`Llm::Live`]/`fetch_*` is pure + deterministic and
//! unit-tested with NO network (the fixture-LLM precedent: guided-learning /
//! morning-brief `input_mode "fixture"`).

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tangram::http;

/// The normalized `recipe` object `data` shape (the SO3 chain reads it; SO4
/// produces it). `source` carries the import URL; `ingredients` are the
/// canonicalized lines the `grocery-list` derive aggregates over.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NormalizedRecipe {
    pub name: String,
    pub servings: u32,
    pub ingredients: Vec<NormalizedIngredient>,
    pub source: String,
}

/// One normalized ingredient line. `canonical_name`/`quantity`/`unit`/`category`
/// are what the SO3 derives consume; `raw` preserves the original free-text line
/// (provenance + a manual-fix anchor). Serialized to the SO3 ingredient shape
/// (`canonicalName`, …).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NormalizedIngredient {
    #[serde(rename = "canonicalName")]
    pub canonical_name: String,
    pub quantity: f64,
    pub unit: String,
    pub category: String,
    /// The original free-text recipe line ("2 tbsp olive oil"). Provenance.
    #[serde(default)]
    pub raw: String,
}

/// A schema.org/Recipe extracted from a page's JSON-LD: the recipe name,
/// servings (best-effort), and the RAW free-text ingredient lines (to be
/// LLM-normalized). The intermediate the extract step produces and the normalize
/// step consumes.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtractedRecipe {
    pub name: String,
    pub servings: u32,
    pub raw_ingredients: Vec<String>,
}

// ── JSON-LD extraction ────────────────────────────────────────────────────

/// Extract the schema.org/Recipe from a page's HTML JSON-LD blocks. Returns the
/// recipe name + raw ingredient lines for normalization. `None` when the page
/// has no `Recipe` JSON-LD (the caller then takes the [`Fallback`] path).
///
/// Handles the real-world shapes:
/// * multiple `<script type="application/ld+json">` blocks,
/// * a top-level JSON ARRAY of objects,
/// * an `@graph` wrapper (Yoast/WordPress emit this),
/// * `@type` as a string OR an array (`["Recipe", "NewsArticle"]`).
pub fn extract_recipe_jsonld(html: &str) -> Option<ExtractedRecipe> {
    for block in jsonld_blocks(html) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&block) else {
            continue; // a malformed block is skipped, not fatal
        };
        if let Some(recipe) = find_recipe_node(&value) {
            return parse_recipe_node(recipe);
        }
    }
    None
}

/// The raw text of every `<script type="application/ld+json">…</script>` block.
/// A tolerant scan (not a full HTML parse — we only need the JSON-LD islands):
/// case-insensitive tag match, attribute order-independent.
fn jsonld_blocks(html: &str) -> Vec<String> {
    let lower = html.to_lowercase();
    let mut blocks = Vec::new();
    let mut search_from = 0usize;
    while let Some(rel) = lower[search_from..].find("<script") {
        let tag_start = search_from + rel;
        // The end of the opening tag.
        let Some(rel_gt) = lower[tag_start..].find('>') else {
            break;
        };
        let open_end = tag_start + rel_gt + 1;
        let open_tag = &lower[tag_start..open_end];
        // Only ld+json script blocks.
        if open_tag.contains("application/ld+json")
            && let Some(rel_close) = lower[open_end..].find("</script")
        {
            let content_end = open_end + rel_close;
            blocks.push(html[open_end..content_end].trim().to_string());
            search_from = content_end;
            continue;
        }
        search_from = open_end;
    }
    blocks
}

/// Find the first schema.org/Recipe node within a parsed JSON-LD value, drilling
/// through a top-level array and an `@graph` wrapper.
fn find_recipe_node(value: &serde_json::Value) -> Option<&serde_json::Value> {
    match value {
        serde_json::Value::Array(items) => items.iter().find_map(find_recipe_node),
        serde_json::Value::Object(map) => {
            if is_recipe_type(map.get("@type")) {
                return Some(value);
            }
            // `@graph`: a flat list of typed nodes — search it.
            if let Some(graph) = map.get("@graph") {
                return find_recipe_node(graph);
            }
            None
        }
        _ => None,
    }
}

/// Whether an `@type` value (string or array) names `Recipe` (case-insensitive).
fn is_recipe_type(ty: Option<&serde_json::Value>) -> bool {
    match ty {
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("recipe"),
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .any(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("recipe"))),
        _ => false,
    }
}

/// Parse a confirmed Recipe node into the [`ExtractedRecipe`] intermediate. A
/// node with no usable `recipeIngredient` array yields `None` (treated as "no
/// recipe found" so the fallback path can take over).
fn parse_recipe_node(node: &serde_json::Value) -> Option<ExtractedRecipe> {
    let name = node
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let raw_ingredients: Vec<String> = node
        .get("recipeIngredient")
        // schema.org also allows the (deprecated) `ingredients` key.
        .or_else(|| node.get("ingredients"))
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    if raw_ingredients.is_empty() {
        return None;
    }

    Some(ExtractedRecipe {
        name,
        servings: parse_servings(node.get("recipeYield")),
        raw_ingredients,
    })
}

/// Best-effort `recipeYield` → a servings count. The field is wildly
/// inconsistent across sites (`4`, `"4 servings"`, `["4", "4 servings"]`), so we
/// take the first integer we can find; default 1 (so per-serving math never
/// divides by zero downstream).
fn parse_servings(yield_value: Option<&serde_json::Value>) -> u32 {
    fn first_int(s: &str) -> Option<u32> {
        let digits: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok().filter(|&n: &u32| n > 0)
    }
    match yield_value {
        Some(serde_json::Value::Number(n)) => n.as_u64().map(|n| n as u32),
        Some(serde_json::Value::String(s)) => first_int(s.trim()),
        Some(serde_json::Value::Array(items)) => items.iter().find_map(|v| match v {
            serde_json::Value::Number(n) => n.as_u64().map(|n| n as u32),
            serde_json::Value::String(s) => first_int(s.trim()),
            _ => None,
        }),
        _ => None,
    }
    .filter(|&n| n > 0)
    .unwrap_or(1)
}

// ── canonical dictionary ──────────────────────────────────────────────────

/// Collapse common ingredient-name variants onto ONE canonical name so the SO3
/// grocery-list does not fragment (`tomato`/`tomatoes`, `scallion`/`green
/// onion`). Small + deterministic on purpose — the LLM does the bulk of the
/// parsing; this dictionary is the post-pass that keeps a known synonym pair
/// from splitting into two grocery rows. Lower-cased, trimmed, trailing-`s`
/// tolerated. Applied AFTER the LLM (so even a divergent LLM canonicalization
/// re-converges on these known pairs).
pub fn canonicalize(name: &str) -> String {
    let n = name.trim().to_lowercase();
    // Tolerate a trailing plural `s` for the lookup (but keep singular output).
    let singular = n.strip_suffix('s').unwrap_or(&n);
    match singular {
        "tomato" | "tomatoe" => "tomato",
        "onion" => "onion",
        "scallion" | "green onion" | "spring onion" => "scallion",
        "garlic" | "garlic clove" | "clove of garlic" => "garlic",
        "pepper" | "bell pepper" | "capsicum" => "bell pepper",
        "cilantro" | "coriander leaf" | "fresh coriander" => "cilantro",
        "chickpea" | "garbanzo" | "garbanzo bean" => "chickpea",
        "scallop" => "scallop",
        "eggplant" | "aubergine" => "eggplant",
        "zucchini" | "courgette" => "zucchini",
        "shrimp" | "prawn" => "shrimp",
        "ground beef" | "minced beef" | "beef mince" => "ground beef",
        _ => return n, // unknown → the LLM's name, lower-cased + trimmed
    }
    .to_string()
}

// ── the URL+JSON-LD cache key ─────────────────────────────────────────────

/// The cache key for an import: `URL + sha256(extracted-recipe)`. A re-import of
/// the same URL whose page content is unchanged hits the cache (free); a changed
/// page (new JSON-LD ⇒ new hash) re-normalizes. Mirrors the design's
/// "cache by URL + JSON-LD hash" so re-import is a no-op.
pub fn cache_key(url: &str, extracted: &ExtractedRecipe) -> String {
    let mut hasher = Sha256::new();
    hasher.update(url.trim().as_bytes());
    hasher.update(b"\0");
    hasher.update(extracted.name.as_bytes());
    hasher.update(b"\0");
    for ing in &extracted.raw_ingredients {
        hasher.update(ing.as_bytes());
        hasher.update(b"\n");
    }
    format!("{:x}", hasher.finalize())
}

// ── LLM normalization (the core) ──────────────────────────────────────────

/// The LLM normalization seam — the fixture/live split that makes ingestion
/// CI-offline + deterministic (the morning-brief `Llm::Fixture`/`Live`
/// precedent). [`Llm::Fixture`] returns a CANNED normalization with ZERO network
/// (CI); [`Llm::Live`] issues the real DeepSeek call (the same host-injected-key
/// egress the agent run uses).
pub enum Llm {
    /// Offline / CI: parse a CANNED LLM response (no network). The canned text
    /// is supplied so a test can drive a recorded DeepSeek reply through the
    /// SAME parse + canonicalization path the live call uses. Constructed only
    /// by tests + a future fixture-mode action arg (the live path uses `Live`).
    #[cfg_attr(not(test), allow(dead_code))]
    Fixture(String),
    /// Live: POST the normalization prompt to DeepSeek (host-injected key).
    Live { model: String },
}

impl Llm {
    /// Normalize the extracted recipe's free-text ingredient lines into the SO3
    /// `{canonicalName, quantity, unit, category, raw}` shape. Returns the full
    /// [`NormalizedRecipe`] (name/servings carried through, `source` = `url`).
    pub async fn normalize(
        &self,
        extracted: &ExtractedRecipe,
        url: &str,
    ) -> Result<NormalizedRecipe, String> {
        let raw_text = match self {
            Llm::Fixture(canned) => canned.clone(),
            Llm::Live { model } => {
                let prompt = build_normalization_prompt(extracted);
                normalize_live(model, &prompt).await?
            }
        };
        let lines = parse_normalized_response(&raw_text, &extracted.raw_ingredients)?;
        Ok(NormalizedRecipe {
            name: extracted.name.clone(),
            servings: extracted.servings,
            ingredients: lines,
            source: url.trim().to_string(),
        })
    }
}

/// Build the normalization prompt: ask the model to turn each free-text line into
/// a strict JSON object. The instruction pins the output schema + the unit/
/// category vocabulary the SO3 derives understand, so the response parses
/// deterministically. (System framing kept terse — DeepSeek follows JSON-mode
/// instructions well; the parse below is also tolerant of fenced JSON.)
pub fn build_normalization_prompt(extracted: &ExtractedRecipe) -> String {
    let lines = extracted
        .raw_ingredients
        .iter()
        .enumerate()
        .map(|(i, l)| format!("{}. {l}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You normalize recipe ingredient lines into structured grocery data.\n\
         For EACH numbered line below, output one JSON object with EXACTLY these keys:\n\
         - \"canonicalName\": the base ingredient, singular, lower-case (e.g. \"olive oil\", \
         \"onion\", \"tomato\"). Strip preparation words (\"diced\", \"chopped\", \"to taste\").\n\
         - \"quantity\": a number (decimal ok; convert fractions like 1/2 to 0.5; use 1 if none).\n\
         - \"unit\": one of tsp, tbsp, cup, g, kg, mg, ml, l, oz, lb, ct (use \"ct\" for whole \
         items like \"1 onion\"; \"\" if truly unitless).\n\
         - \"category\": the grocery aisle (Produce, Dairy, Meat, Pantry, Bakery, Oils, Spices, \
         Frozen, Other).\n\
         Output ONLY a JSON array of these objects, in the same order as the input lines, \
         no prose.\n\n\
         Recipe: {}\n\nIngredient lines:\n{lines}\n",
        extracted.name
    )
}

/// Parse the model's normalization response into ingredient lines, then apply
/// the canonical dictionary post-pass + attach the original `raw` line. Tolerant
/// of a fenced ```json block. The `raw_lines` are zipped in by position (the
/// prompt pins the same order); a length mismatch falls back to no `raw`.
pub fn parse_normalized_response(
    text: &str,
    raw_lines: &[String],
) -> Result<Vec<NormalizedIngredient>, String> {
    let json = extract_json_array(text)
        .ok_or_else(|| "LLM response did not contain a JSON array".to_string())?;
    let mut parsed: Vec<NormalizedIngredient> =
        serde_json::from_str(&json).map_err(|e| format!("LLM JSON did not match schema: {e}"))?;

    for (i, ing) in parsed.iter_mut().enumerate() {
        // The canonical dictionary post-pass keeps known synonyms from
        // fragmenting even if the LLM diverged.
        ing.canonical_name = canonicalize(&ing.canonical_name);
        ing.unit = ing.unit.trim().to_lowercase();
        ing.category = ing.category.trim().to_string();
        // Attach the original free-text line by position (the prompt pins the
        // same order); a length mismatch simply leaves `raw` blank.
        if let Some(raw) = raw_lines.get(i) {
            ing.raw = raw.clone();
        }
    }
    Ok(parsed)
}

/// Pull the first top-level JSON array out of a model response, tolerating a
/// surrounding ```json fence or leading prose. Returns the `[...]` slice.
fn extract_json_array(text: &str) -> Option<String> {
    let t = text.trim();
    // Strip a ```json … ``` fence if present.
    let inner = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .map(|rest| rest.trim_start())
        .and_then(|rest| rest.strip_suffix("```").map(str::trim))
        .unwrap_or(t);
    let start = inner.find('[')?;
    let end = inner.rfind(']')?;
    if end <= start {
        return None;
    }
    Some(inner[start..=end].to_string())
}

/// The DeepSeek chat-completions URL (shared with the agent run): default
/// DeepSeek, overridable to a fixture authority via `TANGRAM_AGENT_LLM_AUTHORITY`
/// (the live config grants only the real DeepSeek host; a test points it at a
/// loopback fixture). Mirrors `crate::agent_llm_url`.
fn llm_url() -> String {
    match std::env::var("TANGRAM_AGENT_LLM_AUTHORITY")
        .ok()
        .filter(|a| !a.trim().is_empty())
    {
        Some(authority) => format!("http://{authority}/v1/chat/completions"),
        None => "https://api.deepseek.com/v1/chat/completions".to_string(),
    }
}

/// Issue the live DeepSeek normalization call. The request carries NO API key —
/// the HOST injects the DeepSeek bearer at the component's http-fetch egress
/// boundary (ADR-0005), so the key never enters the component. Returns the
/// assistant message text (the JSON array the parser reads).
async fn normalize_live(model: &str, prompt: &str) -> Result<String, String> {
    let body = serde_json::json!({
        "model": model,
        "messages": [
            { "role": "user", "content": prompt }
        ],
    });
    let req = http::Request::post(llm_url()).json(&body);
    let resp = http::fetch(req).await.map_err(|e| e.to_string())?;
    if !resp.is_success() {
        return Err(format!("DeepSeek normalization failed ({})", resp.status));
    }
    let payload: serde_json::Value = resp.json().map_err(|e| e.to_string())?;
    payload
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("DeepSeek normalization had no content: {payload}"))
}

// ── the host-mediated fetch (the loopback choke point) ─────────────────────

/// The loopback authority the component reaches the host's `/recipe/fetch`
/// route under. Reuses the SAME `TANGRAM_MCP_AUTHORITY` env the agent tool-loop
/// uses (the host's public bind), defaulting to the host's default bind. It is an
/// AUTHORITY (no scheme), for the same reason the MCP base is — a value with
/// `://` is read by the host secret resolver as a `scheme://locator` ref.
#[cfg(not(test))]
fn recipe_fetch_url(url: &str) -> String {
    let authority = std::env::var("TANGRAM_MCP_AUTHORITY")
        .ok()
        .filter(|a| !a.trim().is_empty())
        .unwrap_or_else(|| "127.0.0.1:8080".to_string());
    format!(
        "http://{authority}/recipe/fetch?url={}",
        http::urlencode(url)
    )
}

/// Fetch a recipe page's HTML via the HOST's `/recipe/fetch` route (the
/// host-mediated, egress-gated fetch — a component cannot fetch arbitrary URLs).
/// Returns the page HTML. The live path only; tests drive the pure extract +
/// fixture-LLM path directly (no network).
#[cfg(not(test))]
pub async fn fetch_page_html(url: &str) -> Result<String, String> {
    let req = http::Request::get(recipe_fetch_url(url));
    let resp = http::fetch(req).await.map_err(|e| e.to_string())?;
    if !resp.is_success() {
        let detail = String::from_utf8_lossy(&resp.body);
        return Err(format!(
            "host recipe fetch failed ({}): {}",
            resp.status,
            detail.trim()
        ));
    }
    Ok(String::from_utf8_lossy(&resp.body).into_owned())
}

/// Test build: ingestion's host fetch is never exercised in unit tests (it needs
/// a live host route); the pure extract + fixture-LLM path is tested directly.
/// This stub keeps the `ingest_recipe` action compiling in the I/O-free test
/// build (mirroring the `run_tool_loop` test stub).
#[cfg(test)]
pub async fn fetch_page_html(_url: &str) -> Result<String, String> {
    Err("recipe page fetch is not exercised in unit tests (needs the host route)".to_string())
}

/// The no-JSON-LD **fallback** (DEFERRED STUB, SO4 scope guard): when a page
/// carries no schema.org/Recipe JSON-LD, the design calls for an LLM-parse of the
/// visible page text. SO4 wires the SEAM but does not implement the live page
/// LLM-parse — it returns a clear, actionable error so the user can paste a
/// JSON-LD-bearing URL or enter the recipe manually (the SO3 path). A later
/// change implements `Fallback::LlmParsePage` here (and can route the host fetch
/// through the `tangram-automation` browser-driver for JS-rendered pages first).
pub struct Fallback;

impl Fallback {
    /// The implemented behavior for SO4: report no JSON-LD found (with guidance).
    /// A later change implements the live LLM page-parse here.
    pub fn message() -> String {
        "no schema.org/Recipe data found on that page (the LLM page-parse fallback \
         is not yet implemented). Try a URL from a major recipe site (most embed \
         JSON-LD), or add the recipe manually via the @recipe object."
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASTA_JSONLD: &str = r#"
    <html><head>
    <script type="application/ld+json">
    {
      "@context": "https://schema.org",
      "@type": "Recipe",
      "name": "Tomato Pasta",
      "recipeYield": "4 servings",
      "recipeIngredient": [
        "2 tbsp olive oil",
        "1 large onion, diced",
        "3 tomatoes, chopped"
      ]
    }
    </script>
    </head><body>...</body></html>
    "#;

    #[test]
    fn extracts_recipe_from_simple_jsonld() {
        let r = extract_recipe_jsonld(PASTA_JSONLD).expect("a Recipe node");
        assert_eq!(r.name, "Tomato Pasta");
        assert_eq!(r.servings, 4);
        assert_eq!(r.raw_ingredients.len(), 3);
        assert_eq!(r.raw_ingredients[0], "2 tbsp olive oil");
    }

    #[test]
    fn extracts_recipe_from_graph_wrapper() {
        // Yoast/WordPress shape: an @graph array of typed nodes.
        let html = r#"
        <script type="application/ld+json">
        { "@context":"https://schema.org",
          "@graph":[
            {"@type":"WebPage","name":"page"},
            {"@type":["Recipe","Thing"],"name":"Soup",
             "recipeYield":2,
             "recipeIngredient":["1 onion","2 cups stock"]}
          ]}
        </script>"#;
        let r = extract_recipe_jsonld(html).expect("a Recipe in @graph");
        assert_eq!(r.name, "Soup");
        assert_eq!(r.servings, 2);
        assert_eq!(r.raw_ingredients, vec!["1 onion", "2 cups stock"]);
    }

    #[test]
    fn extracts_recipe_from_multiple_blocks_and_top_level_array() {
        // First block is not a recipe; second block is a top-level ARRAY holding one.
        let html = r#"
        <script type="application/ld+json">{"@type":"Organization","name":"Site"}</script>
        <script type="application/ld+json">
        [{"@type":"BreadcrumbList"},
         {"@type":"Recipe","name":"Salad","recipeIngredient":["1 head lettuce"]}]
        </script>"#;
        let r = extract_recipe_jsonld(html).expect("recipe in the second block's array");
        assert_eq!(r.name, "Salad");
    }

    #[test]
    fn no_jsonld_returns_none() {
        assert!(extract_recipe_jsonld("<html><body>no structured data</body></html>").is_none());
        // A recipe with no ingredients is treated as "not found" (fallback path).
        let html = r#"<script type="application/ld+json">
            {"@type":"Recipe","name":"Empty"}</script>"#;
        assert!(extract_recipe_jsonld(html).is_none());
    }

    #[test]
    fn malformed_block_is_skipped_not_fatal() {
        let html = r#"
        <script type="application/ld+json">{ this is not json </script>
        <script type="application/ld+json">
        {"@type":"Recipe","name":"OK","recipeIngredient":["1 egg"]}</script>"#;
        let r = extract_recipe_jsonld(html).expect("the valid block still parses");
        assert_eq!(r.name, "OK");
    }

    #[test]
    fn canonical_dictionary_collapses_synonyms() {
        assert_eq!(canonicalize("Tomatoes"), "tomato");
        assert_eq!(canonicalize("tomato"), "tomato");
        assert_eq!(canonicalize("Green Onion"), "scallion");
        assert_eq!(canonicalize("scallions"), "scallion");
        assert_eq!(canonicalize("garbanzo beans"), "chickpea");
        // An unknown name is just lower-cased + trimmed (not invented).
        assert_eq!(canonicalize("  Saffron "), "saffron");
    }

    #[test]
    fn parses_normalized_response_and_attaches_raw() {
        let raw = vec![
            "2 tbsp olive oil".to_string(),
            "1 large onion, diced".to_string(),
        ];
        let llm = r#"[
          {"canonicalName":"olive oil","quantity":2,"unit":"tbsp","category":"Oils"},
          {"canonicalName":"onions","quantity":1,"unit":"ct","category":"Produce"}
        ]"#;
        let out = parse_normalized_response(llm, &raw).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].canonical_name, "olive oil");
        assert_eq!(out[0].unit, "tbsp");
        assert_eq!(out[0].raw, "2 tbsp olive oil");
        // The dictionary post-pass collapsed "onions" → "onion".
        assert_eq!(out[1].canonical_name, "onion");
        assert_eq!(out[1].raw, "1 large onion, diced");
    }

    #[test]
    fn parses_fenced_json_response() {
        let raw = vec!["1 egg".to_string()];
        let llm = "```json\n[{\"canonicalName\":\"egg\",\"quantity\":1,\"unit\":\"ct\",\"category\":\"Dairy\"}]\n```";
        let out = parse_normalized_response(llm, &raw).unwrap();
        assert_eq!(out[0].canonical_name, "egg");
    }

    #[test]
    fn bad_llm_response_is_an_error() {
        assert!(parse_normalized_response("sorry, I can't help", &[]).is_err());
    }

    #[tokio::test]
    async fn fixture_llm_normalizes_end_to_end() {
        // The fixture path drives a RECORDED DeepSeek reply through the same
        // parse + canonicalize path the live call uses — CI-offline, deterministic.
        let extracted = extract_recipe_jsonld(PASTA_JSONLD).unwrap();
        let canned = r#"[
          {"canonicalName":"olive oil","quantity":2,"unit":"tbsp","category":"Oils"},
          {"canonicalName":"onion","quantity":1,"unit":"ct","category":"Produce"},
          {"canonicalName":"tomatoes","quantity":3,"unit":"ct","category":"Produce"}
        ]"#;
        let recipe = Llm::Fixture(canned.to_string())
            .normalize(&extracted, "https://example.com/pasta")
            .await
            .unwrap();
        assert_eq!(recipe.name, "Tomato Pasta");
        assert_eq!(recipe.servings, 4);
        assert_eq!(recipe.source, "https://example.com/pasta");
        assert_eq!(recipe.ingredients.len(), 3);
        // The synonym collapsed so it won't fragment the grocery-list.
        assert_eq!(recipe.ingredients[2].canonical_name, "tomato");
        assert_eq!(recipe.ingredients[2].raw, "3 tomatoes, chopped");
        // The whole shape serializes to the SO3 recipe `data`.
        let data = serde_json::to_value(&recipe).unwrap();
        assert_eq!(data["ingredients"][0]["canonicalName"], "olive oil");
    }

    #[test]
    fn cache_key_is_stable_and_content_sensitive() {
        let e1 = ExtractedRecipe {
            name: "A".into(),
            servings: 2,
            raw_ingredients: vec!["1 egg".into()],
        };
        let k1 = cache_key("https://x/a", &e1);
        // Same URL + same content ⇒ same key (a re-import is a cache hit).
        assert_eq!(k1, cache_key("https://x/a", &e1));
        // Changed content ⇒ different key (re-normalize).
        let mut e2 = e1.clone();
        e2.raw_ingredients.push("2 cups flour".into());
        assert_ne!(k1, cache_key("https://x/a", &e2));
        // Different URL ⇒ different key.
        assert_ne!(k1, cache_key("https://x/b", &e1));
    }

    #[test]
    fn build_prompt_lists_lines_and_pins_schema() {
        let e = ExtractedRecipe {
            name: "Test".into(),
            servings: 1,
            raw_ingredients: vec!["2 tbsp olive oil".into(), "1 onion".into()],
        };
        let p = build_normalization_prompt(&e);
        assert!(p.contains("canonicalName"));
        assert!(p.contains("1. 2 tbsp olive oil"));
        assert!(p.contains("2. 1 onion"));
        assert!(p.contains("Test"));
    }
}
