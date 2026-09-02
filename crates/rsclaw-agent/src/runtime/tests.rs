use rsclaw_config::schema::{ContextPruningConfig, HardClearConfig, SoftTrimConfig};
use rsclaw_provider::{Message, MessageContent, Role};
use rsclaw_skill::SkillRegistry;

use super::*;
use crate::context_mgr::msg_chars;

// ---------------------------------------------------------------
// resolve_default_workspace — locks in the rule that file tools
// (list_dir, search_file, search_content, read, write, edit, shell)
// default to the agent's workspace, NEVER to "." / CWD / $HOME.
// ---------------------------------------------------------------

#[test]
fn workspace_per_agent_override_beats_default_and_fallback() {
    let base = std::path::Path::new("/tmp/some-base");
    let got = resolve_default_workspace(Some("/agents/me"), Some("/agents/all"), base);
    assert_eq!(got, std::path::PathBuf::from("/agents/me"));
}

#[test]
fn workspace_global_default_used_when_no_per_agent_override() {
    let base = std::path::Path::new("/tmp/some-base");
    let got = resolve_default_workspace(None, Some("/agents/all"), base);
    assert_eq!(got, std::path::PathBuf::from("/agents/all"));
}

#[test]
fn workspace_falls_back_to_base_dir_join_workspace() {
    let base = std::path::Path::new("/tmp/some-base");
    let got = resolve_default_workspace(None, None, base);
    // The `main` agent in production hits this branch — it has no
    // per-agent override and the defaults.workspace isn't set, so
    // every file tool resolves to <base_dir>/workspace.
    assert_eq!(got, std::path::PathBuf::from("/tmp/some-base/workspace"));
}

#[test]
fn workspace_tilde_is_expanded() {
    let base = std::path::Path::new("/tmp/some-base");
    let got = resolve_default_workspace(Some("~/myws"), None, base);
    let home = dirs_next::home_dir().expect("home dir for test");
    assert_eq!(got, home.join("myws"));
    assert!(
        got.is_absolute(),
        "expanded ~ must produce an absolute path, got {got:?}"
    );
}

#[test]
fn workspace_never_returns_dot_or_cwd_when_unset() {
    // Regression guard: an earlier implementation called
    // `.to_str().unwrap_or(".")` on the fallback PathBuf — a path with
    // non-UTF-8 bytes would silently degrade to "." (the gateway's CWD)
    // and let every file tool escape the workspace. The helper must
    // never produce "." or a relative path.
    let base = std::path::Path::new("/some/abs/base");
    let got = resolve_default_workspace(None, None, base);
    assert_ne!(got, std::path::PathBuf::from("."));
    assert!(
        got.is_absolute(),
        "default workspace must be absolute, got {got:?}"
    );
}

#[test]
fn explicit_zero_max_tokens_omits_wire_cap() {
    let got = resolve_request_max_tokens(Some(0), None, None, "doubao", "doubao-seed-2.0-pro");
    assert_eq!(got, None);
}

#[test]
fn skill_list_filters_and_paginates_results() {
    let mut skills = SkillRegistry::new();
    skills.insert(rsclaw_skill::SkillManifest {
        name: "douyin-publish".to_owned(),
        description: Some("Publish videos to Douyin".to_owned()),
        version: None,
        requires_rsclaw: None,
        tools: vec![],
        extra: Default::default(),
        dir: Default::default(),
        prompt: String::new(),
    });
    skills.insert(rsclaw_skill::SkillManifest {
        name: "weather".to_owned(),
        description: Some("Forecast lookup".to_owned()),
        version: None,
        requires_rsclaw: None,
        tools: vec![],
        extra: Default::default(),
        dir: Default::default(),
        prompt: String::new(),
    });
    skills.insert(rsclaw_skill::SkillManifest {
        name: "douyin-comments".to_owned(),
        description: Some("Read Douyin comments".to_owned()),
        version: None,
        requires_rsclaw: None,
        tools: vec![],
        extra: Default::default(),
        dir: Default::default(),
        prompt: String::new(),
    });
    let result = crate::tools_skill::paginate_skill_list(
        skills.all(),
        &json!({"query": "douyin", "limit": 1, "offset": 1}),
    );

    assert_eq!(result["count"], 3);
    assert_eq!(result["matched"], 2);
    assert_eq!(result["offset"], 1);
    assert_eq!(result["limit"], 1);
    assert_eq!(result["has_more"], false);
    assert_eq!(result["next_offset"], Value::Null);
    let skills = result["skills"].as_array().unwrap();
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0]["name"], "douyin-publish");
}

