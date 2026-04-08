use anyhow::Result;

use super::config_json::{load_config_json, set_nested_value};
use super::style::*;
use crate::{cli::CronCommand, config};

pub async fn cmd_cron(sub: CronCommand) -> Result<()> {
    match sub {
        CronCommand::List | CronCommand::Status => {
            banner(&format!("rsclaw cron v{}", env!("RSCLAW_BUILD_VERSION")));
            let config = config::load()?;
            let jobs = config
                .ops
                .cron
                .as_ref()
                .and_then(|c| c.jobs.as_deref())
                .unwrap_or(&[]);
            if jobs.is_empty() {
                warn_msg("no cron jobs configured");
            } else {
                println!(
                    "  {:<14} {:<10} {:<14} {}",
                    bold("ID"),
                    bold("STATUS"),
                    bold("AGENT"),
                    bold("SCHEDULE")
                );
                for j in jobs {
                    let enabled = j.enabled.unwrap_or(true);
                    let agent = j.agent_id.as_deref().unwrap_or("(default)");
                    let status = if enabled {
                        green("enabled")
                    } else {
                        red("disabled")
                    };
                    println!(
                        "  {:<14} {:<10} {:<14} {}",
                        cyan(&j.id),
                        status,
                        agent,
                        dim(&j.schedule)
                    );
                }
            }
        }
        CronCommand::Run { id } => {
            // Manual trigger: POST to gateway API if running.
            let config = config::load()?;
            let port = config.gateway.port;
            let jobs = config
                .ops
                .cron
                .as_ref()
                .and_then(|c| c.jobs.as_deref())
                .unwrap_or(&[]);
            let job = jobs
                .iter()
                .find(|j| j.id == id)
                .ok_or_else(|| anyhow::anyhow!("cron job '{id}' not found in config"))?;
            let url = format!("http://127.0.0.1:{port}/api/v1/message");
            let body = serde_json::json!({
                "text": job.message,
                "agent_id": job.agent_id,
                "session_key": format!("cron:{id}:manual"),
            });
            let client = reqwest::Client::new();
            let resp = client
                .post(&url)
                .json(&body)
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("gateway unreachable at {url}: {e}"))?;
            if resp.status().is_success() {
                ok(&format!("cron job '{}' triggered", cyan(&id)));
            } else {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("gateway error {status}: {text}");
            }
        }
        CronCommand::Runs { id } => {
            banner(&format!("rsclaw cron runs: {id}"));
            let log_file = config::loader::base_dir()
                .join("var/data/cron")
                .join(format!("{id}.jsonl"));
            if !log_file.exists() {
                warn_msg(&format!("no run history for '{}'", cyan(&id)));
            } else {
                let content = std::fs::read_to_string(&log_file)?;
                for line in content.lines().rev().take(20) {
                    if let Ok(entry) = serde_json::from_str::<serde_json::Value>(line) {
                        let success = entry["success"].as_bool().unwrap_or(false);
                        let status = if success { green("ok") } else { red("fail") };
                        let ts = entry["startedAt"].as_str().unwrap_or("?");
                        let error = entry["error"].as_str().unwrap_or("");
                        println!(
                            "  [{}] {} {}",
                            dim(ts),
                            status,
                            if error.is_empty() {
                                String::new()
                            } else {
                                red(error)
                            }
                        );
                    }
                }
            }
        }
        CronCommand::Add(args) => {
            validate_cron_schedule(&args.schedule)?;
            let (path, mut val) = load_config_json()?;

            // Auto-enable cron system when adding first job
            // (startup.rs requires cron.enabled=true to start the scheduler)
            if val
                .pointer("/cron/enabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                // Already enabled, no action needed
            } else {
                set_nested_value(&mut val, "cron.enabled", serde_json::json!(true))?;
            }

            let count = val
                .pointer("/cron/jobs")
                .and_then(|v| v.as_array())
                .map_or(0, |a| a.len());
            let id = format!("job-{}", count + 1);
            let mut job = serde_json::json!({
                "id": id,
                "schedule": args.schedule,
                "message": args.message,
            });
            if let Some(agent) = args.agent {
                job["agentId"] = agent.into();
            }
            if let Some(arr) = val.pointer_mut("/cron/jobs").and_then(|v| v.as_array_mut()) {
                arr.push(job);
            } else {
                set_nested_value(&mut val, "cron.jobs", serde_json::json!([job]))?;
            }
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!(
                "added cron job '{}' ({})",
                cyan(&id),
                dim(&args.schedule)
            ));
        }
        CronCommand::Edit { id } => {
            let config = config::load()?;
            config
                .ops
                .cron
                .as_ref()
                .and_then(|c| c.jobs.as_deref())
                .and_then(|j| j.iter().find(|j| j.id == id))
                .ok_or_else(|| anyhow::anyhow!("cron job '{id}' not found"))?;
            let (path, _) = load_config_json()?;
            let editor = std::env::var("EDITOR").unwrap_or_else(|_| {
                if cfg!(windows) { "notepad".to_owned() } else { "vi".to_owned() }
            });
            let status = std::process::Command::new(&editor).arg(&path).status()?;
            if !status.success() {
                anyhow::bail!("editor exited with {status}");
            }
        }
        CronCommand::Enable { id } => cron_set_enabled(id, true)?,
        CronCommand::Disable { id } => cron_set_enabled(id, false)?,
        CronCommand::Rm { id } => {
            let (path, mut val) = load_config_json()?;
            if let Some(jobs) = val.pointer_mut("/cron/jobs").and_then(|j| j.as_array_mut()) {
                let before = jobs.len();
                jobs.retain(|j| j["id"].as_str() != Some(&id));
                if jobs.len() == before {
                    anyhow::bail!("cron job '{id}' not found");
                }
            }
            std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
            ok(&format!("removed cron job '{}'", cyan(&id)));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Cron expression validator
// ---------------------------------------------------------------------------

/// Validate a 5-field cron expression (minute hour day month weekday).
fn validate_cron_schedule(schedule: &str) -> Result<()> {
    let fields: Vec<&str> = schedule.split_whitespace().collect();
    if fields.len() != 5 {
        anyhow::bail!(
            "cron schedule must have exactly 5 fields (got {}): \"{}\"",
            fields.len(),
            schedule
        );
    }
    // (min, max) for each field
    let ranges = [(0u32, 59u32), (0, 23), (1, 31), (1, 12), (0, 7)];
    let names = ["minute", "hour", "day", "month", "weekday"];
    for (i, (field, &(lo, hi))) in fields.iter().zip(ranges.iter()).enumerate() {
        validate_cron_field(field, lo, hi)
            .map_err(|e| anyhow::anyhow!("{} field: {e}", names[i]))?;
    }
    Ok(())
}

fn validate_cron_field(field: &str, min: u32, max: u32) -> Result<()> {
    if field == "*" {
        return Ok(());
    }
    for part in field.split(',') {
        validate_cron_part(part, min, max)?;
    }
    Ok(())
}

fn validate_cron_part(part: &str, min: u32, max: u32) -> Result<()> {
    // */n
    if let Some(step) = part.strip_prefix("*/") {
        let n: u32 = step
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid step '{step}'"))?;
        if n == 0 {
            anyhow::bail!("step cannot be 0");
        }
        return Ok(());
    }
    // n-m or n-m/step
    if part.contains('-') {
        let (range_part, step_opt) = match part.split_once('/') {
            Some((r, s)) => {
                let n: u32 = s
                    .parse()
                    .map_err(|_| anyhow::anyhow!("invalid step '{s}'"))?;
                if n == 0 {
                    anyhow::bail!("step cannot be 0");
                }
                (r, Some(n))
            }
            None => (part, None),
        };
        let (a, b) = range_part
            .split_once('-')
            .ok_or_else(|| anyhow::anyhow!("invalid range '{range_part}'"))?;
        let lo: u32 = a
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid value '{a}'"))?;
        let hi: u32 = b
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid value '{b}'"))?;
        if lo < min || hi > max || lo > hi {
            anyhow::bail!("range {lo}-{hi} out of bounds ({min}-{max})");
        }
        let _ = step_opt;
        return Ok(());
    }
    // single value
    let n: u32 = part
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid value '{part}'"))?;
    if n < min || n > max {
        anyhow::bail!("value {n} out of range ({min}-{max})");
    }
    Ok(())
}

// ---------------------------------------------------------------------------

pub fn cron_set_enabled(id: String, enabled: bool) -> Result<()> {
    let (path, mut val) = load_config_json()?;
    let jobs = val
        .pointer_mut("/cron/jobs")
        .ok_or_else(|| anyhow::anyhow!("no cron.jobs in config"))?;
    if let Some(arr) = jobs.as_array_mut() {
        let mut found = false;
        for job in arr.iter_mut() {
            if job["id"].as_str() == Some(&id) {
                job["enabled"] = serde_json::Value::Bool(enabled);
                found = true;
            }
        }
        if !found {
            anyhow::bail!("cron job '{id}' not found");
        }
    }
    std::fs::write(&path, serde_json::to_string_pretty(&val)?)?;
    if enabled {
        ok(&format!("cron job '{}' enabled", cyan(&id)));
    } else {
        ok(&format!("cron job '{}' disabled", cyan(&id)));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::validate_cron_schedule;

    #[test]
    fn valid_schedules() {
        assert!(validate_cron_schedule("* * * * *").is_ok());
        assert!(validate_cron_schedule("0 9 * * 1").is_ok());
        assert!(validate_cron_schedule("*/15 * * * *").is_ok());
        assert!(validate_cron_schedule("0 0 1 1 *").is_ok());
        assert!(validate_cron_schedule("0,30 8-18 * * 1-5").is_ok());
        assert!(validate_cron_schedule("0 12 * * 7").is_ok()); // weekday 7 = Sunday
    }

    #[test]
    fn wrong_field_count() {
        assert!(validate_cron_schedule("* * * *").is_err());
        assert!(validate_cron_schedule("* * * * * *").is_err());
        assert!(validate_cron_schedule("").is_err());
    }

    #[test]
    fn out_of_range_values() {
        assert!(validate_cron_schedule("60 * * * *").is_err()); // minute > 59
        assert!(validate_cron_schedule("* 24 * * *").is_err()); // hour > 23
        assert!(validate_cron_schedule("* * 0 * *").is_err()); // day < 1
        assert!(validate_cron_schedule("* * * 13 *").is_err()); // month > 12
        assert!(validate_cron_schedule("* * * * 8").is_err()); // weekday > 7
    }

    #[test]
    fn invalid_step_zero() {
        assert!(validate_cron_schedule("*/0 * * * *").is_err());
    }
}
