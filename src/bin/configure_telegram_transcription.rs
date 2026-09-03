//! Local, one-shot voice-key migration. No speech API calls are made.
//! Secrets are read into memory and piped to the native CLI, never put in
//! argv, environment variables, plaintext files, or diagnostic output.
//!
//! The operator must provide absolute `ZEROCLAW_CONFIG_DIR`,
//! `OPENCLAW_CONFIG_DIR`, `ZEROCLAW_BIN`, and `ZEROCLAW_BACKUP_DIR` paths.
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const MODEL: &str = "gpt-transcribe";
const LEGACY: &str = "transcription.openai";
const TYPED: &str = "providers.transcription.openai.luma";
type SafeResult<T> = Result<T, &'static str>;

fn check(condition: bool, code: &'static str) -> SafeResult<()> {
    if condition { Ok(()) } else { Err(code) }
}

fn required_path(name: &str) -> SafeResult<PathBuf> {
    let path = std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or("required_path_missing")?;
    check(path.is_absolute(), "required_path_not_absolute")?;
    Ok(path)
}

fn zeroclaw_dir() -> SafeResult<PathBuf> {
    required_path("ZEROCLAW_CONFIG_DIR")
}

fn openclaw_dir() -> SafeResult<PathBuf> {
    required_path("OPENCLAW_CONFIG_DIR")
}

fn private_read(path: &Path) -> SafeResult<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "private_file_open_failed")?;
    let metadata = file
        .metadata()
        .map_err(|_| "private_file_metadata_failed")?;
    // geteuid has no preconditions and does not change process state.
    let uid = unsafe { libc::geteuid() };
    check(
        metadata.is_file()
            && metadata.len() > 0
            && metadata.len() < 2 * 1024 * 1024
            && metadata.mode() & 0o077 == 0
            && metadata.uid() == uid,
        "unsafe_private_file",
    )?;
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|_| "private_file_read_failed")?;
    Ok(text)
}

fn cli(arguments: &[&str], patch: Option<&Value>) -> SafeResult<Value> {
    let binary = required_path("ZEROCLAW_BIN")?;
    let config_dir = zeroclaw_dir()?;
    let mut child = Command::new(binary)
        .arg("--config-dir")
        .arg(config_dir)
        .args(["--log-level", "error"])
        .args(arguments)
        .arg("--json")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "native_cli_start_failed")?;
    if let Some(patch) = patch {
        let mut bytes = serde_json::to_vec(patch).map_err(|_| "patch_encoding_failed")?;
        let written = child
            .stdin
            .take()
            .ok_or("native_cli_stdin_unavailable")?
            .write_all(&bytes);
        bytes.fill(0);
        written.map_err(|_| "native_cli_stdin_failed")?;
    } else {
        drop(child.stdin.take());
    }
    let output = child
        .wait_with_output()
        .map_err(|_| "native_cli_wait_failed")?;
    check(
        output.status.success() && output.stdout.len() < 1024 * 1024,
        "native_cli_failed",
    )?;
    let result: Value =
        serde_json::from_slice(&output.stdout).map_err(|_| "native_cli_json_failed")?;
    check(
        result.get("code").is_none() && result.get("error").is_none_or(Value::is_null),
        "native_config_rejected",
    )?;
    Ok(result)
}

fn get(key: &str) -> SafeResult<String> {
    cli(&["config", "get", key], None)?["value"]
        .as_str()
        .map(str::to_owned)
        .ok_or("native_get_value_missing")
}

fn list(key: &str) -> SafeResult<Value> {
    serde_json::from_str(&get(key)?).map_err(|_| "native_get_list_failed")
}

fn operation(op: &str, key: &str, value: Value) -> Value {
    json!({"op": op, "path": format!("/{}", key.replace('.', "/")), "value": value})
}

fn toml_document(raw: &str) -> SafeResult<toml::Value> {
    toml::from_str(raw).map_err(|_| "config_toml_invalid")
}

