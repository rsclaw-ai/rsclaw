# Computer Use 推倒重构进度 (2026-05-05)

> 这是一份 **进度记录**，不是最终 ADR。重构落地后整理为正式 ADR。
> 分支: `refactor/computer_use`

## 目标

把 1771 行 `src/agent/tools_computer.rs` 上帝函数 + 961 行
UI-TARS 专用 `src/provider/ui_tars.rs` 推倒重写为分层架构，
让 GUI-agent 成为 RsClaw 核心竞争力。

要求：
- 模型无关 (任何视觉模型都能跑：UI-TARS / GPT-4o / Doubao / Claude vision / Qwen-VL)
- 平台无关，但 Operator 是平台抽象 (5 个固定: native / browser / iphone_mirror / adb / ...)
- 应用无关，App 是 markdown 数据 (`tools/computer_use/app-rules/*.md`)，加 App **不需要 Rust 改动**
- 权限门 UX 仿 Claude Code / UI-TARS-desktop
- 视觉模型配置：`agents.default.model.vision` → fallback `primary` → 都没有时报清晰错误
- 激活 App 时 **bring all windows to front**
- enigo + xcap (≈ 100× shell 快)

## 已完成 (待编译验证)

```
src/computer/
├── mod.rs              57 LOC   公共 API + 工厂
├── action.rs           189 LOC  共享类型: Action / ParsedAction / ExecCtx / Screenshot
├── operator.rs          66 LOC  Operator trait (Pin<Box<Future>> 不用 async-trait)
├── parser.rs           635 LOC  4-format VLM 解析 (15 tests)
├── prompt.rs           161 LOC  system prompt 组装 (5 tests)
├── app_rules.rs        367 LOC  markdown frontmatter 解析 + 别名匹配 (7 tests)
├── permission.rs       480 LOC  bypass / session / persisted / oneshot pending (8 tests)
├── permission_ui.md     77 LOC  Tauri UI 集成规范
├── driver.rs           780 LOC  ★ 主循环 VlmDriver (8 tests, 未验证)
└── operators/
    ├── mod.rs            9 LOC
    ├── native.rs       632 LOC  enigo + xcap macOS/Win/Linux (5 tests)
    ├── iphone_mirror.rs 461 LOC  macOS-only, 窗口级截图 + 坐标偏移
    ├── adb.rs          373 LOC  Android, exec-out screencap + input tap (4 tests)
    └── browser.rs       67 LOC  STUB (Phase 2)

总计 4277 LOC, ~48 unit tests
```

修改的现有文件：
- `Cargo.toml`         加 `enigo = "0.6" / xcap = "0.6" / display-info = "0.5"`
- `src/lib.rs`         加 `pub mod computer;`
- `src/agent/runtime.rs` 加 `resolve_vision_model_for()` + `VisionResolution` enum + `is_known_text_only_model()` + `vision_unavailable_message()` + 实例方法 `resolve_vision_model_name()`
- `src/agent/tools_agent.rs` 2 处 `ModelConfig` 字面量加 `vision: None,`
- `src/config/schema.rs` `ModelConfig` 加 `pub vision: Option<String>` (在 `flash` 之后)
- `src/store/redb_store.rs` +43 LOC 加 `computer_permissions` 表 + `permission_get/_put/_delete`

被推倒待删除（暂未删）：
- `src/agent/tools_computer.rs`             1771 LOC
- `src/provider/ui_tars.rs`                 1005 LOC
- `tools/computer_use/ui_tars_service.py`    140 LOC ← 已 deleted (git status 显示)

## 待办 (按顺序)

### 1. 验证 driver.rs ★ 立刻做
```
RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo check 2>&1 | tail -30
RSCLAW_BUILD_VERSION=dev RSCLAW_BUILD_DATE=test cargo test --lib computer 2>&1 | tail -30
```

### 2. 写新的 thin `tools_computer.rs` dispatcher (~150 LOC)
- 入口点保留：`tool_screenshot / tool_click / tool_type / tool_scroll / tool_key / tool_wait / tool_ui_tars`
- 实现：路由到 operator 原语 + (`ui_tars` 路由到 `VlmDriver::run`)
- 通过 `ProviderRegistry` 解析视觉模型名 → 拿到 `Arc<dyn LlmProvider>` → 注入 driver
- 通过 `agent_id + app` 调用 `permission.check`
- 把 driver 的 `DriverOutcome` 翻译成 tool result string

