//! System-prompt builder.
//!
//! Composes the prompt the VLM sees per turn:
//!   1. Base GUI-agent skeleton (Output Format, Note section).
//!   2. Operator's `action_spaces()` rendered into `## Action Space`.
//!   3. Matched app-rules bodies appended as `## App-Specific Rules`.
//!   4. Optional language hint — Thought-section language is picked
//!      from the user instruction (CJK → Chinese, else English).
//!
//! Keep the prompt compact; the model sees this every turn.

use std::fmt::Write;

use super::action::ActionSpec;
use super::app_rules::AppRule;

pub struct PromptInputs<'a> {
    pub instruction: &'a str,
    pub action_spaces: &'a [ActionSpec],
    pub matched_rules: &'a [&'a AppRule],
    /// Physical pixel dimensions of the screenshot the VLM will see
    /// each turn. When provided, the prompt explicitly tells the model
    /// the coordinate space is absolute pixels in this `WxH` plane —
    /// critical for general-purpose LLMs (kimi-for-coding, gpt-4o,
    /// claude vision) that don't know any normalized convention.
    pub screen_size: Option<(u32, u32)>,
}

/// Build the system prompt the VLM driver sends each turn. The final
/// section (`## User Instruction`) is included with the instruction
/// embedded — the driver does not need to append it again.
pub fn build_system_prompt(inputs: &PromptInputs) -> String {
    let lang = if has_cjk(inputs.instruction) {
        "Chinese"
    } else {
        "English"
    };

    let mut out = String::new();
    out.push_str(
        "You are a GUI agent driving the user's desktop directly. Each turn you receive a screenshot of the current screen and your action history, and you must output ONE next action to advance the task. You ARE the executor — there is no other tool / sub-agent to call.\n\n",
    );

    // Emit the coordinate-space contract FIRST so it precedes every
    // mention of x/y in the Action Space.
    if let Some((w, h)) = inputs.screen_size {
        let _ = writeln!(
            out,
            "## Coordinate Space\nThe screenshot you see each turn is {w}×{h} physical pixels. ALL coordinates in your Action output (`start_box`, `end_box`, etc.) MUST be absolute pixel values within this 0..{w} × 0..{h} plane. Do NOT use normalized 0-1000 ranges, percentages, or ratios. A click target at the very top-left of the screen is `[0, 0, ...]`; a target near the centre is `[{}, {}, ...]`.\n\n",
            w / 2,
            h / 2,
        );
    }

    out.push_str("## Output Format\n");
    out.push_str("Every reply MUST be exactly two lines:\n");
    out.push_str("```\nThought: <one-paragraph reasoning grounded in what you see in the screenshot>\nAction: <one call from the Action Space below>\n```\n\n");
    out.push_str("Do NOT propose calling any tool such as `computer_use`, `ui_tars`, `screenshot`, or `analyze`. Those are wrappers AROUND you — invoking them inside your output is a no-op that wastes a turn. The only valid output is the two-line Thought + Action pair using the Action Space.\n\n");

    out.push_str("## Action Space\n");
    for spec in inputs.action_spaces {
        out.push_str(&spec.render());
        out.push('\n');
    }
    out.push('\n');

    out.push_str("## Note\n");
    let _ = writeln!(out, "- Use {lang} in `Thought` part.");
    out.push_str(
        "- Write a small plan and finally summarize your next action (with its target element) in one sentence in `Thought` part.\n",
    );
    out.push_str(
        "- You may stumble upon new rules or features while driving the GUI. Record them in your `Thought` and reuse them later in the loop.\n",
    );
    out.push_str(
        "- Your thought style should follow the style of the Thought Examples below.\n",
    );
    out.push_str(
        "- If the screenshot is unhelpful (target app not visible), the correct next action is `activate_app(app='AppName')`, NOT prose about which tool to use.\n",
    );
    out.push_str(
        "- When the task is complete, end with `finished(content='...')`. When stuck or needing user input, end with `call_user(reason='...')`. These are terminal actions.\n",
    );

    out.push_str("\n## Thought Examples\n");
    out.push_str(if lang == "Chinese" {
        THOUGHT_EXAMPLES_ZH
    } else {
        THOUGHT_EXAMPLES_EN
    });

    out.push_str("\n## Output Examples\n");
    out.push_str(if lang == "Chinese" {
        OUTPUT_EXAMPLE_ZH
    } else {
        OUTPUT_EXAMPLE_EN
    });

    if !inputs.matched_rules.is_empty() {
        out.push_str("\n## App-Specific Rules\n");
        for rule in inputs.matched_rules {
            let _ = writeln!(out, "### {}", rule.name);
            let body = rule.body.trim_end_matches('\n');
            out.push_str(body);
            out.push('\n');
            out.push_str("---\n");
        }
    }

    out.push_str("\n## User Instruction\n");
    out.push_str(inputs.instruction);

    out
}