fn leaf<'a>(root: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    key.split('.').try_fold(root, |node, part| node.get(part))
}

fn verify() -> SafeResult<Value> {
    let saved = private_read(&zeroclaw_dir()?.join("config.toml"))?;
    let document = toml_document(&saved)?;
    for section in [LEGACY, TYPED] {
        let key_path = format!("{section}.api_key");
        let stored = leaf(&document, &key_path)
            .and_then(toml::Value::as_str)
            .ok_or("credential_missing")?;
        check(
            stored.strip_prefix("enc2:").is_some_and(|hex| {
                hex.len() >= 58 && hex.len() % 2 == 0 && hex.bytes().all(|b| b.is_ascii_hexdigit())
            }),
            "credential_not_encrypted",
        )?;
        check(
            cli(&["config", "get", &key_path], None)?["populated"] == true,
            "credential_not_populated",
        )?;
        check(
            get(&format!("{section}.model"))? == MODEL,
            "transcription_model_mismatch",
        )?;
    }
    for (key, expected) in [
        ("transcription.enabled", "true"),
        ("transcription.max_duration_secs", "120"),
        ("transcription.max_audio_bytes", "10485760"),
        ("agents.main.transcription_provider", "openai.luma"),
        ("channels.telegram.luma.enabled", "true"),
        ("channels.signal.main.enabled", "false"),
        ("peer_groups.telegram_luma.output_modality", "text"),
        ("tts.enabled", "false"),
        ("secrets.encrypt", "true"),
    ] {
        check(get(key)? == expected, "saved_setting_mismatch")?;
    }
    let peers = list("peer_groups.telegram_luma.external_peers")?;
    check(
        peers.as_array().is_some_and(|array| {
            array.len() == 1
                && array[0]
                    .as_str()
                    .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        }),
        "owner_policy_mismatch",
    )?;
    let validation = cli(&["config", "migrate"], None)?;
    check(
        validation["valid"] == true
            && validation["migrated"] == false
            && validation["schema_version"] == 3,
        "strict_validation_failed",
    )?;
    Ok(
        json!({"configured": true, "encryptedCredentials": 2, "model": MODEL,
        "maxSeconds": 120, "maxBytes": 10485760, "pairedOwners": 1,
        "textReplies": true, "configValid": true, "speechApiCalls": 0}),
    )
}

fn status() -> SafeResult<Value> {
    let token = private_read(&zeroclaw_dir()?.join("migration/local-api-token"))?;
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|_| "local_client_failed")?;
    let health: Value = client
        .get("http://127.0.0.1:42617/api/health")
        .bearer_auth(token.trim())
        .send()
        .map_err(|_| "local_health_unavailable")?
        .error_for_status()
        .map_err(|_| "local_health_rejected")?
        .json()
        .map_err(|_| "local_health_invalid")?;
    let components = &health["health"]["components"];
    let telegram_ok = components["channel:telegram.luma"]["status"] == "ok";
    check(telegram_ok, "telegram_not_healthy")?;
    let config: Value = client
        .get("http://127.0.0.1:42617/api/config")
        .bearer_auth(token.trim())
        .send()
        .map_err(|_| "runtime_config_unavailable")?
        .error_for_status()
        .map_err(|_| "runtime_config_rejected")?
        .json()
        .map_err(|_| "runtime_config_invalid")?;
    let voice_loaded = config["transcription"]["enabled"] == true
        && config["transcription"]["openai"]["model"] == MODEL
        && config["providers"]["transcription"]["openai"]["luma"]["model"] == MODEL
        && config["agents"]["main"]["transcription_provider"] == "openai.luma";
    Ok(
        json!({"telegramHealthy": true, "gatewayReachable": true, "voiceSettingsLoaded": voice_loaded}),
    )
}

