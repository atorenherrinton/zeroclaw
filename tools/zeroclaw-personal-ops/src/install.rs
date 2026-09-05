//! Operator-only additive installer. Uses ZeroClaw's atomic validated patch API.
use crate::{private_dir, private_write, schema};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{fs, os::unix::fs::PermissionsExt, path::Path, process::Command};

const COMMON: &str = include_str!("../templates/common.md");
const ROUTING: &str = include_str!("../templates/routing.md");
const ROLES: [(&str, &str); 4] = [
    (
        "communications",
        include_str!("../templates/communications.md"),
    ),
    ("calendar_tasks", include_str!("../templates/calendar.md")),
    ("task_scheduler", include_str!("../templates/scheduler.md")),
    ("coding", include_str!("../templates/coding.md")),
];

fn push_unique(array: &mut Value, value: Value) -> Result<()> {
    let a = array.as_array_mut().context("expected array")?;
    if !a.contains(&value) {
        a.push(value);
    }
    Ok(())
}

pub fn patch(config: &Value, root: &Path, github: &Path) -> Result<Value> {
    let mut ops = Vec::new();
    let mut add =
        |path: String, value: Value| ops.push(json!({"op":"add","path":path,"value":value}));
    let helper_tools: Vec<String> = schema()
        .as_array()
        .context("tools")?
        .iter()
        .filter_map(|v| v["name"].as_str().map(|s| format!("personal_ops__{s}")))
        .collect();
    let main = &config["agents"]["main"];
    ensure!(main.is_object(), "main agent required");
    let default = &config["risk_profiles"]["default"];
    let mut auto = default["auto_approve"].clone();
    push_unique(&mut auto, json!("delegate"))?;
    for name in &helper_tools {
        push_unique(&mut auto, json!(name))?;
    }
    add("/risk_profiles/default/auto_approve".into(), auto);
    add(
        "/risk_profiles/default/delegation_policy".into(),
        json!({"mode":"allow"}),
    );
    add(
        "/runtime_profiles/default/max_delegation_depth".into(),
        json!(1),
    );
    let mut bundles = main["mcp_bundles"].clone();
    push_unique(&mut bundles, json!("personal_ops"))?;
    add("/agents/main/mcp_bundles".into(), bundles);
    let mut delegates = main["delegates"].clone();
    for (alias, _) in ROLES {
        if !delegates
            .as_array()
            .context("delegates array")?
            .iter()
            .any(|v| v.as_str() == Some(alias) || v["agent"] == alias)
        {
            push_unique(&mut delegates, json!({"agent":alias,"mode":"bounded"}))?;
        }
    }
    add("/agents/main/delegates".into(), delegates);
    let servers = config["mcp"]["servers"]
        .as_array()
        .context("MCP servers required")?;
    ensure!(
        !servers.iter().any(|v| v["name"] == "personal_ops"),
        "personal_ops already installed; preserve existing state and use an explicit upgrade"
    );
    add(
        "/mcp/servers/-".into(),
        json!({"name":"personal_ops","command":root.join("bin/zeroclaw-personal-ops"),"args":["mcp",root],"transport":"stdio","tool_timeout_secs":300,"pinned_resources":[]}),
    );
    add(
        "/mcp_bundles/personal_ops".into(),
        json!({"servers":["personal_ops"],"exclude":[]}),
    );
    let mut astra = config["providers"]["models"]["openai"]["sol"].clone();
    ensure!(
        astra.is_object(),
        "existing native OpenAI provider required"
    );
    astra["model"] = json!("gpt-6-astra");
    astra["fallback"] = json!([]);
    astra["timeout_secs"] = json!(300);
    astra["fallback_models"] = json!([]);
    astra
        .as_object_mut()
        .context("provider")?
        .remove("temperature");
    add("/providers/models/openai/astra".into(), astra);
    for (alias, _) in ROLES {
        ensure!(
            config["agents"].get(alias).is_none(),
            "specialist alias already exists; refusing to overwrite"
        );
        let (mut tools, bundles, provider) = match alias {
            "communications" => (
                vec![
                    "google_read__gmail_search",
                    "google_read__gmail_get_message",
                    "google_read__gmail_get_thread",
                    "google_write__gmail_create_draft",
                    "personal_ops__text_prepare",
                    "personal_ops__files_prepare",
                    "personal_ops__voicemail_list",
                    "personal_ops__voicemail_prepare",
                    "personal_ops__delivery_status",
                ],
                vec!["google_read", "google_write", "personal_ops"],
                "openai.terra",
            ),
            "calendar_tasks" => (
                vec![
                    "google_read__calendar_events",
                    "google_read__gmail_search",
                    "google_read__gmail_get_message",
                    "google_read__gmail_get_thread",
                    "google_write__calendar_create_event",
                    "reminders__list",
                    "reminders__search",
                    "reminders__add",
                    "reminders__edit",
                    "reminders__complete",
                    "reminders__delete",
                ],
                vec!["google_read", "google_write", "reminders"],
                "openai.terra",
            ),
            "task_scheduler" => (
                vec![
                    "cron_list",
                    "cron_add",
                    "cron_update",
                    "cron_remove",
                    "cron_runs",
                ],
                vec![],
                "openai.terra",
            ),
            _ => (
                vec![
                    "file_read",
                    "file_write",
                    "file_edit",
                    "glob_search",
                    "content_search",
                    "git_operations",
                    "github_cli__run",
                    "shell",
                ],
                vec!["github_cli"],
                "openai.astra",
            ),
        };
        tools.sort();
        let mut risk = default.clone();
        risk["allowed_tools"] = json!(tools);
        risk["auto_approve"] = json!(tools);
        risk["always_ask"] = json!([]);
        risk["delegation_policy"] = json!({"mode":"forbidden"});
        risk["allowed_roots"] = if alias == "coding" {
            json!([github])
        } else {
            json!([])
        };
        risk["allowed_commands"] = if alias == "coding" {
            json!(["cargo", "rustc", "git", "npm", "node"])
        } else {
            json!([])
        };
        add(format!("/risk_profiles/{alias}"), risk);
        let mut runtime = config["runtime_profiles"]["default"].clone();
        runtime["agentic"] = json!(true);
        runtime["max_delegation_depth"] = json!(0);
        runtime["max_tool_iterations"] = json!(if alias == "coding" { 40 } else { 16 });
        runtime["delegation_timeout_secs"] = json!(if alias == "coding" { 900 } else { 180 });
        runtime["parallel_tools"] = json!(false);
        if alias == "coding" {
            runtime["thinking"]["default_level"] = json!("high");
        }
        add(format!("/runtime_profiles/{alias}"), runtime);
        add(
            format!("/agents/{alias}"),
            json!({"enabled":true,"channels":[],"cron_jobs":[],"delegate_same_risk_profile":false,"delegates":[],"model_provider":provider,"risk_profile":alias,"runtime_profile":alias,"mcp_bundles":bundles,"skill_bundles":[],"knowledge_bundles":[],"memory":{"backend":"sqlite"},"workspace":{"path":root.join("agents").join(alias).join("workspace"),"read_memory_from":[],"unrestricted_filesystem":false}}),
        );
    }
    Ok(json!(ops))
}

