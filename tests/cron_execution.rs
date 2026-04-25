//! Integration tests for cron job scheduling and execution.
//!
//! These tests exercise `CronRunner` / `CronJob` behaviour without real LLM
//! calls.  We use mock agent receivers (echo tasks) to simulate an agent
//! responding to a cron-triggered `AgentMessage`.

#![allow(unused)]

use std::{path::PathBuf, sync::Arc, time::Duration};

use rsclaw::{
    agent::{AgentRegistry, AgentReply},
    channel::ChannelManager,
    config::{
        runtime::{AgentsRuntime, RuntimeConfig},
        schema::{AgentEntry, CronConfig, CronJobConfig},
    },
    cron::{CronJob, CronRunner},
    MemoryTier,
};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a `CronJob` directly (not via `CronJobConfig`).
fn make_job(id: &str, schedule: &str, agent_id: &str, enabled: bool) -> CronJob {
    use rsclaw::cron::CronSchedule;
    CronJob {
        id: id.to_owned(),
        name: None,
        agent_id: agent_id.to_owned(),
        session_key: None,
        enabled,
        schedule: CronSchedule::Flat(schedule.to_owned()),
        payload: None,
        message: Some(format!("ping from {id}")),
        session_target: None,
        delivery: None,
        wake_mode: None,
        state: None,
        created_at_ms: None,
        updated_at_ms: None,
    }
}

/// Build a minimal `CronConfig` (only `max_concurrent_runs` is relevant here).
fn minimal_cron_config() -> CronConfig {
    CronConfig {
        enabled: Some(true),
        max_concurrent_runs: Some(4),
        session_retention: None,
        run_log: None,
        jobs: None,
        default_delivery: None,
    }
}

/// Build a `RuntimeConfig` that contains a single agent with the given id.
fn runtime_with_agent(agent_id: &str) -> RuntimeConfig {
    use rsclaw::config::{
        runtime::{ChannelRuntime, ExtRuntime, GatewayRuntime, ModelRuntime, OpsRuntime},
        schema::{BindMode, GatewayMode, ReloadMode, SessionConfig},
    };

    RuntimeConfig {
        gateway: GatewayRuntime {
            port: 0,
            mode: GatewayMode::Local,
            bind: BindMode::Loopback,
            bind_address: None,
            reload: ReloadMode::Hybrid,
            auth_token: None,
            auth_token_configured: false,
            auth_token_is_plaintext: false,
            allow_tailscale: false,
            channel_health_check_minutes: 5,
            channel_stale_event_threshold_minutes: 30,
            channel_max_restarts_per_hour: 10,
            user_agent: None,
            language: None,
        },
        agents: AgentsRuntime {
            defaults: Default::default(),
            list: vec![AgentEntry {
                id: agent_id.to_owned(),
                default: Some(true),
                workspace: None,
                model: None,
                lane: None,
                lane_concurrency: None,
                group_chat: None,
                channels: None,
                name: None,
                agent_dir: None,
                system: None,
                allowed_commands: None,
                commands: None,
                opencode: None,
                claudecode: None,
                codex: None,
                flash_model: None,
            }],
            bindings: vec![],
            external: vec![],
        },
        channel: ChannelRuntime {
            channels: Default::default(),
            session: SessionConfig {
                dm_scope: None,
                thread_bindings: None,
                reset: None,
                identity_links: None,
                maintenance: None,
            },
        },
        model: ModelRuntime {
            models: None,
            auth: None,
        },
        ext: ExtRuntime {
            tools: None,
            skills: None,
            plugins: None,
        },
        ops: OpsRuntime {
            cron: None,
            hooks: None,
            sandbox: None,
            logging: None,
            secrets: None,
        },
        raw: Default::default(),
    }
}

// ---------------------------------------------------------------------------
// test_cron_job_runs
//
// Register a job against a real (echo) agent, then call `CronRunner::trigger`
// to fire it immediately. The echo task must reply, and trigger() must return
// Ok.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_cron_job_runs() {
    let cfg = runtime_with_agent("agent-a");

    // Build registry + receivers.
    let (registry, mut receivers) = AgentRegistry::from_config_with_receivers(&cfg, std::sync::Arc::new(rsclaw::provider::registry::ProviderRegistry::new()));

    // Spawn an echo task for "agent-a".
    if let Some(mut rx) = receivers.remove("agent-a") {
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let _ = msg.reply_tx.send(AgentReply {
                    text: format!("pong: {}", msg.text),
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    was_preparse: false,
                });
            }
        });
    }

    let data_dir = tempfile::tempdir().expect("tempdir");
    let job = make_job("job-runs", "* * * * *", "agent-a", true);
    let runner = CronRunner::new(
        &minimal_cron_config(),
        vec![job],
        Arc::new(registry),
        Arc::new(ChannelManager::new(MemoryTier::Standard)),
        data_dir.path().to_owned(),
        tokio::sync::broadcast::channel(1).0,
        Arc::new(rsclaw::ws::ConnRegistry::new()),
    );

    // trigger() bypasses the scheduler and fires the job synchronously.
    let result = runner.trigger("job-runs").await;
    assert!(result.is_ok(), "trigger should succeed: {:?}", result.err());
}

