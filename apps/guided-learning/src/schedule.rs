//! Spaced-repetition scheduling + interleaving selection — pure functions, no
//! I/O, no LLM. *Make It Stick*: spacing study over time beats massing, and
//! mixing topics/kinds (interleaving) beats blocking one type.
//!
//! The schedule is a Leitner/SM-2-lite step: each graded answer advances (or
//! resets) a per-question `interval_index` and adjusts a coarse `ease`, from
//! which the next `due_at_ms` is computed. Nothing here touches the clock
//! except through an explicit `now_ms` argument, so it is fully deterministic
//! and unit-testable.

use crate::{Question, ReviewSchedule};

/// A grade at or above this (0..=100) counts as a "correct" recall that
/// advances the Leitner box; below it resets to the first box and shortens the
/// next interval (the item is seen again sooner — *Make It Stick*: a missed
/// item earns more retrieval practice).
pub const PASS_THRESHOLD: u8 = 60;

/// Leitner box intervals in days, indexed by `interval_index`. Index 0 is the
/// genesis box (due immediately); each correct recall promotes one box, capped
/// at the last. SM-2-lite scales the chosen interval by `ease / 100`.
const BOX_DAYS: &[u32] = &[0, 1, 3, 7, 16, 35];

const DAY_MS: i64 = 86_400_000;

/// Ease bounds (×100): a fixed coarse band so the schedule stays bounded and
/// deterministic. A correct recall nudges ease up, a miss nudges it down.
const EASE_MIN: u8 = 130;
const EASE_MAX: u8 = 250;
const EASE_DEFAULT: u8 = 200;
const EASE_STEP: u8 = 15;

impl ReviewSchedule {
    /// The genesis schedule for a freshly generated question: box 0, default
    /// ease, due immediately (`due_at_ms = 0`, which is `<= now` for any real
    /// clock, so a new question is "due now").
    #[must_use]
    pub fn genesis() -> Self {
        Self {
            due_at_ms: 0,
            interval_index: 0,
            ease: EASE_DEFAULT,
        }
    }

    /// Advance this schedule given a grade and the current time. A passing
    /// grade promotes one Leitner box and raises ease; a failing grade resets
    /// to box 0 and lowers ease. The next `due_at_ms` is
    /// `now + BOX_DAYS[box] * (ease/100)` days.
    ///
    /// Pure: the only clock input is `now_ms`.
    pub fn advance(&mut self, grade: u8, now_ms: i64) {
        let passed = grade >= PASS_THRESHOLD;
        if passed {
            let last = (BOX_DAYS.len() - 1) as u8;
            self.interval_index = self.interval_index.saturating_add(1).min(last);
            self.ease = self.ease.saturating_add(EASE_STEP).min(EASE_MAX);
        } else {
            self.interval_index = 0;
            self.ease = self.ease.saturating_sub(EASE_STEP).max(EASE_MIN);
        }
        let base_days = BOX_DAYS[self.interval_index as usize] as i64;
        let scaled_ms = base_days * DAY_MS * i64::from(self.ease) / 100;
        self.due_at_ms = now_ms + scaled_ms;
    }

    /// Whether this item is due for review at `now_ms`.
    #[must_use]
    pub fn is_due(&self, now_ms: i64) -> bool {
        self.due_at_ms <= now_ms
    }
}

/// Select the next questions to present, interleaved across topics and kinds
/// rather than exhausting one topic before the next (*Make It Stick*:
/// interleaving). Deterministic: stable round-robin over topics in their
/// document order, and within a topic, questions in document order.
///
/// `now_ms` decides what is "due"; only due-or-never-attempted questions are
/// candidates. The result is a flat, interleaved ordering — the UI shows the
/// head, due-review section reads the same set filtered to already-attempted.
#[must_use]
pub fn interleave(questions: &[Question], now_ms: i64) -> Vec<&Question> {
    // Group candidate questions by topic, preserving document order within
    // each group and first-seen order across groups.
    let mut groups: Vec<(String, Vec<&Question>)> = Vec::new();
    for q in questions {
        if !q.schedule.is_due(now_ms) {
            continue;
        }
        match groups.iter_mut().find(|(t, _)| *t == q.topic_id) {
            Some((_, v)) => v.push(q),
            None => groups.push((q.topic_id.clone(), vec![q])),
        }
    }

    // Round-robin across topic groups: topic A's first, topic B's first, …,
    // then each topic's second, and so on — the visible "mix".
    let mut out: Vec<&Question> = Vec::new();
    let mut idx = 0;
    loop {
        let mut took_any = false;
        for (_, group) in &groups {
            if let Some(q) = group.get(idx) {
                out.push(q);
                took_any = true;
            }
        }
        if !took_any {
            break;
        }
        idx += 1;
    }
    out
}

/// The subset of `questions` that are due for review AND have at least one
/// prior attempt (i.e. genuine spaced-repetition re-quizzes, not first-time
/// questions). Interleaved across topics like [`interleave`].
#[must_use]
pub fn due_reviews(questions: &[Question], now_ms: i64) -> Vec<&Question> {
    interleave(questions, now_ms)
        .into_iter()
        .filter(|q| !q.attempts.is_empty())
        .collect()
}