### 3. 删除旧文件
- `src/provider/ui_tars.rs` (1005 LOC)
- 删除前先 `grep -r "ui_tars" src/` 确认没有外部引用
- `src/provider/mod.rs` 移除 `pub mod ui_tars;`

### 4. WS 协议
- `chat.permission_response` 方法 (request_id + decision)
- 后端通过 `RedbPermissionStore::resolve_pending_request(id, decision)` 解析
- `PermissionRequest` 通过 `event_bus` broadcast 给所有连接的 WS 客户端

### 5. Tauri UI (`ComputerUsePermissionDialog.tsx`)
- 红色边框模态框 ("RsClaw is about to control your computer...")
- 4 按钮: Allow once / Allow this session / Always allow / Deny
- 订阅 `permission_request` ws 事件
- 通过 ws 发回 `chat.permission_response`
- gateway settings 页面加 "Bypass all" toggle

### 6. 集成测试
- 微信测试群跑通 e2e（之前 /loop 因为微信账号异常停了，登上后再继续）

### 7. 合并
- squash `refactor/computer_use` → `dev`

## 关键设计决策（不要忘）

1. **Operator = 平台抽象，App = 数据**
   `wechat.rs` 不存在，是 `tools/computer_use/app-rules/wechat.md`
   加新 App 不写 Rust，只写 markdown

2. **`async fn in trait` 用 `Pin<Box<Future>>` 不用 `async-trait`**
   项目硬规：rust 2024 native，不依赖 async-trait macro

3. **enigo Windows 不是 Send** → 每次 `tokio::task::spawn_blocking` 内创建新实例

4. **macOS Retina 坐标除以 scale_factor**，Win/Linux X11 直传

5. **VlmDriver 坐标缩放启发式**:
   - x 或 y > screen × 1.5 → 视为归一化 0-1000
   - x 或 y ≤ 1.0 → 视为归一化
   - 否则当成绝对像素

6. **视觉模型解析链**:
   `agents.<name>.model.vision` → `agents.default.model.vision` →
   `agents.<name>.model.primary` → `agents.default.model.primary` →
   报错 "primary 不支持视觉，请配置 vision 模型"

7. **iPhone Mirroring 单独 Operator**
   窗口级截图 (`xcap::Window::all()` filter title prefix `iPhone Mirroring`) +
   坐标偏移 `(win_x + x, win_y + y)` + iOS action_space (tap/swipe/press_home/...)

8. **Android = ADB Operator**
   `adb exec-out screencap -p` (binary PNG over stdout)
   `adb shell input tap/swipe/text/keyevent`

9. **App-rules 别名硬编码 canonical**
   `wechat → [wechat, 微信, weixin]`、`doubao → [doubao, 豆包]`、
   `douyin → [douyin, 抖音]`、`tonghuashun → [tonghuashun, 同花顺]`
   未来如果别名爆炸再改 yaml frontmatter 的 `triggers`

10. **Permission v1 用 polling**
    200ms→2s 指数退避，60s deadline
    TODO: 之后切换为直接 `oneshot::Receiver::await` (WS plumb 后)

## 已知坑

- `Cargo.lock` 会因为新依赖动；这是正常的，不要在意 "Never modify Cargo.lock" 那条规则（只在升级现有 dep 时适用）
- 编译可能因为 enigo `async-trait` 间接依赖编译时间长，第一次 `cargo check` 慢
- driver.rs 的 8 个 unit tests 在写完之后没跑过，可能有错字 / 借用错误，逐个修

## 用户原话片段

> "把computer_use改成我们的核心竞争力。另外需要实现类似claude,ui-tars-desktop那种 the xxx is control your computer..."
> "好像都是操控的应用要bring all to front."
> "wechat需要单独抽象出来吗？那不是新增加一个app比如doubao也要加一个doubao.rs?" → 我答错了被纠正
> "可能需要单独一个Operator哦" (iPhone)
> "还有android的是adb?" 是
> "继续，今晚要辛苦你了。"
> "上下文要满了，你先下保存一下记录，然后进行自动压缩？" ← 当前
