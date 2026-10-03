use std::fs;
use std::path::Path;
use anyhow::Result;

pub fn sync_antigravity_settings(workspace_root: Option<&Path>) -> Result<bool> {
    let home = match dirs::home_dir() {
        Some(h) => h,
        None => return Ok(false),
    };

    let settings_path = home.join(".gemini").join("antigravity-cli").join("settings.json");
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

    // 1. Ensure trustedWorkspaces includes current workspace, /tmp, and ~/.agents
    if let Some(trusted) = json.get_mut("trustedWorkspaces").and_then(|t| t.as_array_mut()) {
        let mut add_trusted = |p: &str| {
            let val = serde_json::Value::String(p.to_string());
            if !trusted.contains(&val) {
                trusted.push(val);
                modified = true;
            }
        };

        if let Some(ws) = workspace_root {
            add_trusted(&ws.to_string_lossy());
        }
        add_trusted(&home.join(".agents").to_string_lossy());
        add_trusted("/tmp");
        add_trusted("/private/tmp");
    }

    // 2. Ensure permissions.allow includes CQ MCP and tmp file write permissions without globs
    if let Some(perms) = json.get_mut("permissions").and_then(|p| p.get_mut("allow")).and_then(|a| a.as_array_mut()) {
        let mut add_perm = |p: &str| {
            let val = serde_json::Value::String(p.to_string());
            if !perms.contains(&val) {
                perms.push(val);
                modified = true;
            }
        };

        // Allow CQ MCP tools
        add_perm("mcp(cq)");
        add_perm("mcp(cq/query)");
        add_perm("mcp(cq/status)");
        add_perm("mcp(cq/propose)");
        add_perm("mcp(cq/confirm)");
        add_perm("mcp(cq/flag)");

        // Allow skills directory and tmp writes
        add_perm(&format!("read_file({})", home.join(".agents").join("skills").display()));
        add_perm("write_file(/tmp)");
        add_perm("write_file(/private/tmp)");
    }

    if modified {
        let updated = serde_json::to_string_pretty(&json)?;
        fs::write(&settings_path, updated)?;
    }

    Ok(modified)
}