fn run(mode: &str, phase: &mut &'static str, backup: &mut Option<PathBuf>) -> SafeResult<Value> {
    if mode == "--verify" {
        return verify();
    }
    if mode == "--status" {
        return status();
    }
    check(
        mode == "--check" || mode == "--apply",
        "expected_check_apply_verify_or_status",
    )?;
    let zeroclaw = zeroclaw_dir()?;
    let openclaw = openclaw_dir()?;
    let config_path = zeroclaw.join("config.toml");
    let source_path = openclaw.join("openclaw.json");
    let metadata = fs::symlink_metadata(&zeroclaw).map_err(|_| "config_directory_unavailable")?;
    check(
        metadata.is_dir() && !metadata.file_type().is_symlink() && metadata.mode() & 0o077 == 0,
        "unsafe_config_directory",
    )?;
    let original = private_read(&config_path)?;
    let source = private_read(&source_path)?;
    let encryption_key = private_read(&zeroclaw.join(".secret_key"))?;
    check(
        encryption_key.trim().len() == 64
            && encryption_key.trim().bytes().all(|b| b.is_ascii_hexdigit()),
        "existing_encryption_key_invalid",
    )?;
    let document = toml_document(&original)?;
    let source_document: Value =
        serde_json::from_str(&source).map_err(|_| "source_config_invalid")?;
    check(
        source_document
            .pointer("/plugins/entries/voice-call/config/realtime/providers/openai/apiKey")
            == Some(&json!({"source":"store","provider":"default","id":"OPENAI_VOICE_API_KEY"})),
        "source_secret_ref_changed",
    )?;
    for (key, expected) in [
        ("transcription.enabled", "false"),
        ("agents.main.transcription_provider", ""),
        ("channels.telegram.luma.enabled", "true"),
        ("channels.signal.main.enabled", "false"),
        ("tts.enabled", "false"),
        ("secrets.encrypt", "true"),
        ("peer_groups.telegram_luma.output_modality", "text"),
    ] {
        check(get(key)? == expected, "unexpected_preflight_setting")?;
    }
    let peers = list("peer_groups.telegram_luma.external_peers")?;
    check(
        peers.as_array().is_some_and(|array| {
            array.len() == 1
                && array[0]
                    .as_str()
                    .is_some_and(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
        }),
        "expected_one_paired_owner",
    )?;
    let channels = list("agents.main.channels")?;
    check(
        channels.as_array().is_some_and(|array| {
            array.len() == 2
                && array.contains(&json!("telegram.luma"))
                && array.contains(&json!("signal.main"))
        }),
        "unexpected_channel_bindings",
    )?;
    check(
        leaf(&document, LEGACY).is_none() && leaf(&document, TYPED).is_none(),
        "provider_already_exists",
    )?;
    if mode == "--check" {
        return Ok(json!({"ready": true, "pairedOwners": 1, "model": MODEL,
            "maxSeconds": 120, "maxBytes": 10485760, "speechApiCalls": 0}));
    }

    *phase = "read_existing_secret";
    let database_path = openclaw.join("state/openclaw.sqlite");
    let metadata =
        fs::symlink_metadata(&database_path).map_err(|_| "secret_database_unavailable")?;
    check(
        metadata.is_file() && !metadata.file_type().is_symlink() && metadata.mode() & 0o077 == 0,
        "unsafe_secret_database",
    )?;
    let connection = Connection::open_with_flags(
        database_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "secret_database_open_failed")?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|_| "secret_database_timeout_failed")?;
    let mut statement = connection.prepare("SELECT value FROM secret_store_entries WHERE scope_kind = 'team' AND scope_id = '' AND name = 'OPENAI_VOICE_API_KEY' AND kind = 'secret' AND deleted_at_ms IS NULL")
        .map_err(|_| "secret_lookup_failed")?;
    let mut rows = statement.query([]).map_err(|_| "secret_lookup_failed")?;
    let api_key: String = rows
        .next()
        .map_err(|_| "secret_lookup_failed")?
        .ok_or("source_secret_missing")?
        .get(0)
        .map_err(|_| "source_secret_type_invalid")?;
    check(
        rows.next().map_err(|_| "secret_lookup_failed")?.is_none(),
        "ambiguous_source_secret",
    )?;
    check(
        (20..=1024).contains(&api_key.len()) && !api_key.chars().any(char::is_whitespace),
        "source_secret_format_invalid",
    )?;
    drop(rows);
    drop(statement);
    drop(connection);
    check(
        Sha256::digest(private_read(&config_path)?) == Sha256::digest(&original),
        "config_changed_during_preflight",
    )?;

    *phase = "backup";
    let backup_root = required_path("ZEROCLAW_BACKUP_DIR")?;
    let directory = tempfile::Builder::new()
        .prefix("pre-telegram-voice-")
        .tempdir_in(backup_root)
        .map_err(|_| "backup_directory_failed")?
        .keep();
    *backup = Some(directory.clone());
    fs::copy(&config_path, directory.join("config.toml")).map_err(|_| "backup_copy_failed")?;
    fs::set_permissions(
        directory.join("config.toml"),
        fs::Permissions::from_mode(0o600),
    )
    .map_err(|_| "backup_permissions_failed")?;
    check(
        Sha256::digest(private_read(&directory.join("config.toml"))?) == Sha256::digest(&original),
        "backup_mismatch",
    )?;

    *phase = "initialize_provider_sections";
    for section in [LEGACY, TYPED] {
        let result = cli(&["config", "init", section], None)?;
        check(
            result["initialized"]
                .as_array()
                .is_some_and(|items| items.contains(&json!(section))),
            "provider_initialization_failed",
        )?;
    }

    *phase = "save_encrypted_configuration";
    let patch = Value::Array(vec![
        operation("test", "transcription.enabled", json!(false)),
        operation("test", "agents.main.transcription_provider", json!("")),
        operation("test", "channels.telegram.luma.enabled", json!(true)),
        operation("test", "channels.signal.main.enabled", json!(false)),
        operation("test", "tts.enabled", json!(false)),
        operation("test", "secrets.encrypt", json!(true)),
        operation("replace", &format!("{LEGACY}.api_key"), json!(api_key)),
        operation("replace", &format!("{LEGACY}.model"), json!(MODEL)),
        operation("replace", &format!("{TYPED}.api_key"), json!(api_key)),
        operation("replace", &format!("{TYPED}.model"), json!(MODEL)),
        operation("replace", "transcription.max_duration_secs", json!(120)),
        operation("replace", "transcription.max_audio_bytes", json!(10485760)),
        operation(
            "replace",
            "agents.main.transcription_provider",
            json!("openai.luma"),
        ),
        operation("replace", "transcription.enabled", json!(true)),
    ]);
    check(
        cli(&["config", "patch", "-"], Some(&patch))?["saved"] == true,
        "patch_not_saved",
    )?;
    drop(patch);

    *phase = "verify_saved_configuration";
    let saved = private_read(&config_path)?;
    check(
        !saved.contains(&api_key),
        "plaintext_secret_found_in_config",
    )?;
    check(
        list("peer_groups.telegram_luma.external_peers")? == peers,
        "owner_policy_changed",
    )?;
    check(
        list("agents.main.channels")? == channels,
        "channel_bindings_changed",
    )?;
    check(
        Sha256::digest(private_read(&source_path)?) == Sha256::digest(source),
        "source_config_changed",
    )?;
    let mut result = verify()?;
    result["backup"] = json!(directory);
    result["openclawConfigUnchanged"] = json!(true);
    result["restartRequired"] = json!(true);
    Ok(result)
}

fn main() {
    let arguments: Vec<String> = std::env::args().collect();
    let mut phase = "preflight";
    let mut backup = None;
    let result = if arguments.len() == 2 {
        run(&arguments[1], &mut phase, &mut backup)
    } else {
        Err("expected_one_mode")
    };
    match result {
        Ok(result) => println!("{result}"),
        Err(code) => {
            eprintln!(
                "{}",
                json!({"ok": false, "phase": phase, "code": code, "backup": backup})
            );
            std::process::exit(1);
        }
    }
}
