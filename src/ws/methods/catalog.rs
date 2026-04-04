use crate::ws::dispatch::{MethodCtx, MethodResult};

pub async fn tools_catalog(ctx: MethodCtx) -> MethodResult {
    let global_dir = crate::skill::default_global_skills_dir().unwrap_or_default();
    let registry =
        crate::skill::load_skills(&global_dir, None, ctx.state.config.ext.skills.as_ref())
            .unwrap_or_default();
    let tools: Vec<serde_json::Value> = registry.all().flat_map(|s| {
        s.tools.iter().map(|t| serde_json::json!({"name": t.name, "description": t.description, "skill": s.name}))
    }).collect();
    Ok(serde_json::json!({"tools": tools}))
}
pub async fn tools_effective(_ctx: MethodCtx) -> MethodResult {
    Ok(serde_json::json!({"tools": []}))
}
