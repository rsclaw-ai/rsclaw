# RsClaw Roadmap

## v1.0 — 2026.4.20 (Current)

AI Agent Engine with long-term memory, self-learning, and multi-agent orchestration.

- 4 agent types (Main/Named/Sub/Task) with bidirectional team communication
- 4 execution backends (Native Rust/Claude Code/OpenCode/ACP)
- 13 messaging channels + custom webhook
- 15+ LLM providers with failover
- 50+ browser automation actions (agent-browser parity)
- Three-layer memory (redb KV + tantivy FTS + hnsw_rs vector)
- AnyCLI structured web data extraction
- A2A v0.3 cross-machine orchestration
- WASM plugin system (wasmtime v29)
- Skill auto-crystallization from usage patterns
- KV cache optimization (API key isolation, TTL, incremental messages)

## v2.0 — Planned

### 1. Full-Platform Perception & Control

**Screen Awareness**
- Real-time screen capture with vision model understanding
- OCR for on-screen text recognition
- UI element detection (buttons, inputs, menus, dialogs)
- Context awareness: understand what app is active, what task the user is doing

**Desktop Takeover**
- Extend computer_use beyond screenshot+click to full desktop control
- Application-level operations: launch apps, switch windows, drag files
- System-level: notifications, clipboard, global hotkeys, file system monitoring
- Native OS integration (macOS Accessibility API, Windows UI Automation)

**Mobile Device Interaction**
- iOS/Android screen mirroring + touch control
- Cross-device task delegation: phone captures → desktop processes → results pushed back
- Mobile notification monitoring and response

### 2. Cross-Application Intelligent Collaboration

**Application Data Flow**
- Seamless data transfer between apps without manual copy-paste
- Example: Extract data from Excel → compose email → send via WeChat — fully automated
- Clipboard-aware pipeline: detect what user copied, offer relevant actions

**Workflow Orchestration**
- Multi-app automation flows (RPA-level but AI-driven)
- Visual workflow builder in UI
- Conditional branching based on screen state / app response
- Error recovery: detect when an app shows an error, adapt the workflow

**Context-Aware Intelligence**
- Understand which application is in focus and what the user is working on
- Proactive suggestions based on screen content
- Cross-app memory: remember what was done in App A when working in App B

### 3. A2A Protocol Enhancement — Production-Ready

**Agent Discovery**
- Automatic capability-based agent discovery (not just manual config)
- Agent registry / directory service
- Capability advertisement: each agent publishes what it can do
- Matching: find the best remote agent for a given task

**Task Negotiation**
- Multi-agent discussion for task decomposition
- Bidding system: multiple agents can offer to handle a task
- SLA negotiation: deadline, quality, cost constraints
- Conflict resolution when agents disagree

**State Synchronization**
- Cross-machine session sharing (not just message passing)
- Distributed memory: agents share relevant memories
- Consistency model: eventual consistency with conflict resolution
- Secure state transfer (encrypted, authenticated)

**Production Hardening**
- Authentication and authorization between agents
- Rate limiting, quota management
- Monitoring, tracing, audit logs
- Graceful degradation when remote agents are unavailable

### 4. Agent Ecosystem

**AnyCLI Adapter Marketplace**
- anycli.org community hub
- Adapter rating, reviews, usage stats
- One-click install: `rsclaw anycli install <name>`
- Auto-generated adapters via AI (point at a website, get an adapter)

**Skill Marketplace**
- Public skill registry with categories
- Skill versioning and dependency management
- Quality gates: automated testing before publish
- Revenue sharing for premium skills

**WASM Plugin SDK**
- Developer documentation and tutorials
- Plugin template generator
- Local testing framework
- Hot-reload during development
- Plugin marketplace with review process

**Developer Tools**
- `rsclaw dev` CLI for plugin/skill development
- Playground: test agents, tools, skills interactively
- Debugging tools: trace tool calls, inspect memory, replay sessions
- Performance profiling for custom plugins

### 5. Infrastructure (Commercial)

**Distributed Inference (rsclaw-server)**
- GPU node scheduling for 10K+ nodes (internet cafe scenario)
- KV cache P2P migration on node drain
- Incremental message transmission (cache_id + delta)
- Failover with partial generation continuation

**Digital Human Pipeline**
- MuseTalk + CosyVoice2 integration
- Real-time voice conversation (STT → Agent → TTS → lip-sync)
- Video understanding + generation closed loop

---

## Contributing

We welcome contributions in all areas. See [CONTRIBUTING.md](CONTRIBUTING.md) for guidelines.

Priority areas for community contribution:
- AnyCLI adapters for new websites
- Skills for common workflows
- WASM plugins for service integrations
- Translations (currently 10 languages)
- Documentation and tutorials