#[test]
fn resolve_or_create_collection_creates_then_reuses_by_name() {
    let tmp = tempfile::TempDir::new().unwrap();
    let kb = rsclaw_kb::KnowledgeService::open(tmp.path().join("kb")).unwrap();

    // First call creates it.
    let (id1, name1, created1) =
        crate::tools_memory::resolve_or_create_collection(&kb, "会议记录", None).unwrap();
    assert!(created1);
    assert_eq!(name1, "会议记录");

    // Second call reuses the same collection (no duplicate).
    let (id2, _n, created2) =
        crate::tools_memory::resolve_or_create_collection(&kb, "会议记录", None).unwrap();
    assert!(!created2);
    assert_eq!(id1, id2);

    // Case-insensitive match for ASCII names.
    let (_, _, created3) =
        crate::tools_memory::resolve_or_create_collection(&kb, "notes", None).unwrap();
    let (_, _, created4) =
        crate::tools_memory::resolve_or_create_collection(&kb, "NOTES", None).unwrap();
    assert!(created3 && !created4);
}

fn text_msg(role: Role, text: &str) -> Message {
    Message {
        role,
        content: MessageContent::Text(text.to_owned()),
        rsclaw_hidden: None,
    }
}

// ------------------------------------------------------------------
// msg_chars
// ------------------------------------------------------------------

#[test]
fn msg_chars_text_variant() {
    let m = text_msg(Role::User, "hello");
    assert_eq!(msg_chars(&m), 5);
}

#[test]
fn default_memory_scope_keeps_user_turns_in_agent_scope() {
    assert_eq!(
        crate::tools_memory::default_memory_scope("main", "chat"),
        "agent:main"
    );
    assert_eq!(
        crate::tools_memory::default_memory_scope("main", "a2a"),
        "agent:main"
    );
    assert_eq!(
        crate::tools_memory::default_memory_scope("main", "cron"),
        "agent:main:cron"
    );
    assert_eq!(
        crate::tools_memory::default_memory_scope("main", "heartbeat"),
        "agent:main:heartbeat"
    );
}

#[test]
fn normalize_memory_scope_accepts_legacy_bare_agent_id() {
    assert_eq!(
        crate::tools_memory::normalize_memory_scope("main", "main"),
        "agent:main"
    );
    assert_eq!(
        crate::tools_memory::normalize_memory_scope("agent:main", "main"),
        "agent:main"
    );
    assert_eq!(
        crate::tools_memory::normalize_memory_scope("global", "main"),
        "global"
    );
}

#[test]
fn format_kb_recall_block_budgets_and_cites_titles() {
    let hit = |title: &str, text: &str| rsclaw_kb::service::SearchHit {
        doc_id: "d1".into(),
        collection_id: None,
        collection_name: None,
        source_title: title.into(),
        chunk_text: text.into(),
        score: 0.5,
    };
    // Normal case: titles present, both hits fit.
    let block = crate::tools_memory::format_kb_recall_block(
        &[hit("年报2025", "营收增长12%"), hit("", "无标题文档的内容")],
        600,
    );
    assert!(block.contains("(年报2025) 营收增长12%"), "{block}");
    assert!(block.contains("(untitled) 无标题文档的内容"), "{block}");

    // Tight budget: a single oversized hit is clipped, not dropped —
    // otherwise one long chunk blanks the whole block.
    let long = "很".repeat(2000);
    let clipped = crate::tools_memory::format_kb_recall_block(&[hit("长文", &long)], 64);
    assert!(!clipped.is_empty());
    assert!(clipped.len() < long.len());

    // No hits → empty string (caller skips injection entirely).
    assert_eq!(crate::tools_memory::format_kb_recall_block(&[], 600), "");
}