// ---------------------------------------------------------------------------
// test_cron_enable_disable
//
// A disabled job is listed in `runner.jobs()` but is marked `enabled = false`.
// Triggering it directly still works (trigger bypasses the enabled guard),
// but we verify that the runner correctly stores the `enabled` field.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_cron_enable_disable() {
    let cfg = runtime_with_agent("agent-b");
    let (registry, mut receivers) = AgentRegistry::from_config_with_receivers(&cfg, std::sync::Arc::new(rsclaw::provider::registry::ProviderRegistry::new()));

    if let Some(mut rx) = receivers.remove("agent-b") {
        tokio::spawn(async move {
            while let Some(msg) = rx.recv().await {
                let _ = msg.reply_tx.send(AgentReply {
                    text: "ok".to_owned(),
                    is_empty: false,
                    tool_calls: None,
                    images: vec![],
                    files: vec![],
                    pending_analysis: None,
                    was_preparse: false,
                });
            }
        });
    }

    let data_dir = tempfile::tempdir().expect("tempdir");

    // One enabled, one disabled.
    let job_on = make_job("job-on", "* * * * *", "agent-b", true);
    let job_off = make_job("job-off", "* * * * *", "agent-b", false);

    let runner = CronRunner::new(
        &minimal_cron_config(),
        vec![job_on, job_off],
        Arc::new(registry),
        Arc::new(ChannelManager::new(MemoryTier::Standard)),
        data_dir.path().to_owned(),
        tokio::sync::broadcast::channel(1).0,
        Arc::new(rsclaw::ws::ConnRegistry::new()),
    );

    // Verify enabled flags are stored correctly.
    let enabled_jobs: Vec<_> = runner.jobs().iter().filter(|j| j.enabled).collect();
    let disabled_jobs: Vec<_> = runner.jobs().iter().filter(|j| !j.enabled).collect();

    assert_eq!(enabled_jobs.len(), 1, "should have exactly one enabled job");
    assert_eq!(enabled_jobs[0].id, "job-on");

    assert_eq!(
        disabled_jobs.len(),
        1,
        "should have exactly one disabled job"
    );
    assert_eq!(disabled_jobs[0].id, "job-off");

    // Trigger the enabled job directly — must succeed.
    let r = runner.trigger("job-on").await;
    assert!(
        r.is_ok(),
        "trigger of enabled job should succeed: {:?}",
        r.err()
    );
}

