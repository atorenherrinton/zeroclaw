//! One-time migration verification. No model, logger, configuration writes, or page output.
//! Takes an existing private install directory and private browser overlay JSON.

use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use zeroclaw_api::tool::{Tool, ToolResult};
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::schema::{BrowserConfig, Config};
use zeroclaw_tools::browser::{BrowserTool, ComputerUseConfig};
use zeroclaw_tools::wrappers::RateLimitedTool;

type SafeResult<T> = Result<T, &'static str>;
const AGENT: &str = "automation_modmail";
const INBOX: &str = "https://www.reddit.com/notifications";
const ALLOWED_HOSTS: &[&str] = &[
    "reddit.com",
    "www.reddit.com",
    "old.reddit.com",
    "mod.reddit.com",
    "modmail.reddit.com",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BrowserOverlay {
    browser: BrowserConfig,
}

// Open the existing owner-private regular file without following a final symlink.
// Never display file contents, file paths, or deserialization errors.
fn private_read(path: &Path) -> SafeResult<String> {
    let mut file: File = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .map_err(|_| "private_file_open_failed")?;
    let meta = file
        .metadata()
        .map_err(|_| "private_file_metadata_failed")?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o777 != 0o600
        || meta.nlink() != 1
        || meta.len() > 2 * 1024 * 1024
    {
        return Err("private_file_boundary_failed");
    }
    let mut text = String::new();
    (&mut file)
        .take(2 * 1024 * 1024 + 1)
        .read_to_string(&mut text)
        .map_err(|_| "private_file_read_failed")?;
    if text.len() > 2 * 1024 * 1024 {
        return Err("private_file_too_large");
    }
    Ok(text)
}

fn load_config(install: &Path, overlay_path: &Path) -> SafeResult<Config> {
    if !install.is_absolute() || !overlay_path.is_absolute() {
        return Err("absolute_paths_required");
    }
    let config_path = install.join("config.toml");
    let text = private_read(&config_path)?;
    // Do not call load_or_init: that path may migrate/create native data directories.
    let mut config: Config = toml::from_str(&text).map_err(|_| "native_config_invalid")?;
    let overlay: BrowserOverlay = serde_json::from_str(&private_read(overlay_path)?)
        .map_err(|_| "browser_overlay_invalid")?;
    config.config_path = config_path;
    config.data_dir = install.join("data");
    config.browser = overlay.browser;
    validate_browser(&config.browser)?;
    if !config.agents.contains_key(AGENT) {
        return Err("monitor_agent_missing");
    }
    // SecurityPolicy::for_agent calls create_dir_all; require its target to exist so
    // verification never creates a new workspace as a side effect.
    if !config.agent_workspace_dir(AGENT).is_dir() {
        return Err("monitor_workspace_missing");
    }
    Ok(config)
}

fn validate_browser(browser: &BrowserConfig) -> SafeResult<()> {
    let endpoint = reqwest::Url::parse(&browser.native_webdriver_url)
        .map_err(|_| "adapter_endpoint_invalid")?;
    let secret_path = endpoint.path().trim_matches('/');
    if !browser.enabled
        || browser.backend != "rust_native"
        || !browser.allowed_private_hosts.is_empty()
        || browser.allowed_domains.is_empty()
        || browser
            .allowed_domains
            .iter()
            .any(|host| !ALLOWED_HOSTS.contains(&host.as_str()))
        || endpoint.scheme() != "http"
        || endpoint.host_str() != Some("127.0.0.1")
        || endpoint.port() != Some(9516)
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
        || !endpoint.path().ends_with('/')
        || secret_path.len() != 64
        || !secret_path.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err("browser_boundary_failed");
    }
    Ok(())
}

fn make_tool(config: &Config) -> SafeResult<RateLimitedTool<BrowserTool>> {
    let security =
        Arc::new(SecurityPolicy::for_agent(config, AGENT).map_err(|_| "monitor_policy_invalid")?);
    if !security.can_act() || !security.is_tool_allowed("browser") {
        return Err("monitor_browser_not_allowed");
    }
    let browser = &config.browser;
    let tool = BrowserTool::new_with_backend(
        security.clone(),
        browser.allowed_domains.clone(),
        browser.session_name.clone(),
        browser.backend.clone(),
        browser.headed,
        browser.native_headless,
        browser.native_webdriver_url.clone(),
        browser.native_chrome_path.clone(),
        ComputerUseConfig {
            endpoint: browser.computer_use.endpoint.clone(),
            api_key: browser.computer_use.api_key.clone(),
            timeout_ms: browser.computer_use.timeout_ms,
            allow_remote_endpoint: browser.computer_use.allow_remote_endpoint,
            window_allowlist: browser.computer_use.window_allowlist.clone(),
            max_coordinate_x: browser.computer_use.max_coordinate_x,
            max_coordinate_y: browser.computer_use.max_coordinate_y,
        },
        browser.allowed_private_hosts.clone(),
    )
    .map_err(|_| "native_browser_constructor_failed")?;
    Ok(RateLimitedTool::new(tool, security))
}