#[test]
fn recall_bundle_from_docs_is_raw_context_with_metadata() {
    let docs = vec![
        crate::memory::MemoryDoc {
            id: "note-1".into(),
            scope: "agent:main".into(),
            kind: "note".into(),
            text: "在吗".into(),
            vector: vec![],
            created_at: 0,
            accessed_at: 0,
            access_count: 0,
            importance: 0.1,
            tier: crate::memory::MemDocTier::Peripheral,
            abstract_text: None,
            overview_text: None,
            tags: vec![],
            pinned: false,
        },
        crate::memory::MemoryDoc {
            id: "entity-1".into(),
            scope: "agent:main".into(),
            kind: "entity".into(),
            text: "用户手机号: 13900001234".into(),
            vector: vec![],
            created_at: 0,
            accessed_at: 0,
            access_count: 0,
            importance: 0.95,
            tier: crate::memory::MemDocTier::Core,
            abstract_text: None,
            overview_text: None,
            tags: vec!["pinned".into()],
            pinned: true,
        },
    ];

    let bundle =
        crate::tools_memory::recall_bundle_from_docs(docs, 1200, "trace-1").expect("bundle");
    assert_eq!(bundle.context, "- 用户手机号: 13900001234");
    assert!(!bundle.context.contains("<recall>"));
    assert_eq!(bundle.metadata.doc_ids, vec!["entity-1"]);
    assert_eq!(bundle.metadata.mode, "committed");
    assert_eq!(bundle.metadata.format, "xml");
    assert_eq!(bundle.metadata.source, "server");
    assert_eq!(bundle.metadata.trace_id.as_deref(), Some("trace-1"));
    assert_eq!(bundle.metadata.max_tokens, Some(1200));
    assert!(bundle.metadata.hash.starts_with("sha256:"));
    assert!(!bundle.metadata.truncated);
}

#[test]
fn msg_chars_parts_variant() {
    let m = Message {
        role: Role::Assistant,
        content: MessageContent::Parts(vec![
            ContentPart::Text {
                text: "abc".to_owned(),
            },
            ContentPart::Text {
                text: "de".to_owned(),
            },
        ]),
        rsclaw_hidden: None,
    };
    assert_eq!(msg_chars(&m), 5);
}

// ------------------------------------------------------------------
// apply_context_pruning — hard clear
// ------------------------------------------------------------------

#[test]
fn hard_clear_removes_all_but_last_user() -> anyhow::Result<()> {
    let mut msgs = vec![
        text_msg(Role::User, &"u".repeat(50_000)),
        text_msg(Role::Assistant, &"a".repeat(50_000)),
        text_msg(Role::Tool, &"t".repeat(50_000)),
        text_msg(Role::User, "last user message"),
    ];

    let cfg = ContextPruningConfig {
        mode: None,
        ttl: None,
        keep_last_assistants: None,
        min_prunable_tool_chars: None,
        soft_trim: None,
        hard_clear: Some(HardClearConfig {
            enabled: Some(true),
            threshold: Some(100_000),
        }),
        tools: None,
    };

    apply_context_pruning(&mut msgs, Some(&cfg));

    assert_eq!(msgs.len(), 1, "hard clear should leave only one message");
    assert_eq!(msgs[0].role, Role::User);
    match &msgs[0].content {
        MessageContent::Text(t) => assert_eq!(t, "last user message"),
        other => return Err(anyhow::anyhow!("expected Text content, got {:?}", other)),
    }
    Ok(())
}

// ------------------------------------------------------------------
// apply_context_pruning — soft trim removes large Tool messages
// ------------------------------------------------------------------

#[test]
fn soft_trim_removes_large_tool_messages() {
    let large_tool = "x".repeat(2_000);
    let mut msgs = vec![
        text_msg(Role::User, "hi"),
        text_msg(Role::Tool, &large_tool),
        text_msg(Role::Assistant, "response"),
    ];

    let cfg = ContextPruningConfig {
        mode: None,
        ttl: None,
        keep_last_assistants: None,
        min_prunable_tool_chars: Some(500),
        soft_trim: Some(SoftTrimConfig {
            enabled: Some(true),
            head_chars: None,
            tail_chars: Some(500), // well below total so trim fires
        }),
        hard_clear: None,
        tools: None,
    };

    apply_context_pruning(&mut msgs, Some(&cfg));

    // The large Tool message should have been removed.
    let has_tool = msgs.iter().any(|m| m.role == Role::Tool);
    assert!(!has_tool, "large Tool message should have been pruned");
}

