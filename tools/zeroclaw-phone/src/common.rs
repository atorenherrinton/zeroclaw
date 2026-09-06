//! Canonical settings are phone.toml plus the existing ZeroClaw config and key.
//! Resolve them at each admission/delivery, never from another runtime's state.
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::Duration,
};
use zeroclaw_config::secrets::SecretStore;

pub type SafeResult<T> = Result<T, &'static str>;

pub fn check(value: bool, error: &'static str) -> SafeResult<()> {
    if value { Ok(()) } else { Err(error) }
}

pub fn private_dir(path: &Path) -> SafeResult<()> {
    if !path.exists() {
        fs::create_dir(path).map_err(|_| "directory_create_failed")?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| "directory_permissions_failed")?;
    }
    let m = fs::symlink_metadata(path).map_err(|_| "directory_metadata_failed")?;
    // geteuid only reads this process's effective identity.
    check(
        m.is_dir()
            && !m.file_type().is_symlink()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0,
        "unsafe_private_directory",
    )
}

pub fn private_read(path: &Path) -> SafeResult<String> {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "private_read_open_failed")?;
    let m = f.metadata().map_err(|_| "private_read_metadata_failed")?;
    check(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.len() <= 2 * 1024 * 1024,
        "unsafe_private_file",
    )?;
    let mut value = String::new();
    f.read_to_string(&mut value)
        .map_err(|_| "private_read_failed")?;
    Ok(value)
}

pub fn atomic_private_write(path: &Path, bytes: &[u8]) -> SafeResult<()> {
    let parent = path.parent().ok_or("missing_parent")?;
    private_dir(parent)?;
    if path.exists() {
        let m = fs::symlink_metadata(path).map_err(|_| "target_metadata_failed")?;
        check(
            m.is_file() && !m.file_type().is_symlink() && m.uid() == unsafe { libc::geteuid() },
            "unsafe_write_target",
        )?;
    }
    let mut f = tempfile::NamedTempFile::new_in(parent).map_err(|_| "temporary_file_failed")?;
    f.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))
        .map_err(|_| "file_permissions_failed")?;
    f.write_all(bytes).map_err(|_| "private_write_failed")?;
    f.as_file().sync_all().map_err(|_| "private_sync_failed")?;
    f.persist(path).map_err(|_| "private_publish_failed")?;
    fs::File::open(parent)
        .and_then(|f| f.sync_all())
        .map_err(|_| "directory_sync_failed")
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhoneConfig {
    pub enabled: bool,
    pub port: u16,
    pub public_base: String,
    pub account_sid: String,
    pub auth_token: String,
    pub from_number: String,
    pub forwarded_from: String,
    pub max_duration_secs: u64,
    pub telegram_alias: String,
    pub telegram_peer_group: String,
    pub telegram_bot_username: String,
    pub openai_key_path: String,
    /// Optional owner-only private channel for inbound voicemail deliveries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voicemail: Option<VoicemailConfig>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoicemailConfig {
    pub telegram_alias: String,
    pub bot_username: String,
    pub channel_id: String,
}

pub struct Settings {
    pub enabled: bool,
    pub port: u16,
    pub public_base: String,
    pub account_sid: String,
    pub auth_token: String,
    pub from_number: String,
    pub forwarded_from: String,
    pub max_duration_secs: u64,
    pub telegram_token: String,
    pub telegram_chat_id: String,
    pub telegram_owner_id: String,
    pub telegram_bot_username: String,
    pub recording_dir: PathBuf,
    pub api_key: String,
    pub instructions: String,
    pub config_dir: PathBuf,
}

pub fn native_dir(root: &Path) -> SafeResult<PathBuf> {
    check(
        root.file_name().is_some_and(|v| v == "phone"),
        "invalid_extension_root",
    )?;
    let extensions = root.parent().ok_or("invalid_extension_root")?;
    check(
        extensions.file_name().is_some_and(|v| v == "extensions"),
        "invalid_extension_parent",
    )?;
    Ok(extensions.parent().ok_or("invalid_config_root")?.to_owned())
}

