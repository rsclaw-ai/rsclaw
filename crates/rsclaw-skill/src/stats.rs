//! Skill usage stats + disuse retirement.
//!
//! Closes the crystallization loop: skills get GENERATED automatically
//! (memory clusters, hard-workflow distillation) but until now nothing
//! ever measured whether they get used — a bad or obsolete auto-skill
//! sat in the prompt's skill list forever. `record_skill_use` counts
//! activations in the redb KV table; `retire_unused_auto_skills` (called
//! from the meditation cycle) moves auto-generated skills that have not
//! been activated within the window into `<skills>/.retired/<name>/`,
//! where the loader no longer sees them but a human can still restore
//! by moving the directory back.
//!
//! Only `auto-*` skills are eligible — hand-installed skills are the
//! user's call to keep regardless of usage.

use std::path::Path;

use anyhow::{Context, Result};
use rsclaw_store::redb_store::RedbStore;

/// Disuse window: an auto-skill not activated for this many days (and old
/// enough to have had the chance) is retired.
pub const RETIRE_AFTER_DAYS: i64 = 14;

fn key(name: &str) -> String {
    format!("skillstat:{name}")
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct SkillStat {
    uses: u64,
    last_used_ms: i64,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Record one activation (called from the `skill_use` tool). Best-effort
/// upsert into the KV table.
pub fn record_skill_use(db: &RedbStore, name: &str) -> Result<()> {
    let k = key(name);
    let mut stat: SkillStat = db
        .kv_get(&k)?
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    stat.uses += 1;
    stat.last_used_ms = now_ms();
    db.kv_set(&k, &serde_json::to_string(&stat)?)?;
    Ok(())
}

/// Retire auto-generated skills (`auto-*` dir names) that have never been
/// activated — or not within [`RETIRE_AFTER_DAYS`] — by moving their
/// directory into `<skills_dir>/.retired/`. A skill younger than the
/// window (by SKILL.md mtime) is left alone so fresh crystallizations get
/// a fair chance. Returns the names retired.
pub fn retire_unused_auto_skills(db: &RedbStore, skills_dir: &Path) -> Result<Vec<String>> {
    retire_unused_auto_skills_at(db, skills_dir, now_ms())
}

/// Clock-injected core of [`retire_unused_auto_skills`] (deterministic
/// tests pass a far-future `now`).
fn retire_unused_auto_skills_at(
    db: &RedbStore,
    skills_dir: &Path,
    now: i64,
) -> Result<Vec<String>> {
    let mut retired = Vec::new();
    let entries = match std::fs::read_dir(skills_dir) {
        Ok(e) => e,
        Err(_) => return Ok(retired), // no skills dir yet — nothing to do
    };
    let cutoff_ms = now - RETIRE_AFTER_DAYS * 24 * 3600 * 1000;

    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()).map(str::to_owned) else {
            continue;
        };
        if !path.is_dir() || !name.starts_with("auto-") {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        // Grace period: judge by last use when we have one, else by the
        // skill's own age — a week-old never-used skill is not yet stale.
        let last_used_ms = db
            .kv_get(&key(&name))?
            .and_then(|raw| serde_json::from_str::<SkillStat>(&raw).ok())
            .map(|s| s.last_used_ms)
            .unwrap_or(0);
        let created_ms = skill_md
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        if last_used_ms.max(created_ms) >= cutoff_ms {
            continue;
        }

        let retired_dir = skills_dir.join(".retired");
        std::fs::create_dir_all(&retired_dir)
            .with_context(|| format!("create {}", retired_dir.display()))?;
        let mut dest = retired_dir.join(&name);
        // Never clobber an earlier retirement of the same name.
        let mut suffix = 1;
        while dest.exists() {
            dest = retired_dir.join(format!("{name}-{suffix}"));
            suffix += 1;
        }
        std::fs::rename(&path, &dest)
            .with_context(|| format!("retire {} -> {}", path.display(), dest.display()))?;
        tracing::info!(skill = %name, dest = %dest.display(), "retired unused auto-skill");
        retired.push(name);
    }
    Ok(retired)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (RedbStore, tempfile::TempDir) {
        let tmp = tempfile::tempdir().unwrap();
        let db = RedbStore::open(
            &tmp.path().join("kv.redb"),
            rsclaw_platform::MemoryTier::High,
        )
        .unwrap();
        (db, tmp)
    }

    fn make_skill(dir: &Path, name: &str) {
        let d = dir.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), "---\nname: x\n---\nbody").unwrap();
    }

    #[test]
    fn record_and_accumulate() {
        let (db, _t) = store();
        record_skill_use(&db, "auto-x").unwrap();
        record_skill_use(&db, "auto-x").unwrap();
        let raw = db.kv_get("skillstat:auto-x").unwrap().unwrap();
        let s: SkillStat = serde_json::from_str(&raw).unwrap();
        assert_eq!(s.uses, 2);
        assert!(s.last_used_ms > 0);
    }

    #[test]
    fn retire_only_stale_auto_skills() {
        let (db, _t) = store();
        let skills = tempfile::tempdir().unwrap();
        make_skill(skills.path(), "auto-old-unused");
        make_skill(skills.path(), "auto-recently-used");
        make_skill(skills.path(), "hand-installed");

        // Clock injection: evaluate one window + a day past file creation so
        // every skill is stale by age. The "recently used" stat is written
        // with a last_used inside that window — only it survives.
        let future = now_ms() + (RETIRE_AFTER_DAYS + 1) * 24 * 3600 * 1000;
        db.kv_set(
            "skillstat:auto-recently-used",
            &serde_json::to_string(&SkillStat {
                uses: 3,
                last_used_ms: future - 3_600_000,
            })
            .unwrap(),
        )
        .unwrap();

        let retired = retire_unused_auto_skills_at(&db, skills.path(), future).unwrap();
        assert_eq!(retired, vec!["auto-old-unused".to_string()]);
        assert!(!skills.path().join("auto-old-unused").exists());
        assert!(
            skills
                .path()
                .join(".retired")
                .join("auto-old-unused")
                .join("SKILL.md")
                .is_file()
        );
        // Hand-installed and recently-used survive.
        assert!(skills.path().join("hand-installed").exists());
        assert!(skills.path().join("auto-recently-used").exists());
    }
}
