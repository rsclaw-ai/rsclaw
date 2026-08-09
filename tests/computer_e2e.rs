//! End-to-end integration test for the new computer_use stack.
//! Tests NativeOperator screenshot + parser + prompt builder
//! without needing a vision LLM.

use std::{
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use rsclaw::computer::{
    action::{Action, ActionSpec, ExecCtx, Screenshot},
    app_rules::AppRuleSet,
    driver::{CoordSpace, DriverOutcome, VlmDriver},
    operator::{ActionFut, ActionOutput, FrontmostFut, Operator, ScreenshotFut},
    operators::native::NativeOperator,
    parser::{CoordFormat, parse_vlm_response},
    permission::{PermissionDecision, PermissionStore},
    prompt::{PlatformKind, PromptInputs, build_system_prompt},
};

// xcap::Monitor::all needs a real display (X11/Wayland/Quartz). GitHub
// Actions Linux runners are headless and fail with "Connection closed,
// error during parsing display string". Run manually with
// `cargo test --test computer_e2e -- --ignored` on a desktop machine.
#[tokio::test]
#[ignore]
async fn native_operator_screenshot_works() {
    let op = NativeOperator::new();
    let snap = op.screenshot().await.expect("xcap screenshot");

    assert!(!snap.png_bytes.is_empty(), "got empty png");
    assert!(
        snap.png_bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
        "not a PNG"
    );
    assert!(snap.physical_size.0 > 0);
    assert!(snap.physical_size.1 > 0);
    assert!(snap.scale_factor > 0.0);

    println!(
        "screenshot ok: {}x{} @{}x ({} bytes)",
        snap.physical_size.0,
        snap.physical_size.1,
        snap.scale_factor,
        snap.png_bytes.len(),
    );
}

// Real-display test: the permission gate's ground truth is whatever the
// OS reports as frontmost. If this query silently stops working (revoked
// Accessibility grant, unsigned rebuild, OS change), the gate degrades to
// "cannot tell" and the mid-run consent re-check goes dark — so it is
// worth pinning against a live desktop rather than only a mock.
#[tokio::test]
#[ignore]
async fn frontmost_app_returns_a_real_app_name() {
    let op = NativeOperator::new();
    let front = op.frontmost_app().await.expect("frontmost query must not error");

    let name = front.expect(
        "frontmost app should be reported on a signed macOS build with Accessibility \
         granted; None here means the permission re-check cannot function",
    );
    assert!(!name.trim().is_empty(), "frontmost app name was blank");
    // The test itself runs under some real GUI app (terminal / IDE).
    println!("frontmost app: {name}");
}

// The consent decision itself. A false positive here is exactly the hole
// the frontmost check exists to close, so assert both directions against
// the live app name rather than a fixture.
#[tokio::test]
#[ignore]
async fn frontmost_app_matches_itself_but_not_another_app() {
    use rsclaw::computer::operators::native::app_labels_match;

    let op = NativeOperator::new();
    let front = op
        .frontmost_app()
        .await
        .expect("frontmost query")
        .expect("frontmost app name");

    assert!(
        app_labels_match(&front, &front),
        "an app must match itself, or every keystroke would re-prompt"
    );
    assert!(
        !app_labels_match(&front, "1Password"),
        "consent for `{front}` must not clear an unrelated app"
    );
}

#[test]
fn parser_handles_real_world_vlm_output() {
    let model_output = "Thought: 用户要打开微信查看新消息。我应该先点击微信图标。\nAction: click(start_box='[120, 80, 220, 110]')";
    let parsed = parse_vlm_response(model_output, CoordFormat::Auto);
    assert_eq!(parsed.len(), 1);
    let action = &parsed[0];
    assert_eq!(action.action_type, "click");
    assert!(action.thought.contains("微信"));
    assert!(action.start.is_some());
}

#[test]
fn prompt_includes_all_sections() {
    let op = NativeOperator::new();
    let action_spaces = op.action_spaces();
    let app_rules_dir = std::env::home_dir()
        .map(|h| h.join(".rsclaw/tools/computer_use/app-rules"))
        .unwrap();
    let app_rules = AppRuleSet::load_dir(&app_rules_dir).unwrap_or_default();
    let matched: Vec<&_> = app_rules.match_instruction("微信群里看看新消息");

    let prompt = build_system_prompt(&PromptInputs {
        instruction: "微信群里看看新消息",
        action_spaces: &action_spaces,
        matched_rules: &matched,
        screen_size: Some((2880, 1800)),
        platform: PlatformKind::Desktop,
    });

    println!("--- generated prompt ({} chars) ---", prompt.len());
    println!("{}", prompt);
    println!("--- end ---");

    assert!(prompt.contains("You are a GUI agent"));
    assert!(prompt.contains("## Output Format"));
    assert!(prompt.contains("## Action Space"));
    // Action Space samples wrap coordinates in the portable `<box>x,y</box>`
    // form (R3 review C1). Prior versions used UI-TARS-tokenizer-specific
    // `<|box_start|>...<|box_end|>` markers — but those force every VLM
    // to have those special tokens in its vocabulary. The `<box>` form
    // is plain text any VLM can emit, and the parser (CoordFormat::Auto)
    // still accepts the legacy UiTarsBoxPair format as a fallback.
    assert!(prompt.contains("click(start_box='<box>x1,y1</box>')"));
    assert!(prompt.contains("## Note"));
    assert!(prompt.contains("Use Chinese in `Thought` part"));
    assert!(prompt.contains("## Thought Examples"));
    assert!(prompt.contains("RsClaw 测试群"));
    assert!(prompt.contains("## Coordinate Space"));
    // Coordinate Space switched to a resolution-independent 0-1000
    // normalized grid; the prompt no longer leaks the host's physical
    // pixel size since most VLM checkpoints train on the normalized
    // shape and don't need (or want) the raw screen extent.
    assert!(prompt.contains("0-1000 normalized grid"));
    assert!(prompt.contains("## Output Examples"));
    assert!(prompt.contains("## User Instruction"));
    assert!(prompt.contains("微信群里看看新消息"));
}

// ---------------------------------------------------------------------------
// Permission re-verification: does the gate actually BLOCK keyboard input
// when focus moves to an app the user never approved?
//
// This is the security property the frontmost check exists to provide, so
// it is asserted against the real driver loop rather than the helpers.
// Everything external is faked (no display, no VLM, no redb) so the test is
// deterministic and runs in CI.
// ---------------------------------------------------------------------------

/// Operator whose reported frontmost app is scripted per call, so a focus
/// change mid-run can be simulated exactly.
struct ScriptedOperator {
    /// One entry per `frontmost_app()` call; the last value repeats.
    focus: Vec<&'static str>,
    calls: AtomicUsize,
    /// Actions that actually reached the operator.
    executed: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Operator for ScriptedOperator {
    fn name(&self) -> &'static str {
        "scripted"
    }

    fn action_spaces(&self) -> Vec<ActionSpec> {
        vec![ActionSpec::new("type(content='')")]
    }

    fn frontmost_app(&self) -> FrontmostFut<'_> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let app = *self.focus.get(n).unwrap_or_else(|| {
            self.focus.last().expect("focus script must not be empty")
        });
        Box::pin(async move { Ok(Some(app.to_owned())) })
    }

    fn screenshot(&self) -> ScreenshotFut<'_> {
        Box::pin(async move {
            Ok(Screenshot {
                png_bytes: vec![0x89, 0x50, 0x4E, 0x47],
                logical_size: (1000, 1000),
                physical_size: (1000, 1000),
                scale_factor: 1.0,
            })
        })
    }

    fn execute<'a>(&'a self, action: &'a Action, _ctx: &'a ExecCtx) -> ActionFut<'a> {
        let label = format!("{action:?}");
        Box::pin(async move {
            self.executed.lock().expect("executed lock").push(label);
            Ok(ActionOutput::ok())
        })
    }
}

