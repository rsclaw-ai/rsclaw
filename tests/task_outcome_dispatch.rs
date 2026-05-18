//! Integration tests for the structured-outcome dispatch matrix.
//!
//! Exercises the full chain that `tool_task_finish` puts in motion:
//!
//!     stage_pending_outcome → drain_pending_outcome
//!         → TaskOutcome::Structured
//!             → decide_action
//!                 → manager.submit_task (when Spawn)
//!
//! Drives the redb-backed task queue manager directly so we don't need
//! HTTP, channel senders, or an LLM. The worker's `run` loop is implicitly
//! tested by re-running the same sequence the worker does on each turn.

use std::sync::Arc;

use rsclaw::{
    MemoryTier,
    gateway::task_queue::{
        decide_action, drain_pending_outcome, stage_pending_outcome, Completion,
        DispatchAction, Priority, QueuedMessage, Recommend, StructuredOutcome,
        TaskOutcome, TaskQueueManager, TaskStatus,
    },
    store::redb_store::RedbStore,
};

fn open_manager() -> (Arc<TaskQueueManager>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = RedbStore::open(&dir.path().join("queue.redb"), MemoryTier::Low)
        .expect("open redb");
    let manager = Arc::new(TaskQueueManager::new(Arc::new(store)));
    (manager, dir)
}

fn make_message(text: &str) -> QueuedMessage {
    QueuedMessage {
        text: text.to_string(),
        sender: "test-user".to_string(),
        channel: "test-channel".to_string(),
        account: None,
        chat_id: "test-chat".to_string(),
        is_group: false,
        reply_to: None,
        timestamp: 0,
        images: vec![],
        files: vec![],
    }
}

fn make_outcome(
    completion: Completion,
    recommend: Recommend,
    follow_ups: Vec<&str>,
) -> StructuredOutcome {
    StructuredOutcome {
        completion,
        recommend,
        verified: false,
        verification_log: None,
        accomplished: vec!["did stage 1".into()],
        skipped: vec![],
        blocked_on: vec![],
        assumptions: vec![],
        follow_up_tasks: follow_ups.into_iter().map(String::from).collect(),
        summary: None,
    }
}

// ---------------------------------------------------------------------------
// task_finish stash <-> worker drain handshake
// ---------------------------------------------------------------------------

#[test]
fn outcome_stash_is_session_scoped() {
    // Two sessions must not see each other's outcomes.
    let a = "test:scope:a";
    let b = "test:scope:b";

    stage_pending_outcome(a, make_outcome(Completion::Full, Recommend::Ship, vec![]));
    stage_pending_outcome(
        b,
        make_outcome(Completion::Partial, Recommend::Retry, vec![]),
    );

    let drained_a = drain_pending_outcome(a).expect("a should have outcome");
    let drained_b = drain_pending_outcome(b).expect("b should have outcome");

    assert_eq!(drained_a.recommend, Recommend::Ship);
    assert_eq!(drained_b.recommend, Recommend::Retry);
}

// ---------------------------------------------------------------------------
// Multi-turn self-drive: parent Continue → children enqueued
// ---------------------------------------------------------------------------

#[test]
fn structured_continue_spawns_follow_up_tasks_into_queue() {
    let (manager, _dir) = open_manager();
    let session = "test:selfdrive:parent";

    // Submit the parent task.
    let parent_msg = make_message("Refactor the auth module");
    let (parent_id, _) = manager
        .submit(session, parent_msg, Priority::User)
        .expect("submit parent");
    assert!(!parent_id.is_empty());

    // Match production order: worker dequeues parent (Pending -> Running)
    // BEFORE staging follow-ups. submit_task's `merge_into_pending` only
    // merges into Pending tasks, so a Running parent means follow-ups
    // create fresh Pending entries instead of being appended to the parent.
    let dequeued = manager.next().expect("next").expect("parent dequeued");
    assert_eq!(dequeued.id, parent_id);

    // Agent calls task_finish with Continue + 3 follow-ups.
    let outcome = make_outcome(
        Completion::Partial,
        Recommend::Continue,
        vec![
            "Extract auth middleware into its own module",
            "Wire the new module into the router",
            "Add an integration test for the unified path",
        ],
    );
    stage_pending_outcome(session, outcome);

    // Replicate the worker's per-turn logic:
    //   drain_pending_outcome → TaskOutcome::Structured → decide_action
    let drained = drain_pending_outcome(session).expect("staged outcome");
    let action = decide_action(&TaskOutcome::Structured(drained), 1, 10);

    match action {
        DispatchAction::Spawn { tasks } => {
            assert_eq!(tasks.len(), 3);
            // Submit each follow-up the way the worker does. Use the same
            // base message so channel/account inheritance is preserved.
            // The first call creates a fresh Pending task; subsequent calls
            // merge into it (same session_key + Pending parent). That's
            // production behaviour — verified separately below.
            let base = make_message("");
            for follow_up in tasks {
                let mut msg = base.clone();
                msg.text = follow_up;
                msg.sender = format!("{}:follow_up", base.sender);
                manager
                    .submit_task(session, msg, Priority::System, 10, 3600)
                    .expect("submit follow-up");
            }
        }
        other => panic!("expected Spawn, got {other:?}"),
    }

    // Parent completes.
    manager.complete(&parent_id).expect("complete parent");

    // One Pending task carrying all 3 follow-ups merged in via
    // merge_into_pending. The agent will read the multi-message task and
    // act on each turn-by-turn — that's the same merge semantics the
    // production worker relies on for batched channel messages.
    let stats = manager.stats().expect("stats");
    assert_eq!(stats.pending, 1, "follow-ups merge into one pending task");

    let drained_task = manager
        .next()
        .expect("next")
        .expect("merged follow-up task");
    assert_eq!(drained_task.messages.len(), 3);
    let merged = drained_task.merged_text();
    assert!(merged.contains("Extract auth middleware"));
    assert!(merged.contains("Wire the new module"));
    assert!(merged.contains("integration test"));
    manager.complete(&drained_task.id).expect("complete child");
}