/// Thought-style examples grounded in real RsClaw use cases — WeChat
/// group monitoring, Doubao prompt entry, Douyin feed scrolling,
/// Tonghuashun watchlist navigation, and a failure-recovery example.
/// These give the model a concrete writing style without leaking the
/// game / puzzle bias of the upstream ui-tars-desktop examples.
const THOUGHT_EXAMPLES_ZH: &str = "\
Example 1 — App not visible yet:
Thought: 屏幕上是 Claude 桌面和几个终端窗口，看不到微信。任务是 \"在 RsClaw 测试群发问候\"，所以第一步必须把微信带到前台。直接调用 activate_app 比去 dock 上找图标更可靠。
Action: activate_app(app='WeChat')

Example 2 — Click search box to find group:
Thought: 微信主界面已打开，左侧最上方有搜索框，placeholder 是 \"搜索\"，坐标大约在屏幕左上角。任务是进 \"RsClaw 测试群\"，点搜索框输入群名比滚动会话列表更快。
Action: click(start_box='[60, 80, 200, 110]')

Example 3 — Type group name (Chinese):
Thought: 搜索框已经获得焦点（光标在闪），下一步直接敲群名。中文输入用 type(content='...')，enigo 会原样投递。
Action: type(content='RsClaw测试群')

Example 4 — Reply with quote, keyboard navigation:
Thought: 已经进入研发群，最新一条是 12:03 张三 \"@我 螃蟹有 bug\"。\"@我\" 命中关键词，需要引用回复。先在消息气泡上右键唤出菜单。
Action: right_single(start_box='[420, 360, 760, 410]')

Example 5 — Wait for context menu after right-click:
Thought: 刚才 right_single 之后 0.5s 内菜单还没渲染完，直接按 down 可能漏选。先等一下。
Action: wait(seconds=0.5)

Example 6 — Bottom-of-list detection, terminal action:
Thought: 上一次 scroll(direction='down') 后截图和之前几乎一致，连续两次同位置滚动都没变化，已到列表底。任务是 \"看新消息\"，已扫完，直接 finished。
Action: finished(content='已扫完该群所有可见消息，未见新增 @我 的内容')
";

const THOUGHT_EXAMPLES_EN: &str = "\
Example 1 — App not visible yet:
Thought: The screenshot shows Claude desktop and a few Terminal windows; WeChat is not visible. The task is \"send a hello in the RsClaw test group\", so the first step must bring WeChat to the front. activate_app is more reliable than hunting for the dock icon.
Action: activate_app(app='WeChat')

Example 2 — Click search box to find group:
Thought: WeChat's main window is up with the search box at the top of the left sidebar (placeholder \"搜索\"), roughly top-left of the screen. Clicking the search box and typing the group name is faster than scrolling the conversation list.
Action: click(start_box='[60, 80, 200, 110]')

Example 3 — Type a search query:
Thought: The search box is focused (cursor visible). Type the group name directly. type(content='...') passes the string through to enigo as-is.
Action: type(content='RsClaw test group')

Example 4 — Reply with quote, keyboard navigation:
Thought: Inside the R&D group, the latest message is 12:03 Zhang San \"@me crab bug\". The \"@me\" mention triggers reply mode. First, right-click the message bubble to open the context menu.
Action: right_single(start_box='[420, 360, 760, 410]')

Example 5 — Wait for context menu after right-click:
Thought: Half a second after right_single the menu may still be rendering; pressing down immediately could miss the first item. Wait briefly.
Action: wait(seconds=0.5)

Example 6 — Bottom-of-list detection, terminal action:
Thought: The screenshot after the last scroll(direction='down') is almost identical to the previous one, and two consecutive scrolls have produced no change — reached the end of the list. The task is \"check new messages\", scanning is complete, so terminate cleanly.
Action: finished(content='Scanned all visible messages; no new @-mentions for me.')
";

const OUTPUT_EXAMPLE_ZH: &str = "\
Thought: 这里写中文思考，按上面的 Thought Examples 风格 …
Action: click(start_box='[120, 80, 220, 110]')
";