// ------------------------------------------------------------------
// build_tool_list always contains the built-in tools
// ------------------------------------------------------------------

#[test]
fn build_tool_list_contains_builtins() {
    let skills = SkillRegistry::new();
    let tools = build_tool_list(&skills, None, "test-agent", &[]);
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    for expected in &[
        "memory",
        "session",
        "agent",
        "channel",
        "read_file",
        "write_file",
        "shell",
    ] {
        assert!(
            names.contains(expected),
            "expected built-in tool `{expected}` in tool list, got: {names:?}"
        );
    }
}

// ------------------------------------------------------------------
// is_internal_session vs is_minimal_context_session
// ------------------------------------------------------------------

#[test]
fn is_internal_session_classifies_ephemeral_prefixes() {
    assert!(is_internal_session("heartbeat:tick-42"));
    assert!(is_internal_session("cron:morning-briefing"));
    assert!(is_internal_session("system:bootstrap"));
    assert!(!is_internal_session("agent:main:telegram:direct:u1"));
    assert!(!is_internal_session("hook:abcd"));
    assert!(!is_internal_session("session:my-named"));
}

#[test]
fn intermediate_notification_text_rejects_whitespace_only_text() {
    assert_eq!(intermediate_notification_text("\n"), None);
    assert_eq!(intermediate_notification_text(" \t\n"), None);
}

#[test]
fn intermediate_notification_text_preserves_real_text_without_mutating_source() {
    let source = "\n正在查看屏幕。\n";
    assert_eq!(
        intermediate_notification_text(source),
        Some("正在查看屏幕。")
    );
    assert_eq!(source, "\n正在查看屏幕。\n");
}

#[test]
fn is_minimal_context_session_excludes_cron() {
    // Heartbeat / system: minimal prompt + memory-only tool set.
    assert!(is_minimal_context_session("heartbeat:tick-42"));
    assert!(is_minimal_context_session("system:bootstrap"));
    // Cron-fired agentTurn must run with the full agent context, even
    // though the session is ephemeral. Regression guard for the
    // "HEARTBEAT_OK" reply bug where cron jobs got the minimal prompt.
    assert!(!is_minimal_context_session("cron:morning-briefing"));
    assert!(!is_minimal_context_session("agent:main:telegram:direct:u1"));
}

// ------------------------------------------------------------------
// model_supports_image_input — schema-driven vision-capability lookup
// ------------------------------------------------------------------

fn build_config_with_models(
    provider_name: &str,
    models: Vec<rsclaw_config::schema::ModelDef>,
) -> rsclaw_config::schema::Config {
    use rsclaw_config::schema::{ApiFormat, Config, ModelsConfig, ProviderConfig};
    let pc = ProviderConfig {
        base_url: None,
        api_key: None,
        api: Some(ApiFormat::OpenAiCompletions),
        models: Some(models),
        enabled: Some(true),
        user_agent: None,
        prefix_id: None,
        compact_timeout_secs: None,
        constrain_tool_calls: None,
    };
    let mut providers = std::collections::HashMap::new();
    providers.insert(provider_name.to_owned(), pc);
    Config {
        models: Some(ModelsConfig {
            mode: None,
            providers,
            retry: None,
        }),
        ..Config::default()
    }
}

fn model_def(
    id: &str,
    inputs: Option<Vec<rsclaw_config::schema::InputType>>,
) -> rsclaw_config::schema::ModelDef {
    rsclaw_config::schema::ModelDef {
        id: id.to_owned(),
        name: None,
        reasoning: None,
        input: inputs,
        cost: None,
        context_window: None,
        max_tokens: None,
        enabled: None,
    }
}

#[test]
fn model_supports_image_input_explicit_image() {
    use rsclaw_config::schema::InputType;
    let cfg = build_config_with_models(
        "kimi",
        vec![model_def(
            "kimi-for-coding",
            Some(vec![InputType::Text, InputType::Image]),
        )],
    );
    // Both qualified and unqualified lookups resolve.
    assert_eq!(
        model_supports_image_input(&cfg, "kimi/kimi-for-coding"),
        Some(true)
    );
    assert_eq!(
        model_supports_image_input(&cfg, "kimi-for-coding"),
        Some(true)
    );
}

