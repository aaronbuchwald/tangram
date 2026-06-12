//! Guided Learning — a *Make It Stick*-driven tutor as a Tangram app.
//!
//! The app walks a learner through a specific piece of material by **asking
//! questions** (retrieval practice) rather than re-presenting it, gates the
//! answer reveal behind a committed attempt (generation), records confidence
//! before the reveal and a grade after (calibration), captures own-words
//! explanations (elaboration), schedules spaced reviews (Leitner/SM-2-lite),
//! and interleaves topics — and co-authors a durable, collaboratively editable
//! `.md` study artifact out of the session.
//!
//! Everything — the material, every question/answer, the schedule, and the
//! artifact — lives in this component's replicated Automerge document. The
//! ONLY egress is the tutor's Anthropic Messages-API call (an AI-enabled
//! component; the host injects the credential at the `http-fetch` boundary,
//! ADR-0005). See `docs/design/guided-learning.md`.

use tangram::prelude::*;

pub mod schedule;
pub mod tutor;

// ── model ────────────────────────────────────────────────────────────────────

#[model]
#[derive(Default)]
pub struct GuidedLearning {
    sessions: Vec<Session>,
}

#[model]
pub struct Session {
    pub id: String,
    /// First line of the material, or a learner-set title.
    pub title: String,
    /// The source material the session teaches. Stored in the doc so the
    /// tutor's questions are reproducible and nothing leaves the component.
    pub material: String,
    /// Derived/added topics for interleaving (Vec, ordered, deterministic).
    pub topics: Vec<Topic>,
    pub questions: Vec<Question>,
    /// Reference answers held back from the questions until reveal, so the
    /// replicated doc can't be used to peek before an attempt is committed.
    /// An entry is consumed (removed) when its question is evaluated/revealed.
    pub pending_answers: Vec<PendingAnswer>,
    /// The growing markdown artifact (the "note"). Raw markdown text, rendered
    /// client-side; editable directly (collaborative via the CRDT).
    pub artifact_md: String,
    pub created_at_ms: i64,
    /// Session-close reflection (*Make It Stick*: reflection).
    #[autosurgeon(missing = "Option::default")]
    pub reflection: Option<String>,
    #[autosurgeon(missing = "Option::default")]
    pub updated_at_ms: Option<i64>,
    /// Where this artifact lives relative to the shell vault, once exported.
    /// `None` until the learner names/saves it.
    #[autosurgeon(missing = "Option::default")]
    pub vault_path: Option<String>,
}

#[model]
pub struct Topic {
    pub id: String,
    pub name: String,
}

/// A reference answer withheld from its question until reveal (so the doc
/// can't be used to peek before a committed attempt).
#[model]
pub struct PendingAnswer {
    pub question_id: String,
    pub model_answer: String,
}

#[model]
pub struct Question {
    pub id: String,
    pub topic_id: String,
    /// "factual" | "elaboration" | "connection" | "application"
    pub kind: String,
    /// The question text (LLM-generated).
    pub prompt: String,
    /// The reference/model answer — populated only when revealed, so the doc
    /// itself can't be used to "peek" before an attempt is committed.
    #[autosurgeon(missing = "Option::default")]
    pub model_answer: Option<String>,
    /// Generation: at least one attempt before reveal.
    pub attempts: Vec<Attempt>,
    pub revealed: bool,
    /// Did the learner show the source for this one? (calibration honesty)
    pub peeked: bool,
    pub schedule: ReviewSchedule,
    pub created_at_ms: i64,
}

#[model]
pub struct Attempt {
    /// Free text; "" with `idk=true` is a valid attempt.
    pub answer: String,
    pub idk: bool,
    /// 0..=100, recorded BEFORE reveal (calibration).
    pub confidence: u8,
    /// Tutor's grade, recorded AFTER reveal. `None` for an unscored attempt.
    #[autosurgeon(missing = "Option::default")]
    pub grade: Option<u8>,
    /// The tutor's Socratic follow-up / correction for this attempt.
    #[autosurgeon(missing = "Option::default")]
    pub feedback: Option<String>,
    pub at_ms: i64,
}