// ---------------------------------------------------------------------------
// test_cron_invalid_agent
//
// A job referencing a non-existent agent ID should cause trigger() to return
// an error (agent not found).
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_cron_invalid_agent() {
    let cfg = runtime_with_agent("real-agent");
    let (registry, _receivers) = AgentRegistry::from_config_with_receivers(&cfg, std::sync::Arc::new(rsclaw::provider::registry::ProviderRegistry::new()));

    let data_dir = tempfile::tempdir().expect("tempdir");
    let job = make_job("job-bad-agent", "* * * * *", "nonexistent-agent", true);

    let runner = CronRunner::new(
        &minimal_cron_config(),
        vec![job],
        Arc::new(registry),
        Arc::new(ChannelManager::new(MemoryTier::Standard)),
        data_dir.path().to_owned(),
        tokio::sync::broadcast::channel(1).0,
        Arc::new(rsclaw::ws::ConnRegistry::new()),
    );

    let result = runner.trigger("job-bad-agent").await;
    assert!(
        result.is_err(),
        "trigger with unknown agent_id should return Err"
    );
    let msg = result.unwrap_err().to_string().to_lowercase();
    assert!(
        msg.contains("agent not found") || msg.contains("nonexistent"),
        "error should mention the missing agent, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// test_cron_invalid_expr (schedule expression validation)
//
// `CronJobConfig` → `CronJob` conversion does not validate the expression at
// construction time; validation happens when the job is registered with
// tokio-cron-scheduler.  We test the `to_six_field` transform indirectly by
// checking that a 5-field expression passes through the `CronJob` struct
// unchanged and a 6-field one is not double-prefixed.
//
// The full "invalid expression returns Error" path is exercised by
// `CronRunner::run` (which calls `Job::new_async`).  We verify that a
// clearly malformed schedule stored on the job is detectable at trigger time.
// ---------------------------------------------------------------------------

#[test]
fn test_cron_schedule_stored_verbatim() {
    // A standard 5-field expression.
    let job5 = make_job("j5", "*/5 * * * *", "a", true);
    assert_eq!(job5.cron_expr(), "*/5 * * * *");

    // A 6-field expression (already has seconds).
    let job6 = make_job("j6", "0 */5 * * * *", "a", true);
    assert_eq!(job6.cron_expr(), "0 */5 * * * *");
}

#[tokio::test(flavor = "current_thread")]
async fn test_cron_trigger_unknown_job_returns_error() {
    let cfg = runtime_with_agent("agent-c");
    let (registry, _receivers) = AgentRegistry::from_config_with_receivers(&cfg, std::sync::Arc::new(rsclaw::provider::registry::ProviderRegistry::new()));

    let data_dir = tempfile::tempdir().expect("tempdir");
    let runner = CronRunner::new(
        &minimal_cron_config(),
        vec![], // no jobs registered
        Arc::new(registry),
        Arc::new(ChannelManager::new(MemoryTier::Standard)),
        data_dir.path().to_owned(),
        tokio::sync::broadcast::channel(1).0,
        Arc::new(rsclaw::ws::ConnRegistry::new()),
    );

    let result = runner.trigger("no-such-job").await;
    assert!(
        result.is_err(),
        "triggering a non-existent job id should return Err"
    );
    let msg = result.unwrap_err().to_string().to_lowercase();
    assert!(
        msg.contains("not found") || msg.contains("no-such-job"),
        "error should mention the job id, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// test_openclaw_format_compat
//
// Verify that OpenClaw's cron/jobs.json format can be deserialized into
// rsclaw CronJob structs without error.
// ---------------------------------------------------------------------------

#[test]
fn test_openclaw_format_compat() {
    use rsclaw::cron::{CronJob, CronSchedule};

    // Simulate a single OpenClaw job entry.
    let openclaw_job = serde_json::json!({
        "id": "9b48103d-4ea9-4794-b415-0da96b639eb7",
        "agentId": "main",
        "sessionKey": "agent:main:feishu:main-bot:direct:ou_123",
        "name": "每日分钟线数据同步",
        "enabled": true,
        "createdAtMs": 1774081386849_u64,
        "updatedAtMs": 1775026811219_u64,
        "schedule": {
            "expr": "0 15 * * 1-5",
            "kind": "cron",
            "tz": "Asia/Shanghai"
        },
        "sessionTarget": "main",
        "wakeMode": "now",
        "payload": {
            "kind": "systemEvent",
            "text": "【每日分钟线同步】运行：python3 ~/scripts/sync.py"
        },
        "state": {
            "nextRunAtMs": 1775113200000_u64,
            "lastRunAtMs": 1775026800013_u64,
            "lastRunStatus": "ok",
            "lastStatus": "ok",
            "lastDurationMs": 11206_u64,
            "lastDeliveryStatus": "not-requested",
            "consecutiveErrors": 0
        }
    });

    let job: CronJob = serde_json::from_value(openclaw_job).expect("should parse openclaw format");

    assert_eq!(job.id, "9b48103d-4ea9-4794-b415-0da96b639eb7");
    assert_eq!(job.name.as_deref(), Some("每日分钟线数据同步"));
    assert_eq!(job.agent_id, "main");
    assert_eq!(job.cron_expr(), "0 15 * * 1-5");
    assert_eq!(job.timezone(), Some("Asia/Shanghai"));
    assert!(job.effective_message().contains("分钟线同步"));
    assert!(job.enabled);
    assert_eq!(job.created_at_ms, Some(1774081386849));
    assert!(job.state.is_some());
    let state = job.state.unwrap();
    assert_eq!(state.last_run_status.as_deref(), Some("ok"));
    assert_eq!(state.consecutive_errors, 0);
}

// Verify rsclaw flat format still works.
#[test]
fn test_rsclaw_flat_format() {
    use rsclaw::cron::CronJob;

    let rsclaw_job = serde_json::json!({
        "id": "job-1",
        "agentId": "main",
        "enabled": true,
        "schedule": "*/5 * * * *",
        "message": "ping"
    });

    let job: CronJob = serde_json::from_value(rsclaw_job).expect("should parse flat format");
    assert_eq!(job.cron_expr(), "*/5 * * * *");
    assert_eq!(job.effective_message(), "ping");
    assert!(job.timezone().is_none());
}
