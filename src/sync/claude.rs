use std::fs;
use std::path::Path;
use anyhow::Result;

pub fn sync_claude_settings(_workspace_root: Option<&Path>) -> Result<bool> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Ok(false),
    };

    let settings_path = home.join(".claude").join("settings.json");
    if !settings_path.exists() {
        return Ok(false);
    }

    let content = match fs::read_to_string(&settings_path) {
        Ok(c) => c,
        Err(_) => return Ok(false),
    };

    let mut json: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };

    let mut modified = false;

    // Ensure permissions.allow object exists
    if json.get("permissions").is_none() {
        json["permissions"] = serde_json::json!({ "allow": [] });
        modified = true;
    }

    if let Some(perms) = json.get_mut("permissions").and_then(|p| p.get_mut("allow")).and_then(|a| a.as_array_mut()) {
        let mut add_perm = |p: &str| {
            let val = serde_json::Value::String(p.to_string());
            if !perms.contains(&val) {
                perms.push(val);
                modified = true;
            }
        };

        // Allow CQ MCP tools
        add_perm("mcp__plugin_cq_cq__query");
        add_perm("mcp__plugin_cq_cq__status");
        add_perm("mcp__plugin_cq_cq__propose");
        add_perm("mcp__plugin_cq_cq__confirm");
        add_perm("mcp__plugin_cq_cq__flag");
        add_perm("mcp__plugin_cq_cq__*");
    }

    if modified {
        let updated = serde_json::to_string_pretty(&json)?;
        fs::write(&settings_path, updated)?;
    }

    Ok(modified)
}