// ---------------------------------------------------------------------------
// Abandon: task is marked Failed, not Completed
// ---------------------------------------------------------------------------

#[test]
fn structured_abandon_marks_task_failed() {
    let (manager, _dir) = open_manager();
    let session = "test:selfdrive:abandon";

    let (task_id, _) = manager
        .submit(session, make_message("Try the impossible"), Priority::User)
        .expect("submit");

    // Drive one turn: agent gives up.
    let outcome = make_outcome(Completion::Failed, Recommend::Abandon, vec![]);
    stage_pending_outcome(session, outcome);
    let drained = drain_pending_outcome(session).expect("staged");
    let action = decide_action(&TaskOutcome::Structured(drained), 1, 10);
    assert_eq!(action, DispatchAction::Fail);

    // Worker would call manager.fail() — do that and inspect. With
    // max_retries=0 (the worker's call from DispatchAction::Fail) the task
    // lands in the terminal `Dead` bucket rather than `Failed`, since
    // fail_task increments retries past the threshold immediately. Both
    // are terminal — Dead just means "no further retries".
    manager
        .fail(&task_id, "agent abandoned", 0)
        .expect("fail call");

    let stats = manager.stats().expect("stats");
    assert_eq!(
        stats.dead, 1,
        "abandoned task (max_retries=0) should land in Dead bucket"
    );
    assert_eq!(stats.failed, 0);
}

// ---------------------------------------------------------------------------
// Retry exhaustion: budget-exceeded retry downgrades to Fail
// ---------------------------------------------------------------------------

#[test]
fn structured_retry_at_max_turns_downgrades_to_fail() {
    let outcome = make_outcome(Completion::Minimal, Recommend::Retry, vec![]);
    // turn == max_turns (5 == 5) → at budget
    let action = decide_action(&TaskOutcome::Structured(outcome), 5, 5);
    assert_eq!(action, DispatchAction::Fail);
}

#[test]
fn structured_retry_under_budget_continues() {
    let outcome = make_outcome(Completion::Minimal, Recommend::Retry, vec![]);
    match decide_action(&TaskOutcome::Structured(outcome), 1, 10) {
        DispatchAction::AutoContinue { prompt, slow } => {
            assert!(prompt.contains("Retry"));
            assert!(slow, "retry should rate-limit");
        }
        other => panic!("expected AutoContinue, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Continue without follow-ups: don't wedge the task open
// ---------------------------------------------------------------------------

#[test]
fn structured_continue_without_followups_completes() {
    let outcome = make_outcome(Completion::Partial, Recommend::Continue, vec![]);
    assert_eq!(
        decide_action(&TaskOutcome::Structured(outcome), 1, 10),
        DispatchAction::Complete
    );
}

// ---------------------------------------------------------------------------
// Mixed multi-turn: turn-1 Partial -> auto-continue,
//                   turn-2 task_finish(Ship) -> Complete
// ---------------------------------------------------------------------------

#[test]
fn mixed_turns_partial_then_task_finish_ship() {
    let session = "test:selfdrive:mixed";

    // Turn 1: agent didn't call task_finish, classifier returns Partial.
    let action_t1 = decide_action(&TaskOutcome::Partial, 1, 5);
    match action_t1 {
        DispatchAction::AutoContinue { .. } => {} // expected
        other => panic!("turn 1 expected AutoContinue, got {other:?}"),
    }

    // Turn 2: agent calls task_finish with Ship.
    stage_pending_outcome(
        session,
        make_outcome(Completion::Full, Recommend::Ship, vec![]),
    );
    let drained = drain_pending_outcome(session).expect("staged");
    let action_t2 = decide_action(&TaskOutcome::Structured(drained), 2, 5);
    assert_eq!(action_t2, DispatchAction::Complete);
}

// ---------------------------------------------------------------------------
// Outcome serialization (A2A metadata.outcome compatibility)
// ---------------------------------------------------------------------------

#[test]
fn outcome_serializes_with_snake_case_keys() {
    let mut out = make_outcome(
        Completion::Partial,
        Recommend::NeedsHuman,
        vec!["follow-up 1"],
    );
    out.blocked_on = vec!["which database?".into()];
    out.assumptions = vec!["assumed postgres".into()];

    let json = serde_json::to_value(&out).expect("serialize outcome");
    assert_eq!(json["completion"], "partial");
    assert_eq!(json["recommend"], "needs_human");
    assert_eq!(json["follow_up_tasks"][0], "follow-up 1");
    assert_eq!(json["blocked_on"][0], "which database?");
    assert_eq!(json["assumptions"][0], "assumed postgres");
    assert_eq!(json["accomplished"][0], "did stage 1");
}

// ---------------------------------------------------------------------------
// Drain is single-shot
// ---------------------------------------------------------------------------

#[test]
fn drain_consumes_outcome() {
    let session = "test:drain:once";
    stage_pending_outcome(
        session,
        make_outcome(Completion::Full, Recommend::Ship, vec![]),
    );
    assert!(drain_pending_outcome(session).is_some());
    // Second drain finds nothing — the stash is single-shot per turn.
    assert!(drain_pending_outcome(session).is_none());
    // Bookkeeping: TaskStatus enum is exposed for downstream callers.
    let _ = TaskStatus::Pending;
}
