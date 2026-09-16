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

const MAX_PRIVATE_READ: u64 = 2 * 1024 * 1024;

pub fn private_read(path: &Path) -> SafeResult<String> {
    let mut f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "private_read_open_failed")?;
    let m = f.metadata().map_err(|_| "private_read_metadata_failed")?;
    check(
        m.is_file()
            && m.uid() == unsafe { libc::geteuid() }
            && m.mode() & 0o077 == 0
            && m.len() <= MAX_PRIVATE_READ,
        "unsafe_private_file",
    )?;
    let mut value = String::new();
    (&mut f)
        .take(MAX_PRIVATE_READ + 1)
        .read_to_string(&mut value)
        .map_err(|_| "private_read_failed")?;
    check(
        value.len() as u64 <= MAX_PRIVATE_READ,
        "private_read_too_large",
    )?;
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
    /// Canonical admission policy for new inbound calls; legacy installs keep keypad consent.
    #[serde(default)]
    pub recording_consent: RecordingConsentMode,
    pub telegram_alias: String,
    pub telegram_peer_group: String,
    pub telegram_bot_username: String,
    pub openai_key_path: String,
    /// Optional owner-only private channel for inbound voicemail deliveries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub voicemail: Option<VoicemailConfig>,
    /// Owner-private opt-in; resolved again before every scheduling attempt/write.
    #[serde(default)]
    pub tentative_rescheduling: bool,
    /// Which engine bridges call audio. Absent means the integrated Realtime session.
    #[serde(default, skip_serializing_if = "VoiceConfig::is_default")]
    pub voice: VoiceConfig,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VoiceEngine {
    /// One integrated OpenAI Realtime session (speech in, model, speech out).
    #[default]
    Realtime,
    /// Separate speech-to-text, chat model, and text-to-speech stages.
    Cascade,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoiceConfig {
    #[serde(default)]
    pub engine: VoiceEngine,
    /// Kept when the engine is switched back so the fallback is one line to undo.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cascade: Option<CascadeConfig>,
}

impl VoiceConfig {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Endpoints for the cascade. Speech recognition and synthesis speak the common
/// OpenAI-compatible HTTP shapes (`/v1/audio/transcriptions`, `/v1/audio/speech`,
/// `/v1/chat/completions`), so a local Kokoro or Whisper server, a local model
/// server, or OpenAI itself can fill any stage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CascadeConfig {
    pub stt_url: String,
    #[serde(default = "default_stt_model")]
    pub stt_model: String,
    /// Optional ISO-639-1 hint passed to the recognizer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stt_language: Option<String>,
    pub llm_url: String,
    pub llm_model: String,
    pub tts_url: String,
    #[serde(default = "default_tts_model")]
    pub tts_model: String,
    #[serde(default = "default_tts_voice")]
    pub tts_voice: String,
}

fn default_stt_model() -> String {
    "gpt-transcribe".into()
}

fn default_tts_model() -> String {
    "kokoro".into()
}

fn default_tts_voice() -> String {
    "af_heart".into()
}

/// Endpoints must be `https`, or plain `http` to this machine. The OpenAI
/// credential is only ever attached to `https://api.openai.com` (see cascade).
pub fn validate_endpoint(value: &str) -> SafeResult<url::Url> {
    let url = url::Url::parse(value).map_err(|_| "cascade_endpoint_invalid")?;
    let loopback = match url.host() {
        Some(url::Host::Domain(name)) => name == "localhost",
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => false,
    };
    check(
        (url.scheme() == "https" || (url.scheme() == "http" && loopback))
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && value.len() <= 512,
        "cascade_endpoint_unsafe",
    )?;
    Ok(url)
}

fn valid_model_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

impl CascadeConfig {
    pub fn validate(&self) -> SafeResult<()> {
        validate_endpoint(&self.stt_url)?;
        validate_endpoint(&self.llm_url)?;
        validate_endpoint(&self.tts_url)?;
        check(
            valid_model_name(&self.stt_model)
                && valid_model_name(&self.llm_model)
                && valid_model_name(&self.tts_model)
                && valid_model_name(&self.tts_voice)
                && self.stt_language.as_deref().is_none_or(|l| {
                    (2..=8).contains(&l.len())
                        && l.chars().all(|c| c.is_ascii_alphabetic() || c == '-')
                }),
            "cascade_model_invalid",
        )
    }
}

impl VoiceConfig {
    fn validate(&self) -> SafeResult<()> {
        if let Some(cascade) = &self.cascade {
            cascade.validate()?;
        }
        check(
            self.engine != VoiceEngine::Cascade || self.cascade.is_some(),
            "cascade_config_missing",
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingConsentMode {
    #[default]
    Explicit,
    Notice,
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
    pub recording_consent: RecordingConsentMode,
    pub telegram_token: String,
    pub telegram_chat_id: String,
    pub telegram_owner_id: String,
    pub telegram_bot_username: String,
    pub recording_dir: PathBuf,
    pub api_key: String,
    pub instructions: String,
    pub config_dir: PathBuf,
    pub voice: VoiceConfig,
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
    p.voice.validate()?;
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
        recording_consent: p.recording_consent,
        telegram_token,
        telegram_chat_id,
        telegram_owner_id: owner.to_owned(),
        telegram_bot_username,
        recording_dir: root.join("recordings"),
        api_key,
        instructions,
        config_dir,
        voice: p.voice,
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
    open_db_with_timeout(root, Duration::from_secs(5))
}

pub fn open_db_with_timeout(root: &Path, busy_timeout: Duration) -> SafeResult<Connection> {
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
    c.busy_timeout(busy_timeout)
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

/// Canonical feature policy; never retain it in a long-lived call handle.
pub fn tentative_rescheduling_enabled(root: &Path) -> SafeResult<bool> {
    let policy: PhoneConfig = toml::from_str(&private_read(&root.join("phone.toml"))?)
        .map_err(|_| "phone_config_invalid")?;
    Ok(policy.enabled && policy.tentative_rescheduling)
}

#[cfg(test)]
mod bounded_private_io_tests {
    use super::*;
    use std::{ffi::CString, time::Instant};

    #[test]
    fn fifo_is_rejected_before_read_and_oversize_private_files_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("policy-fifo");
        let encoded = CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(encoded.as_ptr(), 0o600) }, 0);
        let started = Instant::now();
        assert!(private_read(&fifo).is_err());
        assert!(started.elapsed() < Duration::from_secs(1));
        let large = dir.path().join("large-policy");
        let f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&large)
            .unwrap();
        f.set_len(MAX_PRIVATE_READ + 1).unwrap();
        assert!(private_read(&large).is_err());
    }

    #[test]
    fn short_database_budget_applies_to_initial_pragmas() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("private");
        private_dir(&root).unwrap();
        let db = open_db(&root).unwrap();
        db.execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE;")
            .unwrap();
        let started = Instant::now();
        assert!(open_db_with_timeout(&root, Duration::from_millis(40)).is_err());
        assert!(started.elapsed() < Duration::from_millis(600));
        db.execute_batch("ROLLBACK;").unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CASCADE: &str = r#"
engine = "cascade"
[cascade]
stt_url = "http://127.0.0.1:8000/v1/audio/transcriptions"
llm_url = "https://api.openai.com/v1/chat/completions"
llm_model = "chat-model"
tts_url = "http://127.0.0.1:8880/v1/audio/speech"
"#;

    #[test]
    fn absent_voice_settings_keep_the_realtime_engine_and_stay_absent_on_write() {
        let voice = VoiceConfig::default();
        assert_eq!(voice.engine, VoiceEngine::Realtime);
        assert!(voice.is_default());
        assert!(voice.validate().is_ok());
        let parsed: VoiceConfig = toml::from_str("").unwrap();
        assert!(parsed.is_default());
    }

    #[test]
    fn cascade_settings_parse_with_documented_defaults() {
        let voice: VoiceConfig = toml::from_str(CASCADE).unwrap();
        assert_eq!(voice.engine, VoiceEngine::Cascade);
        let cascade = voice.cascade.as_ref().unwrap();
        assert_eq!(cascade.tts_model, "kokoro");
        assert_eq!(cascade.tts_voice, "af_heart");
        assert_eq!(cascade.stt_model, "gpt-transcribe");
        assert!(voice.validate().is_ok());
        assert!(!voice.is_default());
        let round_trip: VoiceConfig = toml::from_str(&toml::to_string(&voice).unwrap()).unwrap();
        assert_eq!(round_trip, voice);
    }

    #[test]
    fn switching_back_to_realtime_keeps_the_cascade_section_valid_and_dormant() {
        let mut voice: VoiceConfig = toml::from_str(CASCADE).unwrap();
        voice.engine = VoiceEngine::Realtime;
        assert!(voice.validate().is_ok());
        assert!(!voice.is_default());
    }

    #[test]
    fn invalid_cascade_settings_are_rejected() {
        let missing: VoiceConfig = toml::from_str("engine = \"cascade\"").unwrap();
        assert_eq!(missing.validate(), Err("cascade_config_missing"));
        assert!(toml::from_str::<VoiceConfig>("engine = \"other\"").is_err());
        assert!(toml::from_str::<VoiceConfig>("surprise = 1").is_err());
        assert!(toml::from_str::<VoiceConfig>("[cascade]\nstt_url = \"x\"").is_err());

        for (from, to) in [
            (
                "http://127.0.0.1:8880/v1/audio/speech",
                "http://tts.example.com/v1/audio/speech",
            ),
            ("llm_model = \"chat-model\"", "llm_model = \"\""),
            ("llm_model = \"chat-model\"", "llm_model = \"bad\\nmodel\""),
        ] {
            let text = CASCADE.replace(from, to);
            let voice: VoiceConfig = toml::from_str(&text).unwrap();
            assert!(voice.validate().is_err(), "{to}");
        }
        let language = format!("{CASCADE}stt_language = \"english is fine!\"\n");
        let voice: VoiceConfig = toml::from_str(&language).unwrap();
        assert_eq!(voice.validate(), Err("cascade_model_invalid"));
    }
}