#[model]
pub struct ReviewSchedule {
    /// When this item is next due (genesis: 0 = due now).
    pub due_at_ms: i64,
    /// Leitner box / SM-2-lite step.
    pub interval_index: u8,
    /// Coarse ease factor (×100, e.g. 130..=250).
    pub ease: u8,
}

impl Default for ReviewSchedule {
    fn default() -> Self {
        Self::genesis()
    }
}

/// A compact session row for the UI list (no full question/material payload).
/// A return-only type (not stored in the doc), so it derives serialize/schema
/// but not the autosurgeon CRDT traits `#[model]` would add.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub question_count: u32,
    pub due_count: u32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// One calibration data point: a graded attempt's confidence vs grade. A large
/// positive `delta` is over-confidence (the illusion of knowing). Return-only.
#[derive(Debug, Clone, serde::Serialize, schemars::JsonSchema)]
pub struct CalibrationPoint {
    pub question_id: String,
    pub confidence: u8,
    pub grade: u8,
    /// `confidence - grade`: positive = over-confident.
    pub delta: i32,
    pub overconfident: bool,
}

// ── actions ────────────────────────────────────────────────────────────────

#[actions]
impl GuidedLearning {
    /// Start a session over a piece of material. Seeds the artifact with a
    /// title + provenance; topics/questions stay empty until
    /// `generate_questions` runs. Returns the new session id.
    pub fn start_session(&mut self, material: String, title: Option<String>) -> String {
        let now = now_ms();
        let id = uuid::Uuid::new_v4().to_string();
        let title = title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .unwrap_or_else(|| title_from_material(&material));
        let artifact_md = seed_artifact(&title, &material);
        self.sessions.push(Session {
            id: id.clone(),
            title,
            material,
            topics: Vec::new(),
            questions: Vec::new(),
            pending_answers: Vec::new(),
            artifact_md,
            created_at_ms: now,
            reflection: None,
            updated_at_ms: Some(now),
            vault_path: None,
        });
        id
    }

    /// Delete a session and its artifact.
    pub fn delete_session(&mut self, session_id: String) -> Result<(), String> {
        let before = self.sessions.len();
        self.sessions.retain(|s| s.id != session_id);
        if self.sessions.len() == before {
            return Err(format!("no session with id {session_id}"));
        }
        Ok(())
    }

    /// Submit an attempt at a question — the **generation** gate. Records the
    /// answer and the learner's confidence BEFORE any reveal; `grade` stays
    /// `None` until `evaluate_answer` runs (the **calibration** ordering).
    /// `idk=true` with empty text is a valid attempt (still counts, still
    /// enables reveal). Errors if the question doesn't exist or is already
    /// revealed.
    pub fn submit_answer(
        &mut self,
        session_id: String,
        question_id: String,
        answer: String,
        idk: bool,
        confidence: u8,
    ) -> Result<(), String> {
        let q = self.question_mut(&session_id, &question_id)?;
        if q.revealed {
            return Err("question already revealed; cannot submit a new attempt".into());
        }
        let answer = answer.trim().to_string();
        if answer.is_empty() && !idk {
            return Err("provide an answer, or mark \"I don't know\" (idk=true)".into());
        }
        q.attempts.push(Attempt {
            answer,
            idk,
            confidence: confidence.min(100),
            grade: None,
            feedback: None,
            at_ms: now_ms(),
        });
        touch(self, &session_id);
        Ok(())
    }

    /// Record that the learner showed the source for this question (the "peek"
    /// — *Make It Stick*: peeking weakens retrieval; allowed but logged).
    pub fn mark_peeked(&mut self, session_id: String, question_id: String) -> Result<(), String> {
        let q = self.question_mut(&session_id, &question_id)?;
        q.peeked = true;
        Ok(())
    }

    /// Replace the artifact markdown (last-writer-wins, like notes'
    /// `update_note`). Concurrent edits merge via Automerge — this is the
    /// collaborative editing path. Errors if the session doesn't exist.
    pub fn edit_artifact(&mut self, session_id: String, new_md: String) -> Result<(), String> {
        let s = self.session_mut(&session_id)?;
        s.artifact_md = new_md;
        s.updated_at_ms = Some(now_ms());
        Ok(())
    }