#[test]
fn model_supports_image_input_text_only() {
    use rsclaw_config::schema::InputType;
    let cfg = build_config_with_models(
        "deepseek",
        vec![model_def("deepseek-chat", Some(vec![InputType::Text]))],
    );
    assert_eq!(
        model_supports_image_input(&cfg, "deepseek/deepseek-chat"),
        Some(false)
    );
}

#[test]
fn model_supports_image_input_no_input_field_returns_none() {
    let cfg = build_config_with_models("kimi", vec![model_def("kimi-for-coding", None)]);
    // input field absent → caller should fall back to blocklist.
    assert_eq!(
        model_supports_image_input(&cfg, "kimi/kimi-for-coding"),
        None
    );
}

#[test]
fn model_supports_image_input_unknown_model_returns_none() {
    use rsclaw_config::schema::InputType;
    let cfg = build_config_with_models(
        "kimi",
        vec![model_def("kimi-for-coding", Some(vec![InputType::Image]))],
    );
    assert_eq!(model_supports_image_input(&cfg, "openai/gpt-4"), None);
}

// ------------------------------------------------------------------
// is_known_vision_model — built-in allow-list
// ------------------------------------------------------------------

#[test]
fn is_known_vision_model_kimi_family() {
    // kimi-for-coding ships vision tuning.
    assert!(is_known_vision_model("kimi/kimi-for-coding"));
    assert!(is_known_vision_model("kimi-for-coding"));
    // K2.5+ series is multimodal; older K2.x (K2.0..=K2.4) is not.
    assert!(is_known_vision_model("kimi/kimi-k2.5"));
    assert!(is_known_vision_model("kimi/kimi-k2.6-preview"));
    assert!(is_known_vision_model("kimi/kimi-k2.7"));
    // Pre-2.5 must NOT match.
    assert!(!is_known_vision_model("kimi/kimi-k2.0"));
    assert!(!is_known_vision_model("kimi/kimi-k1"));
}

#[test]
fn is_known_vision_model_major_vlms() {
    for name in [
        // International
        "openai/gpt-4o",
        "openai/gpt-4-vision-preview",
        "openai/gpt-5",
        "anthropic/claude-3-opus",
        "anthropic/claude-sonnet-4-5",
        "anthropic/claude-4-7",
        "google/gemini-1.5-pro",
        "google/gemini-3-ultra",
        "google/gemma-3-27b-it",
        "google/gemma-4-9b",
        "google/paligemma-3b-mix",
        "meta/llama-3.2-90b-vision-instruct",
        "meta/llama-4-scout-17b",
        "mistral/pixtral-12b",
        "mistral/mistral-small-3.1-24b",
        "cohere/aya-vision-32b",
        "xai/grok-3",
        "xai/grok-4-fast",
        // Chinese — ByteDance / Alibaba / Moonshot / Zhipu / Baidu / 01 / Baichuan / DeepSeek
        // / Tencent / MiniMax / StepFun
        "doubao/doubao-seed-1.5-vision-pro",
        "doubao/doubao-seed-1.6-vision-thinking",
        // Doubao Seed 2+ — entire 2.x / 3.x / ... subtree is multimodal
        "doubao/doubao-seed-2.0-pro",
        "doubao/doubao-seed-2.0-lite",
        "doubao/doubao-seed-2.0-code",
        "doubao/doubao-seed-2.0-vision",
        "doubao/doubao-seed-2.0-flash",
        "doubao/doubao-seed-2.5-pro", // future minor
        "doubao/doubao-seed-3.0-pro", // future major (auto-covered)
        "doubao/doubao-seed-4-omni",
        "doubao/doubao-vision",
        "doubao/seedream",
        "qwen/qwen-vl-plus",
        "qwen/qwen2.5-vl-72b",
        "qwen/qwen3-vl-30b",
        "qwen/qwen3.5-instruct",
        "qwen/qwen-3.6-pro",
        "qwen/qvq-72b-preview",
        "kimi/kimi-for-coding",
        "kimi/kimi-k2.5",
        "kimi/kimi-k2.6-preview",
        "kimi/kimi-vl-thinking",
        "zhipu/glm-4v-9b",
        "zhipu/glm-4.5v",
        "zhipu/cogagent-9b",
        "baidu/ernie-4.5-vl-424b",
        "baidu/ernie-5-pro",
        "sensetime/sensenova-v6-pro",
        "01-ai/yi-vl-34b",
        "baichuan/baichuan-omni-1.5",
        "deepseek/deepseek-vl2",
        "deepseek/janus-pro-7b",
        "tencent/hunyuan-vision",
        "minimax/minimax-vl-01",
        "stepfun/step-1o-vision-32k",
        "stepfun/step-3",
        // Open-source
        "liuhaotian/llava-1.6-34b",
        "opengvlab/internvl3-78b",
        "openbmb/minicpm-v-2.6",
        "microsoft/phi-3-vision-128k",
        "microsoft/florence-2-large",
        "huggingfaceh4/idefics3-8b",
        "huggingfaceh4/smolvlm-instruct",
        "nvidia/nvila-15b",
        // GUI-agent VLMs
        "bytedance/ui-tars-1.5-7b",
        "bytedance/ui-tars-2",
        "showui-2b",
        "os-atlas-pro-7b",
        // Universal suffix matchers
        "anything-with-vision-suffix",
        "weird-foo-omni",
    ] {
        assert!(is_known_vision_model(name), "should match: {name}");
    }
}

