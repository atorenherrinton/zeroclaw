use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub type SafeResult<T> = Result<T, &'static str>;

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

fn root_dir() -> SafeResult<PathBuf> {
    Ok(zeroclaw_dir()?.join("call-recordings"))
}

fn config_file() -> SafeResult<PathBuf> {
    Ok(root_dir()?.join("config.json"))
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub listen_addr: String,
    pub upstream_addr: String,
    pub public_base_url: String,
    pub webhook_path: String,
    pub media_path_prefix: String,
    pub database_path: PathBuf,
    pub archive_dir: PathBuf,
    pub max_audio_bytes: usize,
    pub max_call_seconds: u64,
    pub expected_telegram_owner: String,
    pub expected_bot_username: String,
}

pub struct Credentials {
    pub account_sid: String,
    pub auth_token: String,
}

pub struct TelegramCredentials {
    pub token: String,
    pub owner: String,
}

pub fn check(condition: bool, error: &'static str) -> SafeResult<()> {
    if condition { Ok(()) } else { Err(error) }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

pub fn log(event: &'static str) {
    eprintln!("{}", json!({"time_ms": now_ms(), "event": event}));
}

pub fn private_read(path: &Path, max_bytes: u64) -> SafeResult<String> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "private_file_unavailable")?;
    let metadata = file
        .metadata()
        .map_err(|_| "private_file_metadata_failed")?;
    let uid = unsafe { libc::geteuid() };
    check(
        metadata.is_file()
            && metadata.uid() == uid
            && metadata.mode() & 0o077 == 0
            && metadata.len() > 0
            && metadata.len() <= max_bytes,
        "unsafe_private_file",
    )?;
    let mut value = String::new();
    file.read_to_string(&mut value)
        .map_err(|_| "private_file_invalid")?;
    Ok(value)
}

pub fn private_directory(path: &Path) -> SafeResult<()> {
    if !path.exists() {
        fs::create_dir(path).map_err(|_| "private_directory_create_failed")?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| "private_directory_permissions_failed")?;
    }
    let meta = fs::symlink_metadata(path).map_err(|_| "private_directory_unavailable")?;
    check(
        meta.is_dir()
            && !meta.file_type().is_symlink()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "unsafe_private_directory",
    )
}

fn toml_leaf<'a>(root: &'a toml::Value, key: &str) -> Option<&'a toml::Value> {
    key.split('.').try_fold(root, |node, part| node.get(part))
}

pub fn telegram_credentials(config: &Config) -> SafeResult<TelegramCredentials> {
    let zeroclaw = zeroclaw_dir()?;
    let raw = private_read(&zeroclaw.join("config.toml"), 2 * 1024 * 1024)?;
    let root: toml::Value = toml::from_str(&raw).map_err(|_| "telegram_config_invalid")?;
    check(
        toml_leaf(&root, "channels.telegram.luma.enabled").and_then(toml::Value::as_bool)
            == Some(true),
        "telegram_channel_not_enabled",
    )?;
    check(
        toml_leaf(&root, "peer_groups.telegram_luma.channel").and_then(toml::Value::as_str)
            == Some("telegram.luma"),
        "telegram_peer_scope_changed",
    )?;
    let peers = toml_leaf(&root, "peer_groups.telegram_luma.external_peers")
        .and_then(toml::Value::as_array)
        .ok_or("telegram_owner_missing")?;
    check(peers.len() == 1, "telegram_owner_not_unique")?;
    let owner = peers[0]
        .as_str()
        .ok_or("telegram_owner_invalid")?
        .to_owned();
    check(
        owner == config.expected_telegram_owner
            && owner.parse::<u64>().ok().is_some_and(|id| id > 0),
        "telegram_owner_changed",
    )?;
    let stored = toml_leaf(&root, "channels.telegram.luma.bot_token")
        .and_then(toml::Value::as_str)
        .ok_or("telegram_token_missing")?;
    check(stored.starts_with("enc2:"), "telegram_token_not_encrypted")?;
    let key = private_read(&zeroclaw.join(".secret_key"), 256)?;
    check(
        key.trim().len() == 64 && key.trim().bytes().all(|byte| byte.is_ascii_hexdigit()),
        "secret_key_invalid",
    )?;
    let token = zeroclaw_config::secrets::SecretStore::new(&zeroclaw, true)
        .decrypt(stored)
        .map_err(|_| "telegram_token_decryption_failed")?;
    check(
        token.split_once(':').is_some_and(|(id, tail)| {
            !id.is_empty()
                && id.bytes().all(|b| b.is_ascii_digit())
                && tail.len() >= 20
                && tail
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        }),
        "telegram_token_invalid",
    )?;
    Ok(TelegramCredentials { token, owner })
}

