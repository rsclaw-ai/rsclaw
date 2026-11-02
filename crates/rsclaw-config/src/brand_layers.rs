//! Brand configuration layers.
//!
//! A brand directory may ship two JSON5 layers that are compiled into the
//! binary (see `rsclaw_brand::CONFIG_DEFAULTS` / `POLICY_LOCKED` and
//! docs/branding.md, "Brand directory"):
//!
//! - `config.defaults.json5` sits **under** the user's config file: it
//!   supplies values the user has not set.
//! - `policy.locked.json5` sits **over** it: its values always win.
//!
//! Effective config = `defaults ⊕ user ⊕ locked`, merged on the JSON value
//! before schema deserialization. Objects merge key by key, recursively;
//! arrays and scalars replace. The layers never reach the user's file:
//! raw read/write paths (`config_json`, the config editors) see only what
//! the user wrote.
//!
//! The default brand ships neither layer, so [`apply`] returns its input
//! untouched.

use std::sync::LazyLock;

use serde_json::Value;

static DEFAULTS: LazyLock<Option<Value>> = LazyLock::new(|| {
    rsclaw_brand::CONFIG_DEFAULTS.map(|src| parse_layer("config.defaults.json5", src))
});

static LOCKED: LazyLock<Option<Value>> = LazyLock::new(|| {
    rsclaw_brand::POLICY_LOCKED.map(|src| parse_layer("policy.locked.json5", src))
});

static LOCKED_PATHS: LazyLock<Vec<String>> =
    LazyLock::new(|| LOCKED.as_ref().map(leaf_paths).unwrap_or_default());

fn parse_layer(name: &str, src: &str) -> Value {
    // The brand build script already rejected anything that is not a JSON5
    // object, so this only fails on a broken build.
    json5::from_str(src).unwrap_or_else(|e| panic!("brand layer {name} is not valid JSON5: {e}"))
}

/// True when this build carries at least one brand config layer.
pub fn is_active() -> bool {
    DEFAULTS.is_some() || LOCKED.is_some()
}

/// True when the brand ships `config.defaults.json5`.
pub fn has_defaults() -> bool {
    DEFAULTS.is_some()
}

/// Dotted paths of every value pinned by `policy.locked.json5`
/// (`tools.exec.safety`, ...). Empty without a locked layer.
pub fn locked_paths() -> &'static [String] {
    &LOCKED_PATHS
}

/// Merge `over` into `base`: objects key by key (recursively), anything
/// else replaces.
pub fn deep_merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (key, value) in o {
                match b.get_mut(key) {
                    Some(slot) => deep_merge(slot, value),
                    None => {
                        b.insert(key.clone(), value.clone());
                    }
                }
            }
        }
        (slot, value) => *slot = value.clone(),
    }
}

/// `defaults ⊕ user ⊕ locked` for explicit layers (testable core of
/// [`apply`]).
pub fn merge_layers(user: Value, defaults: Option<&Value>, locked: Option<&Value>) -> Value {
    let mut out = match defaults {
        Some(d) => {
            let mut base = d.clone();
            deep_merge(&mut base, &user);
            base
        }
        None => user,
    };
    if let Some(l) = locked {
        deep_merge(&mut out, l);
    }
    out
}

/// Effective config value for a user config value, with this build's brand
/// layers applied.
pub fn apply(user: Value) -> Value {
    if !is_active() {
        return user;
    }
    merge_layers(user, DEFAULTS.as_ref(), LOCKED.as_ref())
}

/// Dotted paths of the leaves (non-object values) of `v`.
pub fn leaf_paths(v: &Value) -> Vec<String> {
    fn walk(v: &Value, prefix: &str, out: &mut Vec<String>) {
        match v {
            Value::Object(map) if !map.is_empty() => {
                for (k, child) in map {
                    let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                    walk(child, &path, out);
                }
            }
            _ if !prefix.is_empty() => out.push(prefix.to_owned()),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, "", &mut out);
    out
}

/// Locked paths where `user` sets a value that the locked layer overrides
/// (the user's setting is ignored).
pub fn conflicts_with(user: &Value, locked: &Value) -> Vec<String> {
    leaf_paths(locked)
        .into_iter()
        .filter(|path| {
            let ptr = format!("/{}", path.replace('~', "~0").replace('/', "~1").replace('.', "/"));
            match (user.pointer(&ptr), locked.pointer(&ptr)) {
                (Some(u), Some(l)) => u != l,
                _ => false,
            }
        })
        .collect()
}

/// [`conflicts_with`] against this build's locked layer.
pub fn locked_conflicts(user: &Value) -> Vec<String> {
    match LOCKED.as_ref() {
        Some(l) => conflicts_with(user, l),
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::runtime::IntoRuntime;

    #[test]
    fn layers_merge_in_order() {
        let defaults = json!({"gateway": {"language": "Chinese", "port": 1}, "tools": {"deny": ["a"]}});
        let locked = json!({"tools": {"exec": {"safety": true}}});
        let user = json!({"gateway": {"port": 2}, "tools": {"deny": ["b", "c"], "exec": {"safety": false, "timeoutSeconds": 9}}});
        let out = merge_layers(user, Some(&defaults), Some(&locked));
        assert_eq!(out["gateway"], json!({"language": "Chinese", "port": 2}));
        assert_eq!(out["tools"]["deny"], json!(["b", "c"]), "arrays replace");
        assert_eq!(out["tools"]["exec"], json!({"safety": true, "timeoutSeconds": 9}), "locked wins");
    }

    #[test]
    fn locked_paths_and_conflicts() {
        let locked = json!({"tools": {"exec": {"safety": true}}, "gateway": {"bind": "loopback"}});
        let mut paths = leaf_paths(&locked);
        paths.sort();
        assert_eq!(paths, ["gateway.bind", "tools.exec.safety"]);
        let user = json!({"tools": {"exec": {"safety": false}}, "gateway": {"bind": "loopback"}});
        assert_eq!(conflicts_with(&user, &locked), ["tools.exec.safety"]);
        assert!(conflicts_with(&json!({}), &locked).is_empty());
    }

    #[test]
    fn compiled_layers_yield_a_valid_config() {
        // Whatever this build's brand ships must deserialize and validate on
        // top of an empty user config.
        let value = apply(json!({}));
        let cfg: crate::schema::Config =
            serde_json::from_value(value).expect("brand layers deserialize");
        let runtime = cfg.into_runtime().expect("brand layers build a runtime config");
        crate::validator::validate(&runtime).expect("brand layers validate");
    }

    #[test]
    fn default_brand_has_no_layers() {
        if rsclaw_brand::ID == "rsclaw" {
            assert!(!is_active());
            assert!(locked_paths().is_empty());
            let v = json!({"gateway": {"port": 1}});
            assert_eq!(apply(v.clone()), v);
        }
    }
}