fn native_data(result: ToolResult, action: &str) -> SafeResult<Value> {
    if !result.success {
        return Err("native_action_rejected");
    }
    let data: Value =
        serde_json::from_str(result.output.as_str()).map_err(|_| "native_result_invalid")?;
    if data.get("backend").and_then(Value::as_str) != Some("rust_native")
        || data.get("action").and_then(Value::as_str) != Some(action)
    {
        return Err("native_result_backend_mismatch");
    }
    Ok(data)
}

fn native_error_code(error: anyhow::Error) -> &'static str {
    // Classify only fixed library/adapter phrases. Never print the error chain,
    // which may contain the private endpoint credential or a redirected URL.
    let chain = error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" ");
    if chain.contains("Read-only browser policy rejected") {
        "native_adapter_policy_rejected"
    } else if chain.contains("Dedicated adapter is busy") {
        "native_adapter_busy"
    } else if chain.contains("Dedicated browser driver unavailable") {
        "native_driver_unavailable"
    } else if chain.contains("No matching adapter session") {
        "native_adapter_session_invalid"
    } else if chain.contains("Failed to connect to WebDriver") {
        "native_webdriver_session_failed"
    } else if chain.contains("Failed to read current URL") {
        "native_current_url_failed"
    } else if chain.contains("Failed to open URL") {
        "native_navigation_failed"
    } else {
        "native_open_failed"
    }
}

fn page_flags(body: &str) -> Value {
    // Hints only: user-controlled messages could contain these words. Absence of a
    // sign-in/challenge hint is not proof of account authentication or identity.
    let lower = body.to_lowercase();
    let line_is = |candidate: &str| lower.lines().any(|line| line.trim() == candidate);
    let signed_out_hint = ["log in", "log into reddit", "sign in", "sign into reddit"]
        .iter()
        .any(|candidate| line_is(candidate));
    let challenge_hint = [
        "verify you're human",
        "verify you are human",
        "you've been blocked by network security",
        "please complete the captcha",
        "checking your browser",
    ]
    .iter()
    .any(|candidate| lower.contains(candidate));
    let authenticated_ui_hint = ["log out", "logout", "sign out"]
        .iter()
        .any(|candidate| line_is(candidate));
    let inbox_ui_hint = [
        "inbox",
        "messages",
        "unread",
        "sent",
        "notifications",
        "chats",
    ]
    .iter()
    .filter(|candidate| line_is(candidate))
    .count()
        >= 2;
    json!({
        "status": "native_open_and_get_text_succeeded",
        "native_open_succeeded": true,
        "native_get_text_succeeded": true,
        "body_nonempty": !body.trim().is_empty(),
        "signed_out_hint": signed_out_hint,
        "challenge_hint": challenge_hint,
        "authenticated_ui_hint": authenticated_ui_hint,
        "inbox_ui_hint": inbox_ui_hint,
        "account_identity_verified": false
    })
}

async fn verify(install: PathBuf, overlay_path: PathBuf) -> SafeResult<Value> {
    let config = load_config(&install, &overlay_path)?;
    let tool = make_tool(&config)?;
    // These are the only two browser actions issued by this one-time helper.
    let opened = tokio::time::timeout(
        Duration::from_secs(60),
        tool.execute(json!({"action": "open", "url": INBOX, "approved": true})),
    )
    .await
    .map_err(|_| "native_open_timeout")?
    .map_err(native_error_code)?;
    let opened = native_data(opened, "open").map_err(|_| "native_open_rejected")?;
    if opened
        .get("url")
        .and_then(Value::as_str)
        .map(|url| url.trim_end_matches('/'))
        != Some(INBOX.trim_end_matches('/'))
    {
        return Err("native_inbox_navigation_not_confirmed");
    }
    let text = tokio::time::timeout(
        Duration::from_secs(45),
        tool.execute(json!({"action": "get_text", "selector": "body", "approved": true})),
    )
    .await
    .map_err(|_| "native_get_text_timeout")?
    .map_err(|_| "native_get_text_failed")?;
    let text = native_data(text, "get_text").map_err(|_| "native_get_text_rejected")?;
    let body = text
        .get("text")
        .and_then(Value::as_str)
        .ok_or("native_body_missing")?;
    Ok(page_flags(body))
}