fn read_secret(name: &str) -> SafeResult<String> {
    let path = openclaw_dir()?.join("state/openclaw.sqlite");
    let metadata = fs::symlink_metadata(&path).map_err(|_| "secret_database_unavailable")?;
    check(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "unsafe_secret_database",
    )?;
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "secret_database_open_failed")?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|_| "secret_database_timeout_failed")?;
    let mut query = connection.prepare("SELECT value FROM secret_store_entries WHERE scope_kind='team' AND scope_id='' AND name=?1 AND kind='secret' AND deleted_at_ms IS NULL")
        .map_err(|_| "secret_lookup_failed")?;
    let mut rows = query.query([name]).map_err(|_| "secret_lookup_failed")?;
    let value: String = rows
        .next()
        .map_err(|_| "secret_lookup_failed")?
        .ok_or("secret_missing")?
        .get(0)
        .map_err(|_| "secret_type_invalid")?;
    check(
        rows.next().map_err(|_| "secret_lookup_failed")?.is_none(),
        "secret_not_unique",
    )?;
    Ok(value)
}

pub fn credentials() -> SafeResult<Credentials> {
    let source: Value = serde_json::from_str(&private_read(
        &openclaw_dir()?.join("openclaw.json"),
        2 * 1024 * 1024,
    )?)
    .map_err(|_| "openclaw_config_invalid")?;
    for (field, id) in [
        ("accountSid", "TWILIO_ACCOUNT_SID"),
        ("authToken", "TWILIO_AUTH_TOKEN"),
    ] {
        let pointer = format!("/plugins/entries/voice-call/config/twilio/{field}");
        check(
            source.pointer(&pointer)
                == Some(&json!({"source":"store", "provider":"default", "id":id})),
            "twilio_secret_reference_changed",
        )?;
    }
    let account_sid = read_secret("TWILIO_ACCOUNT_SID")?;
    let auth_token = read_secret("TWILIO_AUTH_TOKEN")?;
    check(
        super::protocol::valid_sid(&account_sid, "AC")
            && auth_token.len() >= 20
            && auth_token.len() <= 256
            && !auth_token.chars().any(char::is_whitespace),
        "twilio_credential_invalid",
    )?;
    Ok(Credentials {
        account_sid,
        auth_token,
    })
}

pub fn load_config() -> SafeResult<Config> {
    let root = root_dir()?;
    let config_path = config_file()?;
    let config: Config = serde_json::from_str(&private_read(&config_path, 32768)?)
        .map_err(|_| "companion_config_invalid")?;
    check(
        config.listen_addr == "127.0.0.1:3335"
            && config.upstream_addr == "127.0.0.1:3334"
            && config.webhook_path == "/voice/webhook"
            && config.media_path_prefix == "/voice/stream/realtime/"
            && config.database_path == root.join("state.sqlite")
            && config.archive_dir == root.join("audio")
            && config.max_audio_bytes > 0
            && config.max_audio_bytes <= 20 * 1024 * 1024
            && config.max_call_seconds > 0
            && config.max_call_seconds <= 3600,
        "companion_config_out_of_scope",
    )?;
    let public = reqwest::Url::parse(&config.public_base_url).map_err(|_| "public_url_invalid")?;
    check(
        public.scheme() == "https"
            && public.host_str().is_some()
            && public.username().is_empty()
            && public.password().is_none()
            && public.port().is_none()
            && public.path() == "/"
            && public.query().is_none()
            && public.fragment().is_none()
            && !config.public_base_url.ends_with('/'),
        "public_url_invalid",
    )?;
    private_directory(&root)?;
    private_directory(&config.archive_dir)?;
    Ok(config)
}