    /// Record the session-close reflection and append it to the artifact's
    /// reflection section (*Make It Stick*: reflection consolidates learning).
    pub fn record_reflection(&mut self, session_id: String, text: String) -> Result<(), String> {
        let s = self.session_mut(&session_id)?;
        let text = text.trim().to_string();
        s.reflection = Some(text.clone());
        if !text.is_empty() {
            s.artifact_md
                .push_str(&format!("\n## Reflection\n\n{text}\n"));
        }
        s.updated_at_ms = Some(now_ms());
        Ok(())
    }

    /// Name/export location for the artifact (Open Decision 1, option B): set
    /// the vault path the artifact would be saved under. Pure metadata — the
    /// actual cross-app copy is a UI-initiated call.
    pub fn set_vault_path(&mut self, session_id: String, path: String) -> Result<(), String> {
        let s = self.session_mut(&session_id)?;
        s.vault_path = Some(path);
        Ok(())
    }

    /// The questions due for review now (already-attempted, due, interleaved
    /// across topics). Pure selector, no I/O.
    #[must_use]
    pub fn due_reviews(&self, session_id: String) -> Vec<Question> {
        self.sessions
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| {
                schedule::due_reviews(&s.questions, now_ms())
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The next questions to present, interleaved across topics/kinds. Pure.
    #[must_use]
    pub fn next_questions(&self, session_id: String) -> Vec<Question> {
        self.sessions
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| {
                schedule::interleave(&s.questions, now_ms())
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Calibration readout for a session: one point per graded attempt
    /// (confidence vs grade), surfacing the illusion of knowing. Pure.
    #[must_use]
    pub fn calibration(&self, session_id: String) -> Vec<CalibrationPoint> {
        let Some(s) = self.sessions.iter().find(|s| s.id == session_id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for q in &s.questions {
            for a in &q.attempts {
                if let Some(grade) = a.grade {
                    let delta = i32::from(a.confidence) - i32::from(grade);
                    out.push(CalibrationPoint {
                        question_id: q.id.clone(),
                        confidence: a.confidence,
                        grade,
                        delta,
                        overconfident: delta >= 20,
                    });
                }
            }
        }
        out
    }

    /// Generate retrieval-practice questions for a session via the tutor LLM
    /// (the AI-enabled-component call). Reads the material out of the doc under
    /// a brief read (dropping the lock before the await), calls the tutor for
    /// structured `{topic, kind, prompt, model_answer}` rows, then commits the
    /// `Question`s + `Topic`s with a genesis schedule (due now). Interleaving
    /// is applied at SELECTION time (`next_questions`), not here.
    pub async fn generate_questions(
        ctx: Ctx<Self>,
        session_id: String,
        count: Option<usize>,
    ) -> Result<usize, String> {
        // Clone the material under a brief read, then drop the snapshot — the
        // store lock is never held across the await below.
        let material = {
            let state = ctx.state().map_err(|e| e.to_string())?;
            state
                .sessions
                .iter()
                .find(|s| s.id == session_id)
                .map(|s| s.material.clone())
                .ok_or_else(|| format!("no session with id {session_id}"))?
        };

        let count = count.unwrap_or(6).clamp(1, 24);
        let generated = tutor::generate(&material, count)
            .await
            .map_err(|e| format!("could not generate questions: {e:#}"))?;
        if generated.is_empty() {
            return Err("the tutor returned no questions for this material".into());
        }

        ctx.mutate("generate_questions", |m| {
            m.commit_questions(&session_id, generated)
        })
        .map_err(|e| e.to_string())?
    }

    /// Evaluate the latest unscored attempt for a question (the **calibration**
    /// + **reveal** step). Sends the question + model answer + the learner's
    /// attempt to the tutor for a structured grade, then commits: sets
    /// `grade`/`feedback`, reveals the model answer, advances the spaced
    /// schedule (a wrong answer is due sooner), and **appends the exchange to
    /// the artifact** (the elaboration/generation output accreting into the
    /// study note). Errors if there is no unscored attempt to grade.
    pub async fn evaluate_answer(
        ctx: Ctx<Self>,
        session_id: String,
        question_id: String,
    ) -> Result<u8, String> {
        // Snapshot what the tutor needs, under a brief read; drop before await.
        let (prompt, model_answer, learner_answer, idk) = {
            let state = ctx.state().map_err(|e| e.to_string())?;
            let s = state
                .sessions
                .iter()
                .find(|s| s.id == session_id)
                .ok_or_else(|| format!("no session with id {session_id}"))?;
            let q = s
                .questions
                .iter()
                .find(|q| q.id == question_id)
                .ok_or_else(|| format!("no question with id {question_id}"))?;
            let attempt = q
                .attempts
                .iter()
                .rev()
                .find(|a| a.grade.is_none())
                .ok_or("no unscored attempt to evaluate; submit_answer first")?;
            // The reference answer lives in pending_answers until reveal (or on
            // the question if it was already revealed once and re-quizzed).
            let model_answer = s
                .pending_answers
                .iter()
                .find(|p| p.question_id == question_id)
                .map(|p| p.model_answer.clone())
                .or_else(|| q.model_answer.clone())
                .unwrap_or_default();
            (
                q.prompt.clone(),
                model_answer,
                attempt.answer.clone(),
                attempt.idk,
            )
        };

        let eval = tutor::evaluate(&prompt, &model_answer, &learner_answer, idk)
            .await
            .map_err(|e| format!("could not evaluate answer: {e:#}"))?;

        ctx.mutate("evaluate_answer", |m| {
            m.commit_evaluation(&session_id, &question_id, eval)
        })
        .map_err(|e| e.to_string())?
    }

    /// Compact session list for the UI. Pure.
    #[must_use]
    pub fn list_sessions(&self) -> Vec<SessionSummary> {
        let now = now_ms();
        let mut rows: Vec<SessionSummary> = self
            .sessions
            .iter()
            .map(|s| SessionSummary {
                id: s.id.clone(),
                title: s.title.clone(),
                question_count: s.questions.len() as u32,
                due_count: schedule::due_reviews(&s.questions, now).len() as u32,
                created_at_ms: s.created_at_ms,
                updated_at_ms: s.updated_at_ms.unwrap_or(s.created_at_ms),
            })
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.updated_at_ms));
        rows
    }
}

// ── internal helpers (not actions) ───────────────────────────────────────────

impl GuidedLearning {
    /// Commit step of `generate_questions`: append topics + questions for the
    /// generated rows. Topics are de-duplicated by name (deterministic id from
    /// the slug). Returns the number of questions added.
    fn commit_questions(
        &mut self,
        session_id: &str,
        generated: Vec<tutor::GeneratedQuestion>,
    ) -> Result<usize, String> {
        let now = now_ms();
        let s = self.session_mut(session_id)?;
        let mut added = 0;
        for g in generated {
            let topic_name = g.topic.trim();
            let topic_id = if topic_name.is_empty() {
                "general".to_string()
            } else {
                format!("topic_{}", slug(topic_name))
            };
            if !s.topics.iter().any(|t| t.id == topic_id) {
                s.topics.push(Topic {
                    id: topic_id.clone(),
                    name: if topic_name.is_empty() {
                        "General".into()
                    } else {
                        topic_name.to_string()
                    },
                });
            }
            // The reference answer is NOT stored on the question at generation
            // time: `model_answer` stays `None` until reveal, so the replicated
            // doc itself can't be used to peek before an attempt is committed.
            // It is stashed separately in `pending_answers` (cleared on reveal)
            // and re-surfaced by `evaluate_answer` (which also re-grades).
            let qid = uuid::Uuid::new_v4().to_string();
            s.questions.push(Question {
                id: qid.clone(),
                topic_id,
                kind: normalize_kind(&g.kind),
                prompt: g.prompt.trim().to_string(),
                model_answer: None,
                attempts: Vec::new(),
                revealed: false,
                peeked: false,
                schedule: ReviewSchedule::genesis(),
                created_at_ms: now,
            });
            s.pending_answers.push(PendingAnswer {
                question_id: qid,
                model_answer: g.model_answer.trim().to_string(),
            });
            added += 1;
        }
        s.updated_at_ms = Some(now);
        Ok(added)
    }

    /// Commit step of `evaluate_answer`: score the latest unscored attempt,
    /// reveal, advance the schedule, and append the exchange to the artifact.
    fn commit_evaluation(
        &mut self,
        session_id: &str,
        question_id: &str,
        eval: tutor::Evaluation,
    ) -> Result<u8, String> {
        let now = now_ms();
        let grade = eval.grade.min(100);
        // The reference answer to reveal: the tutor's, else the one we withheld
        // in pending_answers at generation, else any already on the question.
        let s = self.session_mut(session_id)?;
        let withheld = s
            .pending_answers
            .iter()
            .find(|p| p.question_id == question_id)
            .map(|p| p.model_answer.clone());
        let reveal_answer = eval
            .model_answer
            .filter(|m| !m.trim().is_empty())
            .or(withheld)
            .unwrap_or_default();
        // Reveal consumes the withheld answer.
        s.pending_answers.retain(|p| p.question_id != question_id);

        let q = self.question_mut(session_id, question_id)?;
        let confidence = {
            let attempt = q
                .attempts
                .iter_mut()
                .rev()
                .find(|a| a.grade.is_none())
                .ok_or("no unscored attempt to evaluate")?;
            attempt.grade = Some(grade);
            attempt.feedback = Some(eval.feedback.clone());
            attempt.confidence
        };
        let prompt = q.prompt.clone();
        let learner = q
            .attempts
            .last()
            .map(|a| {
                if a.idk && a.answer.is_empty() {
                    "(I don't know)".to_string()
                } else {
                    a.answer.clone()
                }
            })
            .unwrap_or_default();
        if !reveal_answer.is_empty() {
            q.model_answer = Some(reveal_answer.clone());
        }
        q.revealed = true;
        q.schedule.advance(grade, now);

        // Accrete the exchange into the artifact (the "reward" loop).
        let block = artifact_block(
            &prompt,
            &learner,
            confidence,
            grade,
            &eval.feedback,
            &reveal_answer,
        );
        let s = self.session_mut(session_id)?;
        s.artifact_md.push_str(&block);
        s.updated_at_ms = Some(now);
        Ok(grade)
    }

    fn session_mut(&mut self, session_id: &str) -> Result<&mut Session, String> {
        self.sessions
            .iter_mut()
            .find(|s| s.id == session_id)
            .ok_or_else(|| format!("no session with id {session_id}"))
    }

    /// Test seam: push a question with its withheld answer directly, standing
    /// in for `generate_questions` when a test must exercise the pure sync
    /// gates without an LLM. Not an action (no `&self`/`Ctx` action shape on
    /// the registry) — a plain `#[doc(hidden)]` helper.
    #[doc(hidden)]
    pub fn test_push_question(
        &mut self,
        session_id: &str,
        question_id: &str,
        prompt: &str,
        model_answer: &str,
    ) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            s.questions.push(Question {
                id: question_id.to_string(),
                topic_id: "topic_test".into(),
                kind: "factual".into(),
                prompt: prompt.to_string(),
                model_answer: None,
                attempts: Vec::new(),
                revealed: false,
                peeked: false,
                schedule: ReviewSchedule::genesis(),
                created_at_ms: 0,
            });
            s.pending_answers.push(PendingAnswer {
                question_id: question_id.to_string(),
                model_answer: model_answer.to_string(),
            });
        }
    }

    fn question_mut(
        &mut self,
        session_id: &str,
        question_id: &str,
    ) -> Result<&mut Question, String> {
        let s = self.session_mut(session_id)?;
        s.questions
            .iter_mut()
            .find(|q| q.id == question_id)
            .ok_or_else(|| format!("no question with id {question_id}"))
    }
}

/// Stamp a session's `updated_at_ms` (private helper for sync actions that
/// borrow a question first).
fn touch(model: &mut GuidedLearning, session_id: &str) {
    if let Some(s) = model.sessions.iter_mut().find(|s| s.id == session_id) {
        s.updated_at_ms = Some(now_ms());
    }
}

/// The first non-empty line of the material, truncated, or a default title.
fn title_from_material(material: &str) -> String {
    material
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| {
            let t: String = l.chars().take(80).collect();
            t
        })
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Untitled session".into())
}