#[test]
fn is_known_vision_model_text_only_returns_false() {
    for name in [
        // OpenAI text-only
        "openai/gpt-3.5-turbo",
        "openai/gpt-4", // bare GPT-4 base is text-only
        "openai/text-davinci-003",
        // Anthropic legacy
        "anthropic/claude-2.1",
        "anthropic/claude-instant-1",
        // DeepSeek non-VL
        "deepseek/deepseek-chat",
        "deepseek/deepseek-reasoner",
        "deepseek/deepseek-coder",
        "deepseek/deepseek-v3",
        // Doubao text-only
        "doubao/doubao-seed-1.6", // text variant; only -vision suffix is multimodal
        "doubao/doubao-pro-256k",
        "doubao/doubao-lite",
        // Qwen text-only (pre-3.5)
        "qwen/qwen-turbo",
        "qwen/qwen-max",
        "qwen/qwen-plus",
        "qwen/qwen3.0",
        "qwen/qwen3.4",
        "qwen/qwen-3.4-instruct",
        "qwen/qwen3-coder", // coder is text-only
        // Pre-3 Gemma
        "google/gemma-2-9b",
        "google/gemma-1-7b",
        // Llama text-only
        "meta/llama-3-70b",
        "meta/llama-3.1-405b",
        "meta/llama-3.2-3b", // small Llama 3.2 are text
        // Mistral text-only
        "mistral/mistral-7b-instruct",
        "mistral/mixtral-8x7b",
        "mistral/codestral-22b",
        "mistral/mistral-large-2411",
        // Kimi pre-2.5
        "kimi/kimi-k1",
        "kimi/kimi-k2.0",
        "kimi/kimi-k2.4",
        "kimi/moonshot-v1-128k", // base v1 is text without -vision
        // Zhipu text-only (no v suffix)
        "zhipu/glm-4-flash",
        "zhipu/glm-4.5",
        "zhipu/glm-5", // bare GLM-5 (the VL variant is glm-5v)
        // Baidu text-only
        "baidu/ernie-3.5-128k",
        "baidu/ernie-4.0-turbo",
        "baidu/ernie-speed",
        // Yi text-only
        "01-ai/yi-large",
        "01-ai/yi-lightning",
        // Baichuan text-only
        "baichuan/baichuan2-13b",
        "baichuan/baichuan4",
        // Hunyuan text-only
        "tencent/hunyuan-large",
        "tencent/hunyuan-t1",
        // MiniMax text-only — including base M2 / M2.5 / M2.7
        // (despite "native multimodal" marketing, third-party
        // testing confirms text-only input).
        "minimax/abab6.5-chat",
        "minimax/minimax-m1",
        "minimax/minimax-m2",
        "minimax/minimax-m2.5",
        "minimax/minimax-m2.7",
        "minimax/minimax-m3-base",
        // StepFun text-only
        "stepfun/step-1-128k",
        "stepfun/step-2-mini",
        // SmolLM (NOT SmolVLM)
        "huggingfaceh4/smollm-1.7b",
        "huggingfaceh4/smollm2-1.7b",
        // MiniCPM bare (NOT minicpm-v)
        "openbmb/minicpm-2b",
        "openbmb/minicpm3-4b",
        // Phi text-only
        "microsoft/phi-3-mini-4k",
        "microsoft/phi-4", // bare phi-4 is text; phi-4-multimodal is vision
        // Generic / unknown model — defaults to text-only.
        "some-new-llm/v1",
        "future-vendor/futurelm-2030",
    ] {
        assert!(
            !is_known_vision_model(name),
            "should NOT match (false positive): {name}"
        );
    }
}