fn at<'a>(value: &'a toml::Value, path: &str) -> Option<&'a toml::Value> {
    path.split('.').try_fold(value, |v, k| v.get(k))
}

pub fn load(root: &Path) -> SafeResult<Settings> {
    load_delivery(root, false)
}

pub fn load_voicemail(root: &Path) -> SafeResult<Settings> {
    load_delivery(root, true)
}

fn load_delivery(root: &Path, voicemail: bool) -> SafeResult<Settings> {
    private_dir(root)?;
    let config_dir = native_dir(root)?;
    private_dir(&config_dir)?;
    private_read(&config_dir.join(".secret_key"))?;
    let p: PhoneConfig = toml::from_str(&private_read(&root.join("phone.toml"))?)
        .map_err(|_| "phone_config_invalid")?;
    let native: toml::Value = toml::from_str(&private_read(&config_dir.join("config.toml"))?)
        .map_err(|_| "native_config_invalid")?;
    let base = url::Url::parse(&p.public_base).map_err(|_| "public_url_invalid")?;
    check(
        base.scheme() == "https"
            && base.username().is_empty()
            && base.password().is_none()
            && base.host_str().is_some()
            && base.query().is_none()
            && base.fragment().is_none()
            && base.path() == "/"
            && base.port().is_none(),
        "unsafe_public_url",
    )?;
    check(
        p.port > 1024 && (1..=180).contains(&p.max_duration_secs),
        "invalid_call_limits",
    )?;
    check(
        crate::protocol::valid_sid(&p.account_sid, "AC")
            && e164(&p.from_number)
            && e164(&p.forwarded_from),
        "invalid_phone_identity",
    )?;
    check(
        p.auth_token.starts_with("enc2:"),
        "unencrypted_phone_credential",
    )?;
    let store = SecretStore::new(&config_dir, true);
    let auth_token = store
        .decrypt(&p.auth_token)
        .map_err(|_| "phone_credential_unavailable")?;
    check(
        (20..=1024).contains(&auth_token.len()) && !auth_token.chars().any(char::is_whitespace),
        "phone_credential_invalid",
    )?;
    let alias_path = format!("channels.telegram.{}", p.telegram_alias);
    let telegram = at(&native, &alias_path).ok_or("telegram_alias_missing")?;
    check(
        telegram.get("enabled").and_then(toml::Value::as_bool) == Some(true),
        "telegram_disabled",
    )?;
    let token = telegram
        .get("bot_token")
        .and_then(toml::Value::as_str)
        .ok_or("telegram_credential_missing")?;
    check(
        token.starts_with("enc2:"),
        "unencrypted_telegram_credential",
    )?;
    let group = at(&native, &format!("peer_groups.{}", p.telegram_peer_group))
        .ok_or("owner_group_missing")?;
    let peers = group
        .get("external_peers")
        .and_then(toml::Value::as_array)
        .ok_or("owner_policy_missing")?;
    check(peers.len() == 1, "expected_single_owner")?;
    let owner = peers[0].as_str().ok_or("owner_id_invalid")?;
    check(
        owner.parse::<u64>().is_ok_and(|id| id > 0),
        "owner_id_invalid",
    )?;
    let channel = group
        .get("channel")
        .and_then(toml::Value::as_str)
        .ok_or("owner_scope_missing")?;
    check(
        channel == format!("telegram.{}", p.telegram_alias),
        "owner_scope_invalid",
    )?;
    let key = at(&native, &p.openai_key_path)
        .and_then(toml::Value::as_str)
        .ok_or("voice_key_missing")?;
    check(key.starts_with("enc2:"), "unencrypted_voice_key")?;
    let instructions = private_read(&root.join("screening.md"))?;
    check(
        !instructions.is_empty() && instructions.len() < 64_000,
        "screening_policy_invalid",
    )?;
    let mut telegram_token = store
        .decrypt(token)
        .map_err(|_| "telegram_credential_unavailable")?;
    let api_key = store.decrypt(key).map_err(|_| "voice_key_unavailable")?;
    check(
        !telegram_token.is_empty() && !api_key.is_empty(),
        "empty_native_credential",
    )?;
    let mut telegram_chat_id = owner.to_owned();
    let mut telegram_bot_username = p.telegram_bot_username;
    if let Some(destination) = p.voicemail.filter(|_| voicemail) {
        check(
            valid_private_channel_id(&destination.channel_id),
            "voicemail_channel_invalid",
        )?;
        check(
            !destination.bot_username.is_empty(),
            "voicemail_bot_invalid",
        )?;
        let channel = at(
            &native,
            &format!("channels.telegram.{}", destination.telegram_alias),
        )
        .ok_or("voicemail_telegram_alias_missing")?;
        check(
            channel.get("enabled").and_then(toml::Value::as_bool) == Some(true),
            "voicemail_telegram_disabled",
        )?;
        let token = channel
            .get("bot_token")
            .and_then(toml::Value::as_str)
            .ok_or("voicemail_credential_missing")?;
        check(
            token.starts_with("enc2:"),
            "unencrypted_voicemail_credential",
        )?;
        telegram_token = store
            .decrypt(token)
            .map_err(|_| "voicemail_credential_unavailable")?;
        check(!telegram_token.is_empty(), "voicemail_credential_empty")?;
        telegram_chat_id = destination.channel_id;
        telegram_bot_username = destination.bot_username;
    }
    Ok(Settings {
        enabled: p.enabled,
        port: p.port,
        public_base: p.public_base.trim_end_matches('/').to_owned(),
        account_sid: p.account_sid,
        auth_token,
        from_number: p.from_number,
        forwarded_from: p.forwarded_from,
        max_duration_secs: p.max_duration_secs,
        telegram_token,
        telegram_chat_id,
        telegram_owner_id: owner.to_owned(),
        telegram_bot_username,
        recording_dir: root.join("recordings"),
        api_key,
        instructions,
        config_dir,
    })
}