/// Permission store where only `allowed` is approved; everything else is
/// undecided (`None`), i.e. would require a fresh prompt.
struct FixedPermissions {
    allowed: &'static str,
}

impl PermissionStore for FixedPermissions {
    fn check<'a>(
        &'a self,
        _agent_id: &'a str,
        app: &'a str,
    ) -> rsclaw::computer::permission::CheckFut<'a> {
        let hit = app == self.allowed;
        Box::pin(async move {
            Ok(hit.then_some(PermissionDecision::AllowSession))
        })
    }

    fn record<'a>(
        &'a self,
        _agent_id: &'a str,
        _app: &'a str,
        _decision: PermissionDecision,
    ) -> rsclaw::computer::permission::RecordFut<'a> {
        Box::pin(async move { Ok(()) })
    }

    fn revoke<'a>(
        &'a self,
        _agent_id: &'a str,
        _app: &'a str,
    ) -> rsclaw::computer::permission::RecordFut<'a> {
        Box::pin(async move { Ok(()) })
    }

    fn bypass_all(&self) -> bool {
        false
    }
}

/// Provider that replays a fixed script of VLM responses, one per turn.
struct ScriptedProvider {
    turns: Vec<String>,
    calls: AtomicUsize,
}

impl rsclaw::provider::LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn stream(
        &self,
        req: rsclaw::provider::LlmRequest,
    ) -> futures::future::BoxFuture<'_, anyhow::Result<rsclaw::provider::LlmStream>> {
        // `finished(...)` is not terminal on its own: the driver re-asks the
        // model to confirm the end state from the screenshot and only stops
        // on a YES. That verifier shares this provider, so answer it
        // separately or the loop spins to MaxLoop.
        let is_verifier = req
            .system
            .as_deref()
            .is_some_and(|s| s.contains("strict verifier"));
        if is_verifier {
            let events = vec![
                Ok(rsclaw::provider::StreamEvent::TextDelta(
                    "YES - the requested end state is visible".to_owned(),
                )),
                Ok(rsclaw::provider::StreamEvent::Done { usage: None }),
            ];
            return Box::pin(async move {
                let s: rsclaw::provider::LlmStream =
                    Box::pin(futures::stream::iter(events)) as Pin<Box<_>>;
                Ok(s)
            });
        }

        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let text = self
            .turns
            .get(n)
            .cloned()
            .unwrap_or_else(|| "Thought: done\nAction: finished(content='done')".to_owned());
        Box::pin(async move {
            let events = vec![
                Ok(rsclaw::provider::StreamEvent::TextDelta(text)),
                Ok(rsclaw::provider::StreamEvent::Done { usage: None }),
            ];
            let s: rsclaw::provider::LlmStream =
                Box::pin(futures::stream::iter(events)) as Pin<Box<_>>;
            Ok(s)
        })
    }
}