// ---------------------------------------------------------------------
// plugin_search pure-helper tests (Task 1)
// ---------------------------------------------------------------------

fn pti(plugin: &str, tool: &str, desc: &str) -> PluginToolInfo {
    PluginToolInfo {
        plugin: plugin.to_owned(),
        runtime: "wasm",
        tool: tool.to_owned(),
        description: desc.to_owned(),
        input_schema: json!({"type": "object"}),
    }
}

#[test]
fn search_empty_query_with_plugin_lists_all_alphabetical() {
    let tools = vec![
        pti("demo", "zeta", ""),
        pti("demo", "alpha", ""),
        pti("demo", "mid", ""),
        pti("other", "noise", ""),
    ];
    let result =
        AgentRuntime::search_plugin_tools_pure(tools, &json!({"plugin": "demo", "query": ""}));
    assert_eq!(result["mode"], "list");
    assert_eq!(result["total"], 3);
    let names: Vec<&str> = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["tool"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    assert!(result.get("error").is_none());
}

#[test]
fn search_empty_query_no_plugin_errors() {
    let result = AgentRuntime::search_plugin_tools_pure(vec![], &json!({"query": ""}));
    assert!(result.get("error").is_some());
}

#[test]
fn search_supports_offset_pagination() {
    let tools = (0..5)
        .map(|i| pti("demo", &format!("t{i}"), ""))
        .collect::<Vec<_>>();
    let result = AgentRuntime::search_plugin_tools_pure(
        tools,
        &json!({"plugin": "demo", "query": "", "offset": 2, "limit": 2}),
    );
    let names: Vec<&str> = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["tool"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["t2", "t3"]);
    assert_eq!(result["next_offset"], json!(4));
}

#[test]
fn search_last_page_has_null_next_offset() {
    let tools = (0..3)
        .map(|i| pti("demo", &format!("t{i}"), ""))
        .collect::<Vec<_>>();
    let result = AgentRuntime::search_plugin_tools_pure(
        tools,
        &json!({"plugin": "demo", "query": "", "offset": 2, "limit": 5}),
    );
    assert_eq!(result["tools"].as_array().unwrap().len(), 1);
    assert_eq!(result["next_offset"], Value::Null);
}

// ---------------------------------------------------------------------
// PluginOverride resolver tests (Task 2)
// ---------------------------------------------------------------------

// render_active_plugin_tools_text orchestration is covered by
// integration testing via the /plugin slash command. Unit-testing it
// requires constructing a real WasmPlugin (Engine + Component + Linker),
// which is infeasible in a lib test. The logic that *can* fail in
// isolation — override resolution — is covered by the resolver tests
// below.

#[test]
fn resolve_inject_returns_none_when_no_override() {
    let overrides: std::collections::HashMap<String, PluginOverride> = Default::default();
    let r = AgentRuntime::resolve_plugin_inject_pure(&overrides, "douyin");
    assert_eq!(r, PluginInjectResolution::None);
}

#[test]
fn resolve_inject_returns_names_when_explicit() {
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(
        "douyin".to_owned(),
        PluginOverride {
            inject: vec!["publish".into(), "list".into()],
            ..Default::default()
        },
    );
    let r = AgentRuntime::resolve_plugin_inject_pure(&overrides, "douyin");
    assert_eq!(
        r,
        PluginInjectResolution::Names(vec!["publish".into(), "list".into()])
    );
}

#[test]
fn resolve_inject_returns_all_when_inject_all() {
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(
        "douyin".to_owned(),
        PluginOverride {
            inject_all: true,
            ..Default::default()
        },
    );
    let r = AgentRuntime::resolve_plugin_inject_pure(&overrides, "douyin");
    assert_eq!(r, PluginInjectResolution::All);
}

#[test]
fn resolve_inject_returns_none_when_disabled() {
    // disabled wins over inject / inject_all.
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(
        "douyin".to_owned(),
        PluginOverride {
            disabled: true,
            inject_all: true,
            inject: vec!["publish".into()],
            ..Default::default()
        },
    );
    let r = AgentRuntime::resolve_plugin_inject_pure(&overrides, "douyin");
    assert_eq!(r, PluginInjectResolution::None);
}

// ---------------------------------------------------------------------
// Qualified tool name parsing (`<plugin>__<tool>` and legacy forms)
// ---------------------------------------------------------------------

#[test]
fn parse_qualified_tool_canonical_double_underscore() {
    let r = super::parse_qualified_tool("douyin__publish");
    assert_eq!(r, Some(("douyin".into(), "publish".into())));
}

#[test]
fn parse_qualified_tool_legacy_dot_separator() {
    // Old `model.plugin_tools` configs used the dotted form;
    // accept it for backward compat.
    let r = super::parse_qualified_tool("douyin.publish");
    assert_eq!(r, Some(("douyin".into(), "publish".into())));
}

#[test]
fn parse_qualified_tool_legacy_slash_separator() {
    // Operators muscle-memory from skill paths sometimes use /.
    let r = super::parse_qualified_tool("douyin/publish");
    assert_eq!(r, Some(("douyin".into(), "publish".into())));
}

#[test]
fn parse_qualified_tool_double_underscore_wins_over_dot() {
    // When both separators are present in a tool name we prefer
    // the canonical form so a tool literally named `foo.bar`
    // inside plugin `p` (`p__foo.bar`) resolves correctly.
    let r = super::parse_qualified_tool("p__foo.bar");
    assert_eq!(r, Some(("p".into(), "foo.bar".into())));
}

#[test]
fn parse_qualified_tool_returns_none_without_separator() {
    assert_eq!(super::parse_qualified_tool("publish"), None);
    assert_eq!(super::parse_qualified_tool(""), None);
}

#[test]
fn bucket_qualified_names_groups_by_plugin() {
    let entries = vec![
        "douyin__publish".to_owned(),
        "douyin.list_my_videos".to_owned(), // legacy form, same plugin
        "jimeng__image_txt2img".to_owned(),
        "garbage_no_separator".to_owned(), // dropped silently
    ];
    let buckets = super::bucket_qualified_names(&entries);
    assert_eq!(buckets.len(), 2);
    let douyin = buckets.get("douyin").expect("douyin bucket present");
    assert_eq!(douyin.len(), 2);
    assert!(douyin.contains("publish"));
    assert!(douyin.contains("list_my_videos"));
    let jimeng = buckets.get("jimeng").expect("jimeng bucket present");
    assert!(jimeng.contains("image_txt2img"));
}

#[test]
fn plugin_user_tool_selection_wire_name_uses_double_underscore() {
    let sel = super::PluginUserToolSelection {
        plugin_name: "douyin".into(),
        tool_name: "publish".into(),
        description: String::new(),
        input_schema: json!({}),
        group: None,
    };
    assert_eq!(sel.wire_name(), "douyin__publish");
}

#[test]
fn search_query_mode_scores_and_paginates() {
    let tools = vec![
        pti("demo", "publish_video", "Publish a video"),
        pti("demo", "edit_video", "Edit a video"),
        pti("demo", "add_account", "Manage account"),
    ];
    let result =
        AgentRuntime::search_plugin_tools_pure(tools, &json!({"query": "video", "limit": 5}));
    assert_eq!(result["mode"], "search");
    let names: Vec<&str> = result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["tool"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 2);
    assert!(names.contains(&"publish_video"));
    assert!(names.contains(&"edit_video"));
}
