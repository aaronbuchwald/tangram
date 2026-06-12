//! GL1 — model + genesis parity + the pure scheduling/interleaving functions.
//!
//! No LLM, no wasm runtime: the deterministic-genesis property and the
//! Leitner step + interleaving selector are all pure and unit-testable.

use guided_learning::schedule;
use guided_learning::{GuidedLearning, Question, ReviewSchedule};

/// Genesis is deterministic: the same bytes every time (the shared root that
/// lets independently-started instances — guest or native — merge). Both the
/// component and the native binary derive these identical bytes from the same
/// `genesis_bytes::<GuidedLearning>()` codepath, so byte-equality here is the
/// guest↔native parity the host relies on.
#[test]
fn genesis_is_deterministic_and_empty() {
    let a = tangram_core::genesis_bytes::<GuidedLearning>().expect("genesis a");
    let b = tangram_core::genesis_bytes::<GuidedLearning>().expect("genesis b");
    assert_eq!(
        a, b,
        "genesis bytes must be byte-identical across instances"
    );

    let model = GuidedLearning::default();
    let json = serde_json::to_value(&model).unwrap();
    assert_eq!(
        json["sessions"].as_array().map(|a| a.len()),
        Some(0),
        "default genesis is an empty sessions list (no timestamps/uuids)"
    );
}

/// A passing grade promotes the Leitner box and pushes the due date out; a
/// failing grade resets to box 0 and schedules sooner (more retrieval).
#[test]
fn schedule_advances_on_grade() {
    let now = 1_000_000_000_000;

    let mut s = ReviewSchedule::genesis();
    assert_eq!(s.interval_index, 0);
    assert!(s.is_due(now), "a fresh question (due_at_ms=0) is due now");

    // Pass: box advances, due date moves into the future.
    s.advance(90, now);
    assert_eq!(s.interval_index, 1, "passing promotes one box");
    assert!(s.due_at_ms > now, "next review is scheduled in the future");
    let after_pass_due = s.due_at_ms;

    // A second pass pushes the box and the due date further out.
    s.advance(85, now);
    assert_eq!(s.interval_index, 2);
    assert!(
        s.due_at_ms > after_pass_due,
        "a higher box schedules further out"
    );

    // Fail: reset to box 0 and due sooner than the passing schedule.
    let mut high = s.clone();
    high.advance(20, now);
    assert_eq!(high.interval_index, 0, "a miss resets to the first box");
    assert!(
        high.due_at_ms < s.due_at_ms,
        "a missed item is due sooner than a passed one (more retrieval practice)"
    );
}

fn q(id: &str, topic: &str, attempted: bool) -> Question {
    Question {
        id: id.into(),
        topic_id: topic.into(),
        kind: "factual".into(),
        prompt: format!("prompt {id}"),
        model_answer: None,
        attempts: if attempted {
            vec![guided_learning::Attempt {
                answer: "x".into(),
                idk: false,
                confidence: 50,
                grade: Some(70),
                feedback: None,
                at_ms: 0,
            }]
        } else {
            Vec::new()
        },
        revealed: false,
        peeked: false,
        schedule: ReviewSchedule::genesis(), // due now
        created_at_ms: 0,
    }
}

/// The selector interleaves across topics (round-robin) rather than exhausting
/// one topic before the next.
#[test]
fn interleave_mixes_topics() {
    let now = 10;
    let questions = vec![
        q("a1", "topic_a", false),
        q("a2", "topic_a", false),
        q("a3", "topic_a", false),
        q("b1", "topic_b", false),
        q("b2", "topic_b", false),
    ];
    let order: Vec<&str> = schedule::interleave(&questions, now)
        .iter()
        .map(|q| q.id.as_str())
        .collect();
    // Round-robin: a1, b1, a2, b2, a3 — topics alternate at the head.
    assert_eq!(order, vec!["a1", "b1", "a2", "b2", "a3"], "got {order:?}");
    assert_ne!(
        order[0], order[1],
        "consecutive questions come from different topics (interleaving)"
    );
}

/// Not-yet-due questions are excluded; `due_reviews` keeps only those with a
/// prior attempt (genuine re-quizzes).
#[test]
fn due_reviews_filters_to_attempted_and_due() {
    let now = 10;
    let mut future = q("c1", "topic_c", true);
    future.schedule.due_at_ms = now + 1_000_000; // not due
    let questions = vec![
        q("a1", "topic_a", true),  // due + attempted -> review
        q("b1", "topic_b", false), // due but never attempted -> not a review
        future,                    // attempted but not due -> excluded
    ];
    let reviews: Vec<&str> = schedule::due_reviews(&questions, now)
        .iter()
        .map(|q| q.id.as_str())
        .collect();
    assert_eq!(reviews, vec!["a1"], "got {reviews:?}");
}