pub fn candidate(config: &Value, root: &Path, github: &Path) -> Result<Value> {
    let edits = patch(config, root, github)?;
    let mut result = config.clone();
    for op in edits.as_array().context("patch array")? {
        let path = op["path"].as_str().context("patch path")?;
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        let (last, parents) = parts.split_last().context("patch path")?;
        let mut target = &mut result;
        for key in parents {
            target = target
                .as_object_mut()
                .context("patch parent")?
                .entry((*key).to_owned())
                .or_insert_with(|| json!({}));
        }
        if *last == "-" {
            target
                .as_array_mut()
                .context("append array")?
                .push(op["value"].clone());
        } else {
            target
                .as_object_mut()
                .context("patch object")?
                .insert((*last).to_owned(), op["value"].clone());
        }
    }
    Ok(result)
}

pub fn install(root: &Path, github: &Path) -> Result<()> {
    ensure!(
        root.is_absolute() && github.is_absolute(),
        "absolute operator paths required"
    );
    let raw = fs::read_to_string(root.join("config.toml"))?;
    let config: Value = toml::from_str::<toml::Value>(&raw)?.try_into()?;
    let proposed = candidate(&config, root, github)?;
    let backup = root.join("backups").join(format!(
        "personal-specialists-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
    ));
    private_dir(&backup)?;
    private_write(&backup.join("config.toml"), raw.as_bytes())?;
    let main_path = root.join("agents/main/workspace/AGENTS.md");
    let original = fs::read_to_string(&main_path)?;
    private_write(&backup.join("main-AGENTS.md"), original.as_bytes())?;
    // Validate a complete candidate with the installed native loader before
    // writing live state. The native patch endpoint itself only accepts leaves.
    let stage = backup.join("validation");
    private_dir(&stage)?;
    let candidate_raw = toml::to_string_pretty(&proposed)?;
    private_write(&stage.join("config.toml"), candidate_raw.as_bytes())?;
    if root.join(".secret_key").exists() {
        private_write(
            &stage.join(".secret_key"),
            &fs::read(root.join(".secret_key"))?,
        )?;
    }
    let patch_path = backup.join("validate.json");
    private_write(&patch_path,serde_json::to_string(&json!([{"op":"replace","path":"/runtime_profiles/default/max_delegation_depth","value":1}]))?.as_bytes())?;
    let cli_home = std::env::var("HOME").context("HOME missing")?;
    let cli = Path::new(&cli_home).join(".cargo/bin/zeroclaw");
    let validation = Command::new(&cli)
        .arg("--config-dir")
        .arg(&stage)
        .args(["config", "patch"])
        .arg(&patch_path)
        .output()?;
    ensure!(
        validation.status.success(),
        "native candidate validation failed: {}",
        String::from_utf8_lossy(&validation.stderr)
    );
    for (alias, _) in ROLES {
        let check = Command::new(&cli)
            .arg("--config-dir")
            .arg(&stage)
            .args(["config", "get", &format!("agents.{alias}.model_provider")])
            .output()?;
        ensure!(
            check.status.success()
                && String::from_utf8_lossy(&check.stdout).contains(if alias == "coding" {
                    "openai.astra"
                } else {
                    "openai.terra"
                }),
            "native loader lost specialist {alias}"
        );
    }
    for (alias, body) in ROLES {
        let dir = root.join("agents").join(alias).join("workspace");
        ensure!(
            !dir.exists(),
            "specialist workspace already exists; refusing overwrite"
        );
        private_dir(&dir)?;
        private_write(
            &dir.join("AGENTS.md"),
            format!("{COMMON}\n{body}\n").as_bytes(),
        )?;
    }
    let helper_dir = root.join("extensions/personal-ops");
    private_dir(&helper_dir)?;
    let share = root.join("agents/main/workspace/share");
    private_dir(&share)?;
    private_write(
        &helper_dir.join("sharing.json"),
        &serde_json::to_vec_pretty(&json!({"allowed_roots":[github,share]}))?,
    )?;
    let bin = root.join("bin/zeroclaw-personal-ops");
    ensure!(
        !bin.exists(),
        "helper already exists; explicit upgrade required"
    );
    fs::copy(std::env::current_exe()?, &bin)?;
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700))?;
    ensure!(
        fs::read_to_string(root.join("config.toml"))? == raw,
        "live config changed during preparation; rerun against current state"
    );
    let pending = root.join("config.toml.personal-ops-next");
    private_write(&pending, candidate_raw.as_bytes())?;
    fs::rename(pending, root.join("config.toml"))?;
    let now = fs::read_to_string(root.join("config.toml"))?;
    let installed: Value = toml::from_str::<toml::Value>(&now)?.try_into()?;
    for key in [
        "cron",
        "channels",
        "scheduler",
        "codex_cli",
        "transcription",
    ] {
        ensure!(
            config[key] == installed[key],
            "unrelated configuration changed: {key}"
        );
    }
    let phone = |v: &Value| {
        v["mcp"]["servers"]
            .as_array()
            .and_then(|a| a.iter().find(|s| s["name"] == "phone_calls"))
            .cloned()
    };
    ensure!(
        phone(&config) == phone(&installed),
        "phone MCP configuration changed"
    );
    let next = main_path.with_extension("md.next");
    private_write(&next, format!("{original}\n{ROUTING}\n").as_bytes())?;
    fs::rename(next, &main_path)?;
    println!(
        "Installed four bounded specialists. Backup: {}. Restart the main daemon to load them; do not restart the phone service.",
        backup.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patch_does_not_target_phone_or_schedules() -> Result<()> {
        let c = json!({"agents":{"main":{"mcp_bundles":["phone_calls"],"delegates":[]}},"risk_profiles":{"default":{"auto_approve":[]}},"runtime_profiles":{"default":{}},"providers":{"models":{"openai":{"sol":{"model":"existing"}}}},"mcp":{"servers":[{"name":"phone_calls"}]}});
        let p = patch(
            &c,
            Path::new("/example/config"),
            Path::new("/example/repos"),
        )?;
        for op in p.as_array().context("array")? {
            let path = op["path"].as_str().context("path")?;
            assert!(
                !path.starts_with("/cron")
                    && !path.starts_with("/channels")
                    && !path.contains("phone_calls")
            );
        }
        assert!(
            p.as_array()
                .context("array")?
                .iter()
                .any(|op| op["path"] == "/providers/models/openai/astra"
                    && op["value"]["model"] == "gpt-6-astra")
        );
        Ok(())
    }
}