pub fn initialize() -> SafeResult<Value> {
    let root = root_dir()?;
    let config_path = config_file()?;
    check(!config_path.exists(), "companion_config_already_exists")?;
    private_directory(&root)?;
    private_directory(&root.join("audio"))?;
    let source: Value = serde_json::from_str(&private_read(
        &openclaw_dir()?.join("openclaw.json"),
        2 * 1024 * 1024,
    )?)
    .map_err(|_| "openclaw_config_invalid")?;
    let voice = source
        .pointer("/plugins/entries/voice-call/config")
        .ok_or("voice_config_missing")?;
    let domain = voice
        .pointer("/tunnel/ngrokDomain")
        .and_then(Value::as_str)
        .ok_or("ngrok_domain_missing")?;
    check(
        !domain.is_empty()
            && domain
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-'),
        "ngrok_domain_invalid",
    )?;
    check(
        voice.pointer("/serve/port") == Some(&json!(3334))
            && voice.pointer("/serve/path") == Some(&json!("/voice/webhook")),
        "voice_ingress_changed",
    )?;
    let raw = private_read(&zeroclaw_dir()?.join("config.toml"), 2 * 1024 * 1024)?;
    let zc: toml::Value = toml::from_str(&raw).map_err(|_| "zeroclaw_config_invalid")?;
    let peers = toml_leaf(&zc, "peer_groups.telegram_luma.external_peers")
        .and_then(toml::Value::as_array)
        .ok_or("telegram_owner_missing")?;
    check(peers.len() == 1, "telegram_owner_not_unique")?;
    let config = Config {
        listen_addr: "127.0.0.1:3335".to_owned(),
        upstream_addr: "127.0.0.1:3334".to_owned(),
        public_base_url: format!("https://{domain}"),
        webhook_path: "/voice/webhook".to_owned(),
        media_path_prefix: "/voice/stream/realtime/".to_owned(),
        database_path: root.join("state.sqlite"),
        archive_dir: root.join("audio"),
        max_audio_bytes: 20 * 1024 * 1024,
        max_call_seconds: voice["maxDurationSeconds"].as_u64().unwrap_or(180),
        expected_telegram_owner: peers[0]
            .as_str()
            .ok_or("telegram_owner_invalid")?
            .to_owned(),
        expected_bot_username: std::env::var("ZEROCLAW_TELEGRAM_BOT_USERNAME")
            .map_err(|_| "telegram_bot_username_missing")?,
    };
    telegram_credentials(&config)?;
    credentials()?;
    let data = serde_json::to_vec_pretty(&config).map_err(|_| "config_encoding_failed")?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(config_path)
        .map_err(|_| "config_create_failed")?;
    file.write_all(&data)
        .and_then(|_| file.sync_all())
        .map_err(|_| "config_write_failed")?;
    open_database(&config)?;
    Ok(
        json!({"initialized":true, "secretsCopied":false, "publicRouteChanged":false, "recordingsStarted":0}),
    )
}

pub fn open_database(config: &Config) -> SafeResult<Connection> {
    if config.database_path.exists() {
        let meta =
            fs::symlink_metadata(&config.database_path).map_err(|_| "journal_unavailable")?;
        check(
            meta.is_file()
                && !meta.file_type().is_symlink()
                && meta.uid() == unsafe { libc::geteuid() }
                && meta.mode() & 0o077 == 0,
            "unsafe_journal",
        )?;
    }
    let connection = Connection::open(&config.database_path).map_err(|_| "journal_open_failed")?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(|_| "journal_timeout_failed")?;
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
        CREATE TABLE IF NOT EXISTS sessions (
            call_sid TEXT PRIMARY KEY, nonce_hash TEXT NOT NULL, context_hash TEXT NOT NULL,
            phase TEXT NOT NULL, consent INTEGER NOT NULL DEFAULT -1,
            created_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS recordings (
            recording_sid TEXT PRIMARY KEY, call_sid TEXT NOT NULL UNIQUE,
            duration_secs INTEGER NOT NULL, created_ms INTEGER NOT NULL,
            state TEXT NOT NULL, local_path TEXT, message_id INTEGER,
            attempts INTEGER NOT NULL DEFAULT 0, last_error TEXT
        );
    ",
        )
        .map_err(|_| "journal_schema_failed")?;
    Ok(connection)
}

pub fn status(config: &Config) -> SafeResult<Value> {
    let connection = open_database(config)?;
    let mut query = connection
        .prepare("SELECT state, count(*) FROM recordings GROUP BY state ORDER BY state")
        .map_err(|_| "journal_status_failed")?;
    let mut states = serde_json::Map::new();
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|_| "journal_status_failed")?;
    for row in rows {
        let (state, count) = row.map_err(|_| "journal_status_failed")?;
        states.insert(state, json!(count));
    }
    let calls: i64 = connection
        .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
        .map_err(|_| "journal_status_failed")?;
    Ok(
        json!({"managedCalls":calls, "recordings":states, "archiveDirectory":config.archive_dir,
        "automaticDeletion":false, "telegramOwnerCount":1}),
    )
}

pub fn ngrok_auth_token() -> SafeResult<String> {
    let raw = private_read(
        &openclaw_dir()?.join("service-env/ai.openclaw.gateway.env"),
        128 * 1024,
    )?;
    let mut values = Vec::new();
    for line in raw.lines() {
        let line = line.trim().strip_prefix("export ").unwrap_or(line.trim());
        if let Some(value) = line.strip_prefix("NGROK_AUTHTOKEN=") {
            let value = value.trim();
            let value = if value.starts_with('"') && value.ends_with('"')
                || value.starts_with('\'') && value.ends_with('\'')
            {
                &value[1..value.len() - 1]
            } else {
                value
            };
            check(
                (20..=512).contains(&value.len())
                    && value
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-:".contains(&b)),
                "ngrok_token_format_unsupported",
            )?;
            values.push(value.to_owned());
        }
    }
    check(values.len() == 1, "ngrok_token_not_unique")?;
    Ok(values.remove(0))
}