const OUTPUT_EXAMPLE_EN: &str = "\
Thought: Write your English thought here, following the style of the Thought Examples above ...
Action: click(start_box='[120, 80, 220, 110]')
";

/// True if the string contains any common CJK / Hiragana / Katakana
/// codepoint, used to pick the Thought-section language.
fn has_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c,
            '\u{3040}'..='\u{309F}'   // Hiragana
            | '\u{30A0}'..='\u{30FF}' // Katakana
            | '\u{3400}'..='\u{4DBF}' // CJK Ext A
            | '\u{4E00}'..='\u{9FFF}' // CJK Unified
            | '\u{F900}'..='\u{FAFF}' // CJK Compat
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rule(name: &str, body: &str) -> AppRule {
        AppRule {
            name: name.to_string(),
            triggers: vec![name.to_string()],
            description: None,
            body: body.to_string(),
            path: PathBuf::from(format!("{name}.md")),
        }
    }

    #[test]
    fn empty_action_spaces_renders_empty_section() {
        let inputs = PromptInputs {
            instruction: "do a thing",
            action_spaces: &[],
            matched_rules: &[],
            screen_size: None,
        };
        let p = build_system_prompt(&inputs);
        assert!(p.contains("## Action Space\n\n"), "prompt was:\n{p}");
    }

    #[test]
    fn no_matched_rules_skips_section() {
        let inputs = PromptInputs {
            instruction: "do a thing",
            action_spaces: &[],
            matched_rules: &[],
            screen_size: None,
        };
        let p = build_system_prompt(&inputs);
        assert!(!p.contains("## App-Specific Rules"), "prompt was:\n{p}");
    }

    #[test]
    fn cjk_instruction_picks_chinese() {
        let inputs = PromptInputs {
            instruction: "微信群里看看",
            action_spaces: &[],
            matched_rules: &[],
            screen_size: None,
        };
        let p = build_system_prompt(&inputs);
        assert!(p.contains("Use Chinese in `Thought` part."), "prompt was:\n{p}");
    }

    #[test]
    fn english_instruction_picks_english() {
        let inputs = PromptInputs {
            instruction: "open WeChat and check messages",
            action_spaces: &[],
            matched_rules: &[],
            screen_size: None,
        };
        let p = build_system_prompt(&inputs);
        assert!(p.contains("Use English in `Thought` part."), "prompt was:\n{p}");
    }

    #[test]
    fn cjk_instruction_includes_chinese_thought_examples() {
        let inputs = PromptInputs {
            instruction: "微信群里看看",
            action_spaces: &[],
            matched_rules: &[],
            screen_size: None,
        };
        let p = build_system_prompt(&inputs);
        assert!(p.contains("## Thought Examples"), "missing section");
        assert!(p.contains("RsClaw 测试群"), "missing zh example body");
        assert!(p.contains("## Output Examples"), "missing output section");
    }

    #[test]
    fn english_instruction_includes_english_thought_examples() {
        let inputs = PromptInputs {
            instruction: "open WeChat",
            action_spaces: &[],
            matched_rules: &[],
            screen_size: None,
        };
        let p = build_system_prompt(&inputs);
        assert!(p.contains("## Thought Examples"), "missing section");
        assert!(p.contains("RsClaw test group"), "missing en example body");
        assert!(p.contains("## Output Examples"), "missing output section");
        // Sanity: zh body must NOT leak into the en variant.
        assert!(
            !p.contains("微信"),
            "zh content leaked into en prompt"
        );
    }

    #[test]
    fn matched_rules_bodies_separated_by_dashes() {
        let r1 = rule("alpha", "alpha body");
        let r2 = rule("beta", "beta body");
        let refs: Vec<&AppRule> = vec![&r1, &r2];
        let inputs = PromptInputs {
            instruction: "do",
            action_spaces: &[],
            matched_rules: &refs,
            screen_size: None,
        };
        let p = build_system_prompt(&inputs);
        assert!(p.contains("### alpha"), "prompt was:\n{p}");
        assert!(p.contains("alpha body"), "prompt was:\n{p}");
        assert!(p.contains("### beta"), "prompt was:\n{p}");
        assert!(p.contains("beta body"), "prompt was:\n{p}");
        // At least one separator between the rules.
        assert!(p.matches("\n---\n").count() >= 2, "prompt was:\n{p}");
    }
}
