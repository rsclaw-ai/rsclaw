//! End-to-end integration test for the new computer_use stack.
//! Tests NativeOperator screenshot + parser + prompt builder
//! without needing a vision LLM.

use rsclaw::computer::operators::native::NativeOperator;
use rsclaw::computer::operator::Operator;
use rsclaw::computer::parser::{parse_vlm_response, CoordFormat};
use rsclaw::computer::prompt::{build_system_prompt, PromptInputs};
use rsclaw::computer::app_rules::AppRuleSet;

#[tokio::test]
async fn native_operator_screenshot_works() {
    let op = NativeOperator::new();
    let snap = op.screenshot().await.expect("xcap screenshot");

    assert!(!snap.png_bytes.is_empty(), "got empty png");
    assert!(snap.png_bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]), "not a PNG");
    assert!(snap.physical_size.0 > 0);
    assert!(snap.physical_size.1 > 0);
    assert!(snap.scale_factor > 0.0);

    println!("screenshot ok: {}x{} @{}x ({} bytes)",
        snap.physical_size.0,
        snap.physical_size.1,
        snap.scale_factor,
        snap.png_bytes.len(),
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
    });

    println!("--- generated prompt ({} chars) ---", prompt.len());
    println!("{}", prompt);
    println!("--- end ---");

    assert!(prompt.contains("You are a GUI agent"));
    assert!(prompt.contains("## Output Format"));
    assert!(prompt.contains("## Action Space"));
    // Action Space samples wrap coordinates in `<|box_start|>...<|box_end|>`
    // markers — that's the chat-template tokenizer's bbox sentinel format,
    // distinct from the older `[x1,y1,x2,y2]` shape some VLMs accept. The
    // worker's tokenizer relies on the markers being present verbatim.
    assert!(prompt.contains("click(start_box='<|box_start|>(x1,y1)<|box_end|>')"));
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