/// Seed the artifact with a heading + material provenance.
fn seed_artifact(title: &str, material: &str) -> String {
    let preview: String = material.chars().take(280).collect();
    let ellipsis = if material.chars().count() > 280 {
        "…"
    } else {
        ""
    };
    format!(
        "# {title}\n\n*A study note co-authored through retrieval practice.*\n\n\
         ## Source\n\n> {preview}{ellipsis}\n"
    )
}

/// Canonicalize a question kind to the known set; unknown kinds fall back to
/// "factual".
fn normalize_kind(kind: &str) -> String {
    match kind.trim().to_lowercase().as_str() {
        "elaboration" => "elaboration",
        "connection" => "connection",
        "application" => "application",
        _ => "factual",
    }
    .to_string()
}

/// One artifact block for an evaluated exchange: the question, the learner's
/// own attempt (verbatim — the generation/elaboration output), the calibration
/// note, the tutor's feedback, and the reference answer.
fn artifact_block(
    prompt: &str,
    learner: &str,
    confidence: u8,
    grade: u8,
    feedback: &str,
    model_answer: &str,
) -> String {
    let delta = i16::from(confidence) - i16::from(grade);
    let calib = if delta >= 20 {
        format!("You were {confidence}% confident — graded {grade}. Over-confident here.")
    } else if delta <= -20 {
        format!(
            "You were {confidence}% confident — graded {grade}. You knew more than you thought."
        )
    } else {
        format!("Confidence {confidence}% vs grade {grade} — well calibrated.")
    };
    format!(
        "\n### {prompt}\n\n**My answer:** {learner}\n\n_{calib}_\n\n\
         **Tutor:** {feedback}\n\n**Reference:** {model_answer}\n"
    )
}

