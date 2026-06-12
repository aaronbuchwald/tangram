//! The prompt builder (pure, in-memory — step 2 of the AI-enabled-component
//! pattern).
//!
//! Assembles the effective prompt from four data-driven parts: the master
//! `system_prompt`, the learned few-shot preamble (folded from human
//! feedback), the per-section sub-prompts, and the resolved source inputs.
//! Pure and deterministic given its inputs, so it is trivially unit-testable
//! and the `effective_prompt` stored on a run is exactly reproducible.

use crate::source::BriefInput;
use crate::{BriefConfig, LearnedExample, OutputSection};

/// How many top-weighted learned examples to fold into the preamble. Bounded
/// so the prompt stays within a sane budget (design §8.1 "capped to a token
/// budget").
const MAX_LEARNED: usize = 8;

/// The assembled prompt: the `system` instruction and the `user` message.
/// Stored joined as `BriefRun.effective_prompt` (auditable) and, in the live
/// tier, sent as the Anthropic Messages `system` + `messages[0]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub system: String,
    pub user: String,
}

impl Prompt {
    /// The single auditable string stored on the run (system + user), so the
    /// human can see exactly how their config + learned preamble combined.
    pub fn effective(&self) -> String {
        format!("[system]\n{}\n\n[user]\n{}", self.system, self.user)
    }
}

/// Build the prompt from the config, the ENABLED sections (render order), the
/// learned examples, and the resolved inputs.
pub fn build_prompt(
    config: &BriefConfig,
    sections: &[OutputSection],
    learned: &[LearnedExample],
    inputs: &[BriefInput],
) -> Prompt {
    let mut system = config.system_prompt.trim().to_string();

    // Fold the highest-weighted learned examples into the system preamble
    // (in-tangram learning — design §8). Deterministic order: weight desc,
    // then created-at, then id, so the same learned set yields the same prompt.
    let mut top: Vec<&LearnedExample> = learned.iter().collect();
    top.sort_by(|a, b| {
        b.weight
            .cmp(&a.weight)
            .then_with(|| a.created_at_ms.cmp(&b.created_at_ms))
            .then_with(|| a.id.cmp(&b.id))
    });
    let folded: Vec<&LearnedExample> = top
        .into_iter()
        .filter(|l| !l.note.trim().is_empty())
        .take(MAX_LEARNED)
        .collect();
    if !folded.is_empty() {
        system.push_str("\n\nLearned preferences from my past feedback (apply these):");
        for ex in folded {
            system.push_str(&format!("\n- {}", ex.note.trim()));
        }
    }

    // The user message: the requested output sections, then the inputs the
    // model may draw from. We instruct a strict per-section JSON output so the
    // result maps cleanly back onto SectionOutputs (live tier uses a matching
    // json_schema; the fixture response already matches this shape).
    let mut user = String::new();
    user.push_str("Produce the following sections. For each, write content matching its format.\n");
    for s in sections {
        user.push_str(&format!(
            "\n## {} (id: {}, format: {})\n{}\n",
            s.title,
            s.id,
            s.format,
            s.prompt.trim()
        ));
    }

    user.push_str(
        "\n---\nInputs (today's calendar and email; do not invent anything not listed):\n",
    );
    if inputs.is_empty() {
        user.push_str("(no inputs available)\n");
    } else {
        for i in inputs {
            let detail = if i.detail.trim().is_empty() {
                String::new()
            } else {
                format!(" — {}", i.detail.trim())
            };
            user.push_str(&format!("- [{}] {}{}\n", i.kind, i.title.trim(), detail));
        }
    }

    user.push_str(
        "\nReturn a JSON object {\"sections\": [{\"section_id\": \"…\", \"content\": \"…\"}]} \
         with exactly one entry per requested section id.",
    );

    Prompt { system, user }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BriefConfig {
        BriefConfig {
            system_prompt: "Be my chief of staff. Flag what needs action.".into(),
            model_tier: "default".into(),
            max_runs: 30,
        }
    }

    fn section(id: &str, title: &str, prompt: &str, format: &str) -> OutputSection {
        OutputSection {
            id: id.into(),
            title: title.into(),
            prompt: prompt.into(),
            format: format.into(),
            position: 0,
            enabled: true,
        }
    }

    fn learned(id: &str, note: &str, weight: i64, created: i64) -> LearnedExample {
        LearnedExample {
            id: id.into(),
            note: note.into(),
            weight,
            created_at_ms: created,
        }
    }

    fn input(kind: &str, title: &str, detail: &str) -> BriefInput {
        BriefInput {
            kind: kind.into(),
            when_ms: 0,
            title: title.into(),
            detail: detail.into(),
        }
    }

    #[test]
    fn prompt_includes_sections_and_inputs() {
        let p = build_prompt(
            &config(),
            &[section(
                "sec_actions",
                "Action items",
                "Only what needs action.",
                "checklist",
            )],
            &[],
            &[input("gmail", "Invoice due", "from billing")],
        );
        assert!(p.system.contains("chief of staff"));
        assert!(p.user.contains("Action items"));
        assert!(p.user.contains("sec_actions"));
        assert!(p.user.contains("Invoice due"));
        assert!(p.user.contains("from billing"));
        // No learned preamble when there is none.
        assert!(!p.system.contains("Learned preferences"));
    }

    #[test]
    fn learned_examples_fold_into_preamble_by_weight() {
        let learned = vec![
            learned("l1", "Keep action items to 3 max.", 1, 100),
            learned("l2", "Always name the sender.", 5, 200),
            learned("l3", "", 9, 50), // empty note is skipped
        ];
        let p = build_prompt(&config(), &[], &learned, &[]);
        assert!(p.system.contains("Learned preferences"));
        // higher weight appears before lower weight
        let hi = p.system.find("name the sender").unwrap();
        let lo = p.system.find("3 max").unwrap();
        assert!(hi < lo);
        assert!(!p.system.contains("\n- \n"), "empty note not folded");
    }

    #[test]
    fn build_is_deterministic() {
        let l = vec![learned("l2", "b", 5, 200), learned("l1", "a", 5, 100)];
        let inputs = [input("calendar", "Standup", "Zoom")];
        let a = build_prompt(&config(), &[], &l, &inputs);
        let b = build_prompt(&config(), &[], &l, &inputs);
        assert_eq!(a, b);
    }
}
