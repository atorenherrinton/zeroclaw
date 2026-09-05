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
        if name != "personal_ops__imessage_approve" {
            push_unique(&mut auto, json!(name))?;
        }
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
                    "personal_ops__contacts_search",
                    "personal_ops__contacts_get",
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
                    "reminders__list_lists",
                    "reminders__create_list",
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
    ops.extend(
        native_routing_patch(config)?
            .as_array()
            .context("routing patch")?
            .clone(),
    );
    Ok(json!(ops))
}

/// The resilient provider advertises native tools only when every fallback
/// supports them. Gemini currently uses text tools, so including it disables
/// native tool definitions even on successful primary OpenAI requests.
pub fn native_routing_patch(config: &Value) -> Result<Value> {
    for alias in ["sol", "terra"] {
        let provider = &config["providers"]["models"]["openai"][alias];
        ensure!(
            provider.is_object() && provider["requires_openai_auth"] == true,
            "native OpenAI authentication required for {alias}"
        );
    }
    Ok(json!([
        {"op":"add","path":"/providers/models/openai/sol/fallback","value":["openai.terra"]},
        {"op":"add","path":"/providers/models/openai/terra/fallback","value":[]}
    ]))
}

/// Operator-only upgrade: validated native patch, private rollback copy, no
/// agent instructions, tool policy, channel configuration, or phone changes.
pub fn repair_routing(root: &Path) -> Result<()> {
    ensure!(root.is_absolute(), "absolute CONFIG_DIR required");
    let raw = fs::read_to_string(root.join("config.toml"))?;
    let config: Value = toml::from_str::<toml::Value>(&raw)?.try_into()?;
    let patch = native_routing_patch(&config)?;
    let backup = root
        .join("backups")
        .join(format!("native-tool-routing-{}", uuid::Uuid::new_v4()));
    private_dir(&backup)?;
    private_write(&backup.join("config.toml"), raw.as_bytes())?;
    let path = backup.join("routing-patch.json");
    private_write(&path, &serde_json::to_vec_pretty(&patch)?)?;
    ensure!(
        fs::read_to_string(root.join("config.toml"))? == raw,
        "live config changed during preparation"
    );
    let cli_home = std::env::var("HOME").context("HOME missing")?;
    let result = Command::new(Path::new(&cli_home).join(".cargo/bin/zeroclaw"))
        .arg("--config-dir")
        .arg(root)
        .args(["config", "patch"])
        .arg(path)
        .output()?;
    ensure!(
        result.status.success(),
        "native routing patch failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    println!(
        "Native tool routing enabled: Sol falls back to Terra; Terra has no cross-provider fallback. Backup: {}. Restart the main daemon to refresh its channel prompt; leave the phone service running.",
        backup.display()
    );
    Ok(())
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

/// Add one native cron wakeup and a worker with no inbound channel or delegate
/// route. Per-message due times and approvals remain solely in the outbox.
pub fn message_config(config: &Value, root: &Path) -> Result<Value> {
    let mut next = config.clone();
    let gate = "personal_ops__imessage_approve";
    let profile = &mut next["risk_profiles"]["default"];
    ensure!(
        profile["level"] == "supervised",
        "Telegram message approval requires supervised main risk profile"
    );
    ensure!(
        next["agents"]["main"]["risk_profile"] == "default",
        "main must use default risk profile"
    );
    let profile = &mut next["risk_profiles"]["default"];
    profile["auto_approve"]
        .as_array_mut()
        .context("auto_approve")?
        .retain(|v| v != gate && v != "personal_ops__delivery_execute");
    if !profile["always_ask"].is_array() {
        profile["always_ask"] = json!([]);
    }
    push_unique(&mut profile["always_ask"], json!(gate))?;
    push_unique(
        &mut profile["always_ask"],
        json!("personal_ops__delivery_execute"),
    )?;
    for name in ["imessage_draft", "imessage_list", "imessage_cancel"] {
        push_unique(
            &mut profile["auto_approve"],
            json!(format!("personal_ops__{name}")),
        )?;
    }
    for alias in ["communications", "task_scheduler"] {
        let risk = &mut next["risk_profiles"][alias];
        for field in ["allowed_tools", "auto_approve"] {
            for name in if alias == "communications" {
                vec!["imessage_draft", "imessage_list"]
            } else {
                vec!["imessage_list", "imessage_cancel"]
            } {
                push_unique(&mut risk[field], json!(format!("personal_ops__{name}")))?;
            }
        }
        push_unique(
            &mut next["agents"][alias]["mcp_bundles"],
            json!("personal_ops"),
        )?;
    }
    let worker = "imessage_delivery";
    if next["agents"].get(worker).is_some() {
        ensure!(
            next["cron"][worker]["name"] == "Approved iMessage dispatcher",
            "worker name is already in use"
        );
    }
    let binary = root.join("bin/zeroclaw-personal-ops");
    let mut risk = next["risk_profiles"]["default"].clone();
    risk["allowed_tools"] = json!(["shell"]);
    risk["auto_approve"] = json!(["shell"]);
    risk["always_ask"] = json!([]);
    risk["excluded_tools"] = json!([]);
    risk["allowed_commands"] = json!([binary]);
    risk["allowed_roots"] = json!([root]);
    risk["require_approval_for_medium_risk"] = json!(false);
    risk["delegation_policy"] = json!({"mode":"forbidden"});
    next["risk_profiles"][worker] = risk;
    let mut runtime = next["runtime_profiles"]["default"].clone();
    runtime["agentic"] = json!(false);
    runtime["max_delegation_depth"] = json!(0);
    // Sixty fixed wakeups per hour plus bounded retries; do not inherit a
    // conversational profile's lower action cap and silently miss schedules.
    runtime["max_actions_per_hour"] = json!(120);
    next["runtime_profiles"][worker] = runtime;
    next["agents"][worker] = json!({"enabled":true,"channels":[],"cron_jobs":[worker],"delegates":[],"model_provider":"openai.sol","risk_profile":worker,"runtime_profile":worker,"mcp_bundles":[],"skill_bundles":[],"knowledge_bundles":[],"memory":{"backend":"none"},"workspace":{"path":root.join("agents").join(worker).join("workspace"),"read_memory_from":[],"unrestricted_filesystem":false}});
    let quote = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"));
    next["cron"][worker] = json!({"name":"Approved iMessage dispatcher","job_type":"shell","enabled":true,"schedule":{"kind":"every","every_ms":60000},"command":format!("{} dispatch-messages {}",quote(&binary),quote(root)),"allowed_tools":[],"uses_memory":false,"shell_output_format":"raw","delivery":{"mode":"none"}});
    Ok(next)
}

pub fn enable_messages(root: &Path) -> Result<()> {
    ensure!(root.is_absolute(), "absolute CONFIG_DIR required");
    let raw = fs::read_to_string(root.join("config.toml"))?;
    let config: Value = toml::from_str::<toml::Value>(&raw)?.try_into()?;
    let next = message_config(&config, root)?;
    let backup = root
        .join("backups")
        .join(format!("imessage-review-{}", uuid::Uuid::new_v4()));
    private_dir(&backup)?;
    private_write(&backup.join("config.toml"), raw.as_bytes())?;
    let stage = backup.join("validation");
    private_dir(&stage)?;
    let rendered = toml::to_string_pretty(&next)?;
    private_write(&stage.join("config.toml"), rendered.as_bytes())?;
    if root.join(".secret_key").exists() {
        private_write(
            &stage.join(".secret_key"),
            &fs::read(root.join(".secret_key"))?,
        )?;
    }
    let check = backup.join("validate.json");
    private_write(
        &check,
        b"[{\"op\":\"replace\",\"path\":\"/cron/imessage_delivery/enabled\",\"value\":true}]",
    )?;
    let home = std::env::var("HOME")?;
    let result = Command::new(Path::new(&home).join(".cargo/bin/zeroclaw"))
        .arg("--config-dir")
        .arg(&stage)
        .args(["config", "patch"])
        .arg(check)
        .output()?;
    ensure!(
        result.status.success(),
        "candidate validation failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    // All policy and tool changes are validated before live mutation.
    ensure!(
        fs::read_to_string(root.join("config.toml"))? == raw,
        "live config changed during validation"
    );
    let binary = root.join("bin/zeroclaw-personal-ops");
    private_write(&backup.join("zeroclaw-personal-ops"), &fs::read(&binary)?)?;
    fs::set_permissions(
        backup.join("zeroclaw-personal-ops"),
        fs::Permissions::from_mode(0o700),
    )?;
    let pending_binary = binary.with_extension("messages-next");
    private_write(&pending_binary, &fs::read(std::env::current_exe()?)?)?;
    fs::set_permissions(&pending_binary, fs::Permissions::from_mode(0o700))?;
    fs::rename(pending_binary, binary)?;
    let pending = root.join("config.toml.messages-next");
    private_write(&pending, rendered.as_bytes())?;
    fs::rename(pending, root.join("config.toml"))?;
    private_dir(&root.join("agents/imessage_delivery/workspace"))?;
    for alias in ["main", "communications", "task_scheduler"] {
        let path = root.join("agents").join(alias).join("workspace/AGENTS.md");
        let original = fs::read_to_string(&path)?;
        private_write(
            &backup.join(format!("{alias}-AGENTS.md")),
            original.as_bytes(),
        )?;
        let section = include_str!("../templates/imessage-review.md");
        if !original.contains("## Reviewed iMessage drafts and schedules") {
            let pending = path.with_extension("messages-next");
            private_write(&pending, format!("{original}\n{section}").as_bytes())?;
            fs::rename(pending, path)?;
        }
    }
    println!(
        "Enabled Telegram-reviewed iMessage drafts and native dispatch wakeup. Backup: {}. Restart only the main daemon.",
        backup.display()
    );
    Ok(())
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
    private_write(
        &next,
        format!(
            "{original}\n{ROUTING}\n{}\n",
            include_str!("../templates/contacts.md")
        )
        .as_bytes(),
    )?;
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
        let c = json!({"agents":{"main":{"mcp_bundles":["phone_calls"],"delegates":[]}},"risk_profiles":{"default":{"auto_approve":[]}},"runtime_profiles":{"default":{}},"providers":{"models":{"openai":{"sol":{"model":"existing","requires_openai_auth":true},"terra":{"model":"existing","requires_openai_auth":true}}}},"mcp":{"servers":[{"name":"phone_calls"}]}});
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

    #[test]
    fn native_routing_removes_mixed_protocol_and_cycles() -> Result<()> {
        let c = json!({"providers":{"models":{"openai":{
            "sol":{"requires_openai_auth":true,"fallback":["gemini.flash"]},
            "terra":{"requires_openai_auth":true,"fallback":["gemini.flash","openai.sol"]}
        }}}});
        let patch = native_routing_patch(&c)?;
        let mut repaired = c;
        for op in patch.as_array().context("patch")? {
            let target = repaired
                .pointer_mut(op["path"].as_str().context("path")?)
                .context("target")?;
            *target = op["value"].clone();
        }
        assert_eq!(
            repaired["providers"]["models"]["openai"]["sol"]["fallback"],
            json!(["openai.terra"])
        );
        assert_eq!(
            repaired["providers"]["models"]["openai"]["terra"]["fallback"],
            json!([])
        );
        assert_eq!(native_routing_patch(&repaired)?, patch);
        assert_eq!(patch.as_array().context("patch")?.len(), 2);
        Ok(())
    }

    #[test]
    fn native_routing_rejects_unverified_provider_family() {
        assert!(native_routing_patch(&json!({})).is_err());
    }

    #[test]
    fn reviewed_message_install_preserves_services_and_requires_human_gate() -> Result<()> {
        let c = json!({"agents":{"main":{"risk_profile":"default"},"communications":{"mcp_bundles":[]},"task_scheduler":{"mcp_bundles":[]}},"risk_profiles":{"default":{"level":"supervised","auto_approve":["personal_ops__delivery_execute"],"always_ask":[]},"communications":{"allowed_tools":[],"auto_approve":[]},"task_scheduler":{"allowed_tools":[],"auto_approve":[]}},"runtime_profiles":{"default":{}},"cron":{"existing":{"enabled":true}},"channels":{"fixture":true},"mcp":{"servers":[{"name":"phone_calls"}]}});
        let n = message_config(&c, Path::new("/example/root"))?;
        assert_eq!(n["channels"], c["channels"]);
        assert_eq!(n["mcp"], c["mcp"]);
        assert_eq!(n["cron"]["existing"], c["cron"]["existing"]);
        let gate = json!("personal_ops__imessage_approve");
        assert!(
            n["risk_profiles"]["default"]["always_ask"]
                .as_array()
                .context("gates")?
                .contains(&gate)
        );
        for alias in ["communications", "task_scheduler", "imessage_delivery"] {
            assert!(
                !n["risk_profiles"][alias]["allowed_tools"]
                    .as_array()
                    .context("tools")?
                    .contains(&gate)
            );
        }
        assert_eq!(n["agents"]["imessage_delivery"]["channels"], json!([]));
        assert_eq!(n["cron"]["imessage_delivery"]["job_type"], "shell");
        assert_eq!(message_config(&n, Path::new("/example/root"))?, n);
        let mut unsafe_config = c;
        unsafe_config["risk_profiles"]["default"]["level"] = json!("full");
        assert!(message_config(&unsafe_config, Path::new("/example/root")).is_err());
        Ok(())
    }
}