/// Slugify a name into a deterministic id fragment: lowercase alphanumerics
/// joined by single underscores ("Light Reactions" → "light_reactions").
fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_sep = true;
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
    if out.is_empty() {
        out.push_str("general");
    }
    out
}

fn now_ms() -> i64 {
    tangram::time::now_ms()
}

/// MCP instructions, shared between the native app builder and the WASM
/// component's `describe()` export.
const INSTRUCTIONS: &str = "A Make It Stick-driven tutor. Start a session over a piece of \
     material with start_session, then generate_questions (LLM) to populate retrieval-practice \
     questions. Answer with submit_answer (records your confidence BEFORE the reveal); \
     evaluate_answer (LLM) grades the attempt, gives Socratic feedback, advances the spaced \
     review schedule, and appends the exchange to the study artifact. due_reviews / \
     next_questions interleave topics; calibration surfaces over-confidence. Edit the markdown \
     artifact with edit_artifact (collaborative via the CRDT). The tutor needs an Anthropic \
     credential configured host-side; without it the sync actions (artifact editing, \
     reflection) still work and the tutor reports unavailable.";

/// Capabilities object reported by `describe()`/the capabilities probe — ONE
/// constructor so the WASM `describe()` manifest and any native route agree.
/// `tutor_available` reflects whether an Anthropic credential is resolvable
/// (mirrors nutrition's `description_input`); the host ANDs in egress-injection
/// resolution for the component path.
#[must_use]
pub fn capabilities_json(tutor_available: bool) -> serde_json::Value {
    serde_json::json!({
        "tutor": "anthropic/claude-opus-4-8",
        "tutor_available": tutor_available,
    })
}

/// The guided-learning app, fully configured (native binary / multi-app host).
#[cfg(not(target_family = "wasm"))]
#[must_use]
pub fn app() -> App<GuidedLearning> {
    App::<GuidedLearning>::new("guided-learning")
        .instructions(INSTRUCTIONS)
        .ui_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/ui"))
}

// Compiled for wasm32-wasip2, the same model + actions become a Tangram
// component (`tangram-host` owns the platform around it; the tutor's HTTP goes
// through the host's allowlist-enforced `http-fetch` import, which injects the
// Anthropic credential at the egress boundary — ADR-0005). The capabilities
// object reports whether the tutor credential is resolvable.
#[cfg(target_family = "wasm")]
tangram::export_component!(GuidedLearning {
    name: "guided-learning",
    instructions: INSTRUCTIONS,
    capabilities: || Some(capabilities_json(tutor::credential_present())),
});