fn drive(
    focus: Vec<&'static str>,
    turns: Vec<String>,
    allowed: &'static str,
) -> (DriverOutcome, Vec<String>) {
    let executed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let operator = ScriptedOperator {
        focus,
        calls: AtomicUsize::new(0),
        executed: Arc::clone(&executed),
    };
    let rules = AppRuleSet::default();
    let driver = VlmDriver {
        operator: &operator,
        provider: Arc::new(ScriptedProvider {
            turns,
            calls: AtomicUsize::new(0),
        }),
        model_name: "test".into(),
        coord_format: CoordFormat::Auto,
        coord_space: CoordSpace::Normalized,
        max_loop: 4,
        abort: Arc::new(AtomicBool::new(false)),
        app_rules: &rules,
        permission: Arc::new(FixedPermissions { allowed }),
        agent_id: "agent:test".into(),
        app: allowed.to_owned(),
        permission_emit: None,
        headless_auto_allow: false,
        status_emit: None,
        run_id: "test-run".into(),
        action_spaces_override: None,
    };

    let outcome = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(driver.run("type a message"))
        .expect("driver run");

    let done = executed.lock().expect("executed lock").clone();
    (outcome, done)
}

#[test]
fn typing_is_blocked_when_focus_moved_to_an_unapproved_app() {
    // Consent covers "Notes". Focus is Notes at the gate, then silently
    // becomes 1Password before the model types. Without the re-check the
    // keystrokes would land in the password manager.
    // `initial_consent_target` short-circuits on the declared label without
    // querying, so the very first frontmost_app() call is the pre-type
    // re-check: focus has already moved to 1Password by then.
    let (outcome, executed) = drive(
        vec!["1Password"],
        vec!["Thought: type\nAction: type(content='secret')".to_owned()],
        "Notes",
    );

    assert!(
        matches!(outcome, DriverOutcome::PermissionDenied),
        "focus moved to an unapproved app; run must abort, got {outcome:?}"
    );
    assert!(
        executed.is_empty(),
        "no keystroke may reach an unapproved app, but got: {executed:?}"
    );
}

#[test]
fn typing_proceeds_while_focus_stays_on_the_approved_app() {
    // Same script, focus never leaves Notes: the run must complete and the
    // keystrokes must actually execute. Guards against the fix degrading
    // into "deny everything", which would pass the test above trivially.
    let (outcome, executed) = drive(
        vec!["Notes"],
        vec![
            "Thought: type\nAction: type(content='hello')".to_owned(),
            "Thought: done\nAction: finished(content='ok')".to_owned(),
        ],
        "Notes",
    );

    assert!(
        matches!(outcome, DriverOutcome::Finished { .. }),
        "approved app must run to completion, got {outcome:?}"
    );
    assert_eq!(executed.len(), 1, "the type action should have executed");
    assert!(executed[0].contains("Type"), "got {executed:?}");
}