pub fn valid_private_channel_id(value: &str) -> bool {
    value.starts_with("-100")
        && value.len() > 4
        && value
            .parse::<i64>()
            .is_ok_and(|id| id < 0 && id.to_string() == value)
}

pub fn delivery_chat_matches(chat: &serde_json::Value, expected: &str) -> bool {
    let Some(id) = chat["id"].as_i64() else {
        return false;
    };
    if id.to_string() != expected {
        return false;
    }
    if id > 0 {
        return chat["type"] == "private";
    }
    valid_private_channel_id(expected)
        && chat["type"] == "channel"
        && chat["username"].as_str().is_none_or(str::is_empty)
        && chat["active_usernames"]
            .as_array()
            .is_none_or(Vec::is_empty)
}

pub fn e164(s: &str) -> bool {
    let b = s.as_bytes();
    (3..=16).contains(&b.len())
        && b[0] == b'+'
        && (b'1'..=b'9').contains(&b[1])
        && b[2..].iter().all(u8::is_ascii_digit)
}

pub fn open_db(root: &Path) -> SafeResult<Connection> {
    private_dir(root)?;
    let path = root.join("phone.sqlite");
    if path.exists() {
        let m = fs::symlink_metadata(&path).map_err(|_| "database_metadata_failed")?;
        check(
            m.is_file()
                && !m.file_type().is_symlink()
                && m.uid() == unsafe { libc::geteuid() }
                && m.mode() & 0o077 == 0,
            "unsafe_database",
        )?;
    } else {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|_| "database_create_failed")?;
    }
    let c = Connection::open(path).map_err(|_| "database_open_failed")?;
    c.busy_timeout(Duration::from_secs(5))
        .map_err(|_| "database_timeout_failed")?;
    c.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;
        CREATE TABLE IF NOT EXISTS calls (
            call_sid TEXT PRIMARY KEY, account_sid TEXT NOT NULL, from_candidate TEXT NOT NULL,
            consent INTEGER, consent_token TEXT NOT NULL UNIQUE, media_token TEXT UNIQUE,
            created_ms INTEGER NOT NULL, phase TEXT NOT NULL, transcript TEXT, outcome TEXT,
            summary_status TEXT NOT NULL DEFAULT 'pending', summary_text TEXT, summary_message_id INTEGER
        );").map_err(|_| "database_initialize_failed")?;
    Ok(c)
}
