//! Self-evolution config: tunable thresholds for memory crystallization.
//!
//! Resolved at gateway startup from `config.ext.evolution` and stashed in a
//! process-wide `OnceLock`. Readers in `memory.rs`, `crystallizer.rs`, and
//! `meditation.rs` look up the live values without paying lock cost on the
//! hot path.

use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Resolved config (no Option<> — defaults applied)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct EvolutionConfig {
    /// Master kill-switch. When false, no crystallization runs.
    pub enabled: bool,
    pub cluster: ClusterConfig,
    pub promotion: PromotionConfig,
    pub meditation: MeditationParams,
}

#[derive(Debug, Clone)]
pub struct ClusterConfig {
    /// Minimum related Core docs needed to attempt distillation.
    pub min_size: usize,
    /// Cosine similarity threshold for "related".
    pub similarity_threshold: f32,
}

#[derive(Debug, Clone)]
pub struct PromotionConfig {
    /// Path 1: access_count alone wins promotion.
    pub access_only: i64,
    /// Path 2: importance alone wins promotion.
    pub importance_only: f32,
    /// Path 3a: access_count threshold when combined with importance.
    pub both_access: i64,
    /// Path 3b: importance threshold when combined with access_count.
    pub both_importance: f32,
}

#[derive(Debug, Clone)]
pub struct MeditationParams {
    /// Max clusters processed per meditation crystallize phase.
    pub max_per_cycle: usize,
    /// Cosine similarity threshold for dedup phase.
    pub dedup_threshold: f32,
    /// Days after crystallization before demoting source memories.
    pub crystallized_ttl_days: u32,
}

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cluster: ClusterConfig {
                min_size: 3,
                similarity_threshold: 0.75,
            },
            promotion: PromotionConfig {
                access_only: 15,
                importance_only: 0.9,
                both_access: 5,
                both_importance: 0.8,
            },
            meditation: MeditationParams {
                max_per_cycle: 5,
                dedup_threshold: 0.92,
                crystallized_ttl_days: 7,
            },
        }
    }
}

impl EvolutionConfig {
    /// Looser thresholds for testing — chat 2-3 turns on a topic and the
    /// pipeline fires. NOT for production.
    pub fn test_preset() -> Self {
        Self {
            enabled: true,
            cluster: ClusterConfig {
                min_size: 2,
                similarity_threshold: 0.5,
            },
            promotion: PromotionConfig {
                access_only: 3,
                importance_only: 0.6,
                both_access: 2,
                both_importance: 0.5,
            },
            meditation: MeditationParams {
                max_per_cycle: 1,
                dedup_threshold: 0.92,
                crystallized_ttl_days: 7,
            },
        }
    }

    /// Build from raw schema. Preset (if any) sets the base; individual
    /// fields override on top of that base.
    pub fn from_raw(raw: Option<&crate::config::schema::EvolutionConfig>) -> Self {
        let raw = match raw {
            Some(r) => r,
            None => return Self::default(),
        };

        let mut cfg = match raw.preset.as_deref() {
            Some("test") => Self::test_preset(),
            _ => Self::default(),
        };

        if let Some(v) = raw.enabled {
            cfg.enabled = v;
        }
        if let Some(c) = &raw.cluster {
            if let Some(v) = c.min_size {
                cfg.cluster.min_size = v;
            }
            if let Some(v) = c.similarity_threshold {
                cfg.cluster.similarity_threshold = v;
            }
        }
        if let Some(p) = &raw.promotion {
            if let Some(v) = p.access_only {
                cfg.promotion.access_only = v;
            }
            if let Some(v) = p.importance_only {
                cfg.promotion.importance_only = v;
            }
            if let Some(v) = p.both_access {
                cfg.promotion.both_access = v;
            }
            if let Some(v) = p.both_importance {
                cfg.promotion.both_importance = v;
            }
        }
        if let Some(m) = &raw.meditation {
            if let Some(v) = m.max_per_cycle {
                cfg.meditation.max_per_cycle = v;
            }
            if let Some(v) = m.dedup_threshold {
                cfg.meditation.dedup_threshold = v;
            }
            if let Some(v) = m.crystallized_ttl_days {
                cfg.meditation.crystallized_ttl_days = v;
            }
        }
        cfg
    }
}

// ---------------------------------------------------------------------------
// Process-wide singleton
// ---------------------------------------------------------------------------

static EVO: OnceLock<EvolutionConfig> = OnceLock::new();

/// Initialize the global evolution config. Called once during gateway
/// startup. Subsequent calls are ignored — this is intentional to keep
/// readers lock-free.
pub fn init_evolution_config(cfg: EvolutionConfig) {
    let _ = EVO.set(cfg);
}

/// Get the current evolution config. Returns the default if not yet
/// initialized (early-boot safety).
pub fn evolution_config() -> &'static EvolutionConfig {
    EVO.get_or_init(EvolutionConfig::default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_pre_config_constants() {
        let cfg = EvolutionConfig::default();
        assert!(cfg.enabled);
        assert_eq!(cfg.cluster.min_size, 3);
        assert!((cfg.cluster.similarity_threshold - 0.75).abs() < 0.001);
        assert_eq!(cfg.promotion.access_only, 15);
        assert!((cfg.promotion.importance_only - 0.9).abs() < 0.001);
        assert_eq!(cfg.promotion.both_access, 5);
        assert!((cfg.promotion.both_importance - 0.8).abs() < 0.001);
        assert_eq!(cfg.meditation.max_per_cycle, 5);
        assert!((cfg.meditation.dedup_threshold - 0.92).abs() < 0.001);
        assert_eq!(cfg.meditation.crystallized_ttl_days, 7);
    }

    #[test]
    fn test_preset_is_looser() {
        let d = EvolutionConfig::default();
        let t = EvolutionConfig::test_preset();
        assert!(t.cluster.min_size < d.cluster.min_size);
        assert!(t.cluster.similarity_threshold < d.cluster.similarity_threshold);
        assert!(t.promotion.access_only < d.promotion.access_only);
        assert!(t.promotion.importance_only < d.promotion.importance_only);
    }

    #[test]
    fn from_raw_none_returns_default() {
        let cfg = EvolutionConfig::from_raw(None);
        assert_eq!(cfg.cluster.min_size, 3);
    }

    #[test]
    fn from_raw_preset_then_override() {
        // Build a raw config with preset = "test" and an explicit override
        // on min_size to verify the layering order.
        let raw = crate::config::schema::EvolutionConfig {
            enabled: None,
            preset: Some("test".to_owned()),
            cluster: Some(crate::config::schema::EvolutionClusterConfig {
                min_size: Some(7),           // override even after preset
                similarity_threshold: None,  // keep preset's 0.5
            }),
            promotion: None,
            meditation: None,
        };
        let cfg = EvolutionConfig::from_raw(Some(&raw));
        assert_eq!(cfg.cluster.min_size, 7); // explicit override wins
        assert!((cfg.cluster.similarity_threshold - 0.5).abs() < 0.001); // from preset
        assert_eq!(cfg.promotion.access_only, 3); // from preset
    }
}