async fn classify_target_metadata(install: PathBuf, overlay_path: PathBuf) -> SafeResult<Value> {
    let _config = load_config(&install, &overlay_path)?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|_| "metadata_client_failed")?;
    // Parent-authorized read-only diagnosis against the dedicated native profile.
    // No CDP methods, sessions, activation, titles, page data, or navigation.
    let mut response = client
        .get("http://127.0.0.1:18801/json/list")
        .send()
        .await
        .map_err(|_| "metadata_request_failed")?;
    if !response.status().is_success() {
        return Err("metadata_request_rejected");
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "metadata_response_failed")?
    {
        if bytes.len().saturating_add(chunk.len()) > 128 * 1024 {
            return Err("metadata_response_too_large");
        }
        bytes.extend_from_slice(&chunk);
    }
    let targets: Value = serde_json::from_slice(&bytes).map_err(|_| "metadata_invalid")?;
    let targets = targets.as_array().ok_or("metadata_invalid")?;
    let mut counts = std::collections::BTreeMap::<&'static str, usize>::new();
    for target in targets.iter().filter(|target| target["type"] == "page") {
        let raw = target["url"].as_str().unwrap_or_default();
        let category = if raw == "about:blank" {
            "about_blank"
        } else if raw.starts_with("chrome://newtab") || raw.starts_with("chrome://new-tab-page") {
            "chrome_new_tab"
        } else if let Ok(url) = reqwest::Url::parse(raw) {
            if url
                .host_str()
                .is_some_and(|host| ALLOWED_HOSTS.contains(&host))
            {
                let path = url.path().trim_end_matches('/');
                if path.contains("login") || path.contains("register") {
                    "reddit_login"
                } else if path.contains("challenge") || path.contains("captcha") {
                    "reddit_challenge"
                } else if url.query().is_none()
                    && url.fragment().is_none()
                    && [
                        "/message/inbox",
                        "/message/messages",
                        "/message/unread",
                        "/mail/all",
                        "/mail/inbox",
                        "/mail/unread",
                        "/notifications",
                    ]
                    .contains(&path)
                {
                    "approved_inbox"
                } else {
                    "other_reddit_route"
                }
            } else {
                "non_reddit"
            }
        } else {
            "invalid_url_metadata"
        };
        *counts.entry(category).or_default() += 1;
    }
    Ok(json!({"status": "dedicated_target_metadata_classified", "page_categories": counts}))
}

#[tokio::main]
async fn main() {
    // No tracing/log subscriber is initialized. Panic and normal failure output
    // are static, so even native errors containing the adapter token stay private.
    std::panic::set_hook(Box::new(|_| eprintln!("native_verification_panicked")));
    // Match the real CLI's process-level TLS initialization. Both transitive TLS
    // provider features may be present, so rustls cannot safely choose one itself.
    #[cfg(feature = "agent-runtime")]
    if rustls::crypto::ring::default_provider()
        .install_default()
        .is_err()
    {
        eprintln!("native_crypto_provider_initialization_failed");
        std::process::exit(2);
    }
    #[cfg(not(feature = "agent-runtime"))]
    {
        eprintln!("native_verification_requires_agent_runtime_feature");
        std::process::exit(2);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 2 && !(args.len() == 3 && args[2] == "--classify-targets") {
        eprintln!("native_verification_arguments_invalid");
        std::process::exit(2);
    }
    let result = if args.len() == 3 {
        classify_target_metadata(PathBuf::from(&args[0]), PathBuf::from(&args[1])).await
    } else {
        verify(PathBuf::from(&args[0]), PathBuf::from(&args[1])).await
    };
    match result {
        Ok(flags) => println!("{flags}"),
        Err(code) => {
            println!("{}", json!({"status": code}));
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_expose_only_static_hints_not_page_contents_or_identity() {
        let flags = page_flags("inbox\nmessages\nunread\nlog out\nprivate synthetic message");
        assert_eq!(flags["inbox_ui_hint"], true);
        assert_eq!(flags["authenticated_ui_hint"], true);
        assert_eq!(flags["signed_out_hint"], false);
        assert_eq!(flags["account_identity_verified"], false);
        assert!(!flags.to_string().contains("private synthetic message"));
        let signed_out = page_flags("Log in\nVerify you're human");
        assert_eq!(signed_out["signed_out_hint"], true);
        assert_eq!(signed_out["challenge_hint"], true);
        assert_eq!(signed_out["authenticated_ui_hint"], false);
    }

    #[test]
    fn overlay_endpoint_cannot_bypass_restricted_adapter() {
        let mut browser = BrowserConfig {
            backend: "rust_native".into(),
            allowed_domains: vec!["www.reddit.com".into()],
            native_webdriver_url: format!("http://127.0.0.1:9516/{}/", "a".repeat(64)),
            ..BrowserConfig::default()
        };
        assert!(validate_browser(&browser).is_ok());
        browser.native_webdriver_url = "http://127.0.0.1:9515/".into();
        assert!(validate_browser(&browser).is_err());
        browser.native_webdriver_url = format!("http://127.0.0.1:9516/{}/", "a".repeat(64));
        browser.allowed_private_hosts.push("*".into());
        assert!(validate_browser(&browser).is_err());
    }
}
