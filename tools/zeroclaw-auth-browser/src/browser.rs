use crate::{policy::validate_url, proxy};
use anyhow::{Context, Result, bail};
use reqwest::{Client, Method};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    os::{
        fd::AsRawFd,
        unix::{
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
            process::CommandExt as _,
        },
    },
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    task::JoinHandle,
};

const CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
// The live DOM is authoritative; share the Safari connector's fixed traversal
// and scoped selectors instead of maintaining another component resolver.
const DOM_COMMON: &str = include_str!("dom/common.js");
const DOM_READY: &str = include_str!("dom/ready.js");
const CLICK_POINT: &str = include_str!("click-point.js");
const LOGIN_SCRIPT: &str = include_str!("login.js");
const FILL_SCRIPT: &str = r#"
const element = target(args.selector);
const input = element instanceof HTMLInputElement;
const textarea = element instanceof HTMLTextAreaElement;
const types = ['text','email','password','search','tel','url','number','date','datetime-local','month','time','week','color','range'];
if (input && !types.includes(element.type)) throw new Error('Unsupported fill input');
if (!input && !textarea && !element.isContentEditable) throw new Error('Target is not an editable field');
if (element.readOnly) throw new Error('Field is read-only');
// Native DOM setters cannot enter WebDriver's file-upload path even if a page
// changes the target type. A nonempty value on a file input throws instead.
if (input || textarea) {
  const prototype = input ? HTMLInputElement.prototype : HTMLTextAreaElement.prototype;
  const setter = Object.getOwnPropertyDescriptor(prototype, 'value').set;
  setter.call(element, args.text);
} else { element.textContent = args.text; }
element.dispatchEvent(new Event('input',{bubbles:true,composed:true}));
element.dispatchEvent(new Event('change',{bubbles:true,composed:true}));
return true;
"#;
const SUBMIT_GUARD: &str = r#"
// A site's handlers still control its own page. Before trusted clicks/keys,
// reject ordinary forms that would send a filled password to another origin,
// including submit-button overrides and forms changed since credential fill.
for (const input of deepQuery('input[type="password"]')) {
  if (!input.value || !input.form) continue;
  const form = input.form;
  const checkAction = raw => {
    const action = new URL(raw || location.href, location.href);
    if (action.protocol !== 'https:' || action.origin !== location.origin || action.username || action.password) {
      throw new Error('Credential form submission has another origin');
    }
  };
  checkAction(form.action);
  for (const submitter of deepQuery('button[formaction],input[formaction]')) {
    if (submitter.form === form) checkAction(submitter.formAction);
  }
}
"#;
const FOCUS_SCRIPT: &str = r#"
const element = target(args.selector);
if (element instanceof HTMLInputElement && element.type === 'file') throw new Error('File controls are unsupported');
element.focus();
return true;
"#;
const SUMMARY_SCRIPT: &str = r#"
const controls = deepQuery(controlsSelector).filter(visible);
const text = composedText(document.body,20001);
return {url:location.href,title:document.title,text,text_truncated:text.length>20000,
 totalControls:controls.length, offset:args.offset,
 controls:controls.slice(args.offset,args.offset+12).map(e=>{
 const control = {tag:e.tagName.toLowerCase(),text:controlText(e),disabled:disabled(e),...(e.href?{href:e.href}:{})};
 try { control.selector = selectorFor(e); }
 catch (error) { control.selector = null; control.selector_error = String(error.message || error).slice(0,160); }
 return control;
})};
"#;

fn page_result(mut page: Value, offset: usize) -> Result<Value> {
    loop {
        let count = page["controls"].as_array().map_or(0, Vec::len);
        let total = page["totalControls"].as_u64().unwrap_or(0) as usize;
        page["next_offset"] = if offset + count < total {
            json!(offset + count)
        } else {
            Value::Null
        };
        page["untrusted"] = json!(true);
        let result = json!({"content":[{"type":"text","text":serde_json::to_string(&page)?}]});
        // Account for the MCP formatter and then the runtime's JSON encoding.
        // Fits the 4 KiB read preview without losing selectors or pagination.
        if serde_json::to_string(&serde_json::to_string_pretty(&result)?)?.len() <= 3500 {
            return Ok(result);
        }
        if count > 1 {
            page["controls"]
                .as_array_mut()
                .context("Invalid controls")?
                .pop();
        } else if page["text"].as_str().is_some_and(|t| !t.is_empty()) {
            page["text"] = json!("");
            page["text_truncated"] = json!(true);
        } else {
            bail!("Page metadata exceeds the bounded response limit; no action should be repeated");
        }
    }
}

// Use the core's existing embedded-resource intake so PNG bytes become a
// workspace image attachment before source-result admission. Inline image data
// otherwise reaches the text budget as base64 and aborts ordinary screenshots.
fn screenshot_result(data: &str) -> Result<Value> {
    if data.len() > 3_000_000 {
        bail!("Screenshot exceeds the safe response limit");
    }
    Ok(json!({"content":[{"type":"resource","resource":{
        "uri":"zeroclaw://auth-browser/screenshot.png",
        "mimeType":"image/png","blob":data
    }}]}))
}

#[derive(Deserialize, Debug)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Browse {
    Open {
        url: String,
    },
    Read {
        #[serde(default)]
        offset: usize,
    },
    Screenshot,
    Scroll {
        direction: Direction,
    },
}
#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Up,
    Down,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Interact {
    Click { selector: String },
    Fill { selector: String, text: String },
    Press { selector: String, key: Key },
}
#[derive(Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Key {
    Enter,
    Tab,
    Escape,
    ArrowDown,
    ArrowUp,
}

impl Key {
    fn wire(&self) -> &'static str {
        match self {
            Self::Enter => "\u{e007}",
            Self::Tab => "\u{e004}",
            Self::Escape => "\u{e00c}",
            Self::ArrowDown => "\u{e015}",
            Self::ArrowUp => "\u{e013}",
        }
    }
}

pub struct Browser {
    http: Client,
    endpoint: String,
    session: Option<String>,
    driver: Child,
    driver_group: i32,
    watchdog: Child,
    proxy_task: JoinHandle<Result<()>>,
    // Held for the entire driver lifetime. A second helper must never share it.
    _profile_lock: File,
    credentials_marker: PathBuf,
    redaction_origins_path: PathBuf,
    redaction_origins: Mutex<Vec<String>>,
    redaction_primed: AtomicBool,
    screenshots_disabled: AtomicBool,
    secrets: Mutex<zeroize::Zeroizing<Vec<String>>>,
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.proxy_task.abort();
        // The driver was started in its own process group, which contains only
        // the dedicated Chrome it launched. Never target an existing user browser.
        if self.driver_group > 0 {
            unsafe {
                libc::kill(-self.driver_group, libc::SIGKILL);
            }
        }
        let _ = self.watchdog.start_kill();
    }
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != unsafe { libc::geteuid() }
    {
        bail!("Browser state must be an owned directory, not a symlink");
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn private_file(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .write(true)
        .read(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1
    {
        bail!("Browser state file must be an owned regular file");
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn lock_profile(path: &Path) -> Result<File> {
    let file = private_file(path)?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!(
            "The dedicated authenticated browser profile is already in use; close its existing session first"
        );
    }
    Ok(file)
}

fn read_redaction_origins(path: &Path, credentials_used: bool) -> Result<Vec<String>> {
    use std::io::Read as _;
    if !path.exists() && !credentials_used {
        return Ok(Vec::new());
    }
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .context("Persistent credential redaction metadata is unavailable")?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.nlink() != 1
        || metadata.len() > 2_000_000
    {
        bail!("Invalid persistent credential redaction metadata");
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let origins: Vec<String> = serde_json::from_str(&contents)
        .map_err(|_| anyhow::Error::msg("Invalid persistent credential redaction metadata"))?;
    if origins.len() > 20_000
        || origins.iter().any(|origin| {
            validate_url(origin).is_err()
                || url::Url::parse(origin)
                    .is_ok_and(|url| url.origin().ascii_serialization() != *origin)
        })
    {
        bail!("Invalid persistent credential redaction origins");
    }
    Ok(origins)
}

fn write_redaction_origins(path: &Path, origins: &[String]) -> Result<()> {
    use std::io::Write as _;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&temporary)
        .context("Cannot persist credential redaction origins")?;
    let result = (|| -> Result<()> {
        file.write_all(&serde_json::to_vec(origins)?)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result.context("Cannot persist credential redaction origins")
}

fn secret_variants(secret: &str) -> Vec<String> {
    let encoded: String = url::form_urlencoded::byte_serialize(secret.as_bytes()).collect();
    let escaped = serde_json::to_string(secret).unwrap_or_default();
    vec![
        secret.to_owned(),
        encoded.clone(),
        encoded.replace('+', "%20"),
        escaped
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(secret)
            .to_owned(),
        secret
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#39;"),
    ]
}

fn redact_value(value: &mut Value, secrets: &[String]) {
    match value {
        Value::String(text) => {
            for secret in secrets {
                *text = text.replace(secret, "[REDACTED]");
                // DOM collection itself is bounded. Also hide a secret prefix
                // at that boundary, including a password clipped mid-character.
                let max = secret.len().min(text.len());
                for n in (4..=max).rev() {
                    if secret.is_char_boundary(n) && text.ends_with(&secret[..n]) {
                        text.truncate(text.len() - n);
                        text.push_str("[REDACTED]");
                        break;
                    }
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                redact_value(value, secrets);
            }
        }
        Value::Object(values) => {
            let original = std::mem::take(values);
            for (mut key, mut value) in original {
                // Keys are not clipped by DOM collection. Replacing partial
                // prefixes here could corrupt fixed summary schema names.
                for secret in secrets {
                    key = key.replace(secret, "[REDACTED]");
                }
                redact_value(&mut value, secrets);
                values.insert(key, value);
            }
        }
        _ => {}
    }
}

impl Browser {
    pub async fn start() -> Result<Self> {
        let driver_path = std::env::current_exe()?
            .parent()
            .context("Executable has no parent")?
            .join("chromedriver");
        Self::start_with_driver(&driver_path).await
    }

    pub async fn start_with_driver(driver_path: &Path) -> Result<Self> {
        let home = std::env::var_os("HOME").context("Home directory is unavailable")?;
        let state = PathBuf::from(home).join(".zeroclaw/auth-browser");
        ensure_private_directory(&state)?;
        let profile = state.join("chrome-profile");
        ensure_private_directory(&profile)?;
        let profile_lock = lock_profile(&state.join("chrome-profile.lock"))?;
        let credentials_marker = state.join("credentials-used");
        let screenshots_disabled = fs::symlink_metadata(&credentials_marker).is_ok();
        let redaction_origins_path = state.join("redaction-origins.json");
        let redaction_origins =
            read_redaction_origins(&redaction_origins_path, screenshots_disabled)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let proxy_address = listener.local_addr()?;
        let port_listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = port_listener.local_addr()?.port();
        drop(port_listener);
        let mut command = Command::new(driver_path);
        command
            .args([
                format!("--port={port}"),
                "--allowed-ips=127.0.0.1".into(),
                "--log-level=OFF".into(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let driver = command
            .spawn()
            .context("Cannot start the dedicated ChromeDriver")?;
        let driver_group = driver
            .id()
            .context("Dedicated ChromeDriver has no process ID")? as i32;
        let mut watchdog_command = Command::new(std::env::current_exe()?);
        watchdog_command
            .args([
                "--watch-driver-group",
                &driver_group.to_string(),
                &std::process::id().to_string(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        watchdog_command.as_std_mut().process_group(0);
        let watchdog = watchdog_command
            .spawn()
            .context("Cannot start the browser lifecycle watchdog")?;
        let mut this = Self {
            http: Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(40))
                .build()?,
            endpoint: format!("http://127.0.0.1:{port}"),
            session: None,
            driver,
            driver_group,
            watchdog,
            proxy_task: zeroclaw_spawn::spawn!(proxy::serve(listener)),
            _profile_lock: profile_lock,
            credentials_marker,
            redaction_origins_path,
            redaction_origins: Mutex::new(redaction_origins),
            redaction_primed: AtomicBool::new(false),
            screenshots_disabled: AtomicBool::new(screenshots_disabled),
            secrets: Mutex::new(zeroize::Zeroizing::new(Vec::new())),
        };
        let mut ready = false;
        for _ in 0..50 {
            if this.driver.try_wait()?.is_some() {
                bail!("Dedicated ChromeDriver exited at startup");
            }
            if this
                .http
                .get(format!("{}/status", this.endpoint))
                .send()
                .await
                .is_ok()
            {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !ready {
            bail!("Dedicated ChromeDriver did not become ready");
        }
        let capabilities = json!({"capabilities":{"alwaysMatch":{
            "browserName":"chrome", "acceptInsecureCerts":false,
            "pageLoadStrategy":"eager", "unhandledPromptBehavior":"dismiss",
            "timeouts":{"implicit":0,"pageLoad":30000,"script":5000},
            "goog:chromeOptions":{"binary":CHROME,"args":[
                "--headless=new", "--window-size=1440,1000", "--disable-quic",
                format!("--user-data-dir={}", profile.display()),
                format!("--proxy-server=http://{proxy_address}"), "--proxy-bypass-list=<-loopback>",
                "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
                "--disable-background-networking", "--disable-extensions", "--no-first-run"
            ],"prefs":{"download_restrictions":3,"profile.default_content_setting_values.notifications":2,
                "credentials_enable_service":false,"profile.password_manager_enabled":false}}
        }}});
        let session = this.request(Method::POST, "/session", capabilities).await?;
        let sid = session["sessionId"]
            .as_str()
            .context("ChromeDriver did not return a session")?;
        if !sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            bail!("Invalid session ID");
        }
        this.session = Some(sid.into());
        this.call(
            Method::POST,
            "/goog/cdp/execute",
            json!({"cmd":"Browser.setDownloadBehavior","params":{"behavior":"deny"}}),
        )
        .await?;
        Ok(this)
    }

    async fn request(&self, method: Method, route: &str, body: Value) -> Result<Value> {
        let mut request = self
            .http
            .request(method.clone(), format!("{}{route}", self.endpoint));
        if method == Method::POST {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|_| anyhow::Error::msg("Browser connection failed"))?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|_| anyhow::Error::msg("Browser returned an invalid response"))?;
        if !status.is_success() || body["value"]["error"].is_string() {
            // Driver messages may quote script arguments, URLs, or a page's
            // exception. Never allow those strings to echo credentials.
            bail!("Browser navigation or interaction failed; read the page before retrying");
        }
        Ok(body["value"].clone())
    }

    async fn call(&self, method: Method, route: &str, body: Value) -> Result<Value> {
        if self.proxy_task.is_finished() {
            bail!("Public-network guard is unavailable; browser stopped.");
        }
        let sid = self.session.as_deref().context("No browser session")?;
        self.request(method, &format!("/session/{sid}{route}"), body)
            .await
    }

    pub async fn current_origin(&self) -> Result<String> {
        let url = self.call(Method::GET, "/url", Value::Null).await?;
        let url = validate_url(url.as_str().context("No current URL")?)?;
        Ok(url.origin().ascii_serialization())
    }

    async fn check_current(&self) -> Result<()> {
        self.current_origin().await?;
        Ok(())
    }

    // Main may prime this with vault secrets before observing a persistent
    // session. This method never sends secrets to the page or driver.
    pub fn remember_secret(&self, secret: &str) -> Result<()> {
        if secret.is_empty() {
            return Ok(());
        }
        let mut secrets = self
            .secrets
            .lock()
            .map_err(|_| anyhow::Error::msg("Credential redaction is unavailable"))?;
        for candidate in secret_variants(secret) {
            if !secrets.contains(&candidate) {
                secrets.push(candidate);
            }
        }
        secrets.sort_by_key(|value| std::cmp::Reverse(value.len()));
        Ok(())
    }

    fn remember_origin(&self, origin: &str) -> Result<()> {
        let mut origins = self
            .redaction_origins
            .lock()
            .map_err(|_| anyhow::Error::msg("Credential redaction is unavailable"))?;
        if origins.iter().any(|value| value == origin) {
            return Ok(());
        }
        if origins.len() >= 20_000 {
            bail!("Credential redaction origin limit reached");
        }
        let mut updated = origins.clone();
        updated.push(origin.to_owned());
        updated.sort();
        write_redaction_origins(&self.redaction_origins_path, &updated)?;
        *origins = updated;
        Ok(())
    }

    fn redact(&self, value: &mut Value) -> Result<()> {
        let secrets = self
            .secrets
            .lock()
            .map_err(|_| anyhow::Error::msg("Credential redaction is unavailable"))?;
        redact_value(value, &secrets);
        Ok(())
    }

    pub async fn login(
        &self,
        username: &str,
        password: &str,
        origin: &str,
        username_selector: Option<&str>,
        password_selector: Option<&str>,
        field: &str,
    ) -> Result<Value> {
        let validated = validate_url(origin)?;
        if validated.origin().ascii_serialization() != origin
            || self.current_origin().await? != origin
        {
            bail!("Login origin does not match the current HTTPS page");
        }
        if !matches!(field, "both" | "username" | "password") {
            bail!("Login field must be both, username, or password");
        }
        if username.len() > 8000
            || password.len() > 16384
            || (field != "password" && username.is_empty())
            || (field != "username" && password.is_empty())
        {
            bail!("Login credential is missing or exceeds the supported length");
        }
        for selector in [username_selector, password_selector].into_iter().flatten() {
            if selector.is_empty() || selector.len() > 2000 {
                bail!("Login selector must be 1-2000 characters");
            }
        }
        self.remember_secret(password)?;
        self.remember_origin(origin)?;
        // Screenshot masking cannot cover arbitrary page reflection reliably.
        // Persist this restriction before injection so it survives a crash.
        private_file(&self.credentials_marker)?.sync_all()?;
        self.screenshots_disabled.store(true, Ordering::SeqCst);
        let value = self.dom(LOGIN_SCRIPT, json!({
            "origin":origin,"username":username,"password":password,"field":field,
            "username_selector":username_selector,"password_selector":password_selector
        })).await.map_err(|_| anyhow::Error::msg(
            "Login fields could not be filled safely; read the page and verify the origin and unique field selectors before retrying"
        ))?;
        let username_filled = value["username_filled"]
            .as_bool()
            .context("Invalid login result")?;
        let password_filled = value["password_filled"]
            .as_bool()
            .context("Invalid login result")?;
        // Only fixed booleans leave the secret-bearing operation. The caller
        // decides whether to click the site's sign-in/continue control.
        Ok(json!({"username_filled":username_filled,"password_filled":password_filled}))
    }

    async fn execute_fixed(&self, script: &str) -> Result<Value> {
        self.call(
            Method::POST,
            "/execute/sync",
            json!({"script":script,"args":[]}),
        )
        .await
    }

    async fn dom(&self, body: &str, args: Value) -> Result<Value> {
        let script = format!(
            "try {{ const args = arguments[0]; {DOM_COMMON} {body} }} catch(error) {{ return {{__domError:true}}; }}"
        );
        let value = self
            .call(
                Method::POST,
                "/execute/sync",
                json!({"script":script,"args":[args]}),
            )
            .await?;
        if value["__domError"] == true {
            bail!("Browser DOM operation failed; read the page to verify the target");
        }
        Ok(value)
    }

    async fn summary(&self, offset: usize) -> Result<Value> {
        let origin = self.current_origin().await?;
        // Persisted pages can reflect a password from a previous process.
        // Prime redaction locally before inspecting the authenticated page;
        // these credentials are never sent to the DOM for redaction.
        let mut origins = vec![origin];
        if !self.redaction_primed.load(Ordering::SeqCst) {
            origins.extend(
                self.redaction_origins
                    .lock()
                    .map_err(|_| anyhow::Error::msg("Credential redaction is unavailable"))?
                    .iter()
                    .cloned(),
            );
        }
        origins.sort();
        origins.dedup();
        for origin in origins {
            let accounts = crate::vault::accounts_for_origin(&origin).map_err(|_| {
                anyhow::Error::msg("Cannot load credential redaction for this profile")
            })?;
            for account in accounts {
                let password = crate::vault::load(&account).map_err(|_| {
                    anyhow::Error::msg(
                        "Cannot read this page safely while credential redaction is unavailable",
                    )
                })?;
                self.remember_secret(&password)?;
            }
        }
        self.redaction_primed.store(true, Ordering::SeqCst);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        let mut previous = None;
        let readiness = loop {
            let ready_body = format!(
                "if (!composedText(document.body,1200) && !deepQuery(controlsSelector).some(visible)) return JSON.stringify({{ready:false,reasons:['page_empty'],fingerprint:'empty'}}); {DOM_READY}"
            );
            let raw = self.dom(&ready_body, json!({})).await?;
            let state: Value = serde_json::from_str(raw.as_str().context("Invalid readiness")?)?;
            let stable = previous.as_ref() == Some(&state["fingerprint"]);
            previous = Some(state["fingerprint"].clone());
            if state["ready"] == true && stable || tokio::time::Instant::now() >= deadline {
                break json!({"ready":state["ready"] == true && stable,"reasons":state["reasons"]});
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        };
        let mut page = self.dom(SUMMARY_SCRIPT, json!({"offset":offset})).await?;
        page["readiness"] = readiness;
        self.check_current().await?;
        self.redact(&mut page)?;
        // Redact before truncating so a clipped password cannot bypass it.
        if let Some(text) = page["text"].as_str() {
            let truncated = text.chars().count() > 1200;
            let text: String = text.chars().take(1200).collect();
            page["text"] = json!(text);
            page["text_truncated"] = json!(truncated || page["text_truncated"] == true);
        }
        if let Some(controls) = page["controls"].as_array_mut() {
            for control in controls {
                if let Some(text) = control["text"].as_str() {
                    control["text"] = json!(text.chars().take(160).collect::<String>());
                }
            }
        }
        page_result(page, offset)
    }

    pub async fn browse(&self, args: Browse) -> Result<Value> {
        match args {
            Browse::Open { url } => {
                let url = validate_url(&url)?;
                self.call(Method::POST, "/url", json!({"url":url.as_str()}))
                    .await?;
                self.summary(0).await
            }
            Browse::Read { offset } => self.summary(offset).await,
            Browse::Scroll { direction } => {
                self.check_current().await?;
                self.execute_fixed(match direction {
                    Direction::Up => "window.scrollBy(0,-700); return true;",
                    Direction::Down => "window.scrollBy(0,700); return true;",
                })
                .await?;
                self.summary(0).await
            }
            Browse::Screenshot => {
                if self.screenshots_disabled.load(Ordering::SeqCst) {
                    bail!(
                        "Screenshots are disabled for this persistent profile after credential use; use the redacted page reader"
                    );
                }
                self.check_current().await?;
                let png = self.call(Method::GET, "/screenshot", Value::Null).await?;
                self.check_current().await?;
                let data = png.as_str().context("Invalid screenshot")?;
                screenshot_result(data)
            }
        }
    }

    pub async fn interact(&self, args: Interact) -> Result<Value> {
        self.check_current().await?;
        let selector = match &args {
            Interact::Click { selector }
            | Interact::Fill { selector, .. }
            | Interact::Press { selector, .. } => selector,
        };
        if selector.is_empty() || selector.len() > 2000 {
            bail!("Selector must be 1-2000 characters");
        }
        self.check_current().await?;
        match args {
            Interact::Click { selector } => {
                let point = self
                    .dom(
                        &format!("{SUBMIT_GUARD}{CLICK_POINT}"),
                        json!({"selector":selector}),
                    )
                    .await?;
                self.call(Method::POST, "/actions", json!({"actions":[{"type":"pointer","id":"auth-browser-pointer","parameters":{"pointerType":"mouse"},"actions":[{"type":"pointerMove","duration":0,"origin":"viewport","x":point["x"],"y":point["y"]},{"type":"pointerDown","button":0},{"type":"pointerUp","button":0}]}]}))
                    .await.context("Click was attempted; read the page to reconcile before any retry")?;
            }
            Interact::Fill { selector, text } => {
                if text.len() > 8000 {
                    bail!("Text exceeds 8000 characters");
                }
                self.dom(FILL_SCRIPT, json!({"selector":selector,"text":text}))
                    .await?;
            }
            Interact::Press { selector, key } => {
                self.dom(
                    &format!("{SUBMIT_GUARD}{FOCUS_SCRIPT}"),
                    json!({"selector":selector}),
                )
                .await?;
                self.check_current().await?;
                // Keyboard actions never reinterpret key text as a local file.
                self.call(
                    Method::POST,
                    "/actions",
                    json!({"actions":[{
                        "type":"key","id":"auth-browser-keyboard","actions":[
                            {"type":"keyDown","value":key.wire()},
                            {"type":"keyUp","value":key.wire()}
                        ]
                    }]}),
                )
                .await?;
            }
        }
        self.summary(0).await.map_err(|error| anyhow::Error::msg(format!("Browser action was attempted; do not repeat it. Read the page to reconcile the outcome. Observation failed: {error}")))
    }

    pub async fn close(&mut self) {
        if let Some(sid) = self.session.take() {
            let _ = tokio::time::timeout(
                Duration::from_secs(1),
                self.request(Method::DELETE, &format!("/session/{sid}"), Value::Null),
            )
            .await;
        }
        if self.driver_group > 0 {
            unsafe {
                libc::kill(-self.driver_group, libc::SIGTERM);
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), self.driver.wait()).await;
            // Reap any Chrome children that ignored graceful termination before
            // releasing the exclusive profile lock or stopping the watchdog.
            unsafe {
                libc::kill(-self.driver_group, libc::SIGKILL);
            }
            let _ = tokio::time::timeout(Duration::from_secs(1), self.driver.wait()).await;
            self.driver_group = 0;
        }
        let _ = self.watchdog.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(1), self.watchdog.wait()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secrets_are_redacted_from_nested_values_keys_encodings_and_clipped_suffixes() {
        let secret = "Synthetic p&ss<>42";
        let secrets = secret_variants(secret);
        let mut page = json!({secret: {"url":"https://example.com/?p=Synthetic+p%26ss%3C%3E42",
            "text":"before Synthetic p&ss<>42 after", "clipped":"before Synthetic p&ss<>"}});
        redact_value(&mut page, &secrets);
        let output = page.to_string();
        assert!(!output.contains(secret));
        assert!(!output.contains("Synthetic"));
        assert!(page.get("[REDACTED]").is_some());
    }

    #[test]
    fn prefix_redaction_preserves_summary_schema_and_ordinary_short_suffixes() {
        let mut page =
            json!({"text":"a normal text", "url":"https://example.com/test", "controls":[]});
        let expected = page.clone();
        redact_value(&mut page, &secret_variants("textual-synthetic-password"));
        // A long matching prefix in page text is conservative; static keys and
        // unrelated short suffixes must remain usable by the reader.
        assert!(page.get("text").is_some());
        assert_eq!(page["url"], expected["url"]);
        assert!(page.get("controls").is_some());
    }

    #[test]
    fn private_profile_lock_is_exclusive_and_released_when_dropped() {
        let path = std::env::temp_dir().join(format!(
            "zeroclaw-auth-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = lock_profile(&path).unwrap();
        assert_eq!(
            first.metadata().unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(lock_profile(&path).is_err());
        drop(first);
        drop(lock_profile(&path).unwrap());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn persisted_redaction_origin_list_is_bounded_and_missing_metadata_fails_closed() {
        let path = std::env::temp_dir().join(format!(
            "zeroclaw-auth-origins-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert_eq!(
            read_redaction_origins(&path, false).unwrap(),
            Vec::<String>::new()
        );
        assert!(read_redaction_origins(&path, true).is_err());
        write_redaction_origins(&path, &["https://example.com".into()]).unwrap();
        assert_eq!(
            read_redaction_origins(&path, true).unwrap(),
            vec!["https://example.com"]
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        write_redaction_origins(&path, &["https://linkedin.com".into()]).unwrap();
        assert!(read_redaction_origins(&path, true).is_err());
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn paginated_controls_survive_encoded_budget_without_skips() {
        let controls: Vec<Value> = (0..80).map(|i| json!({"selector":format!("shadow:[\"#host{i}\",\"#control\"]"),"text":"\"\\多😀".repeat(40)})).collect();
        let mut offset = 0;
        while offset < controls.len() {
            let page = json!({"url":"https://example.com","text":"\"\\多😀".repeat(150),"totalControls":controls.len(),"controls":controls[offset..(offset+12).min(controls.len())],"offset":offset});
            let result = page_result(page, offset).unwrap();
            assert!(
                serde_json::to_string(&serde_json::to_string_pretty(&result).unwrap())
                    .unwrap()
                    .len()
                    <= 3500
            );
            let parsed: Value =
                serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
            let count = parsed["controls"].as_array().unwrap().len();
            assert!(count > 0);
            assert_eq!(parsed["controls"], json!(controls[offset..offset + count]));
            offset += count;
            if offset < controls.len() {
                assert_eq!(parsed["next_offset"], offset);
            } else {
                assert!(parsed["next_offset"].is_null());
            }
        }
    }
    #[test]
    fn screenshots_use_embedded_resources_and_keep_the_existing_capture_limit() {
        for len in [100_000, 3_000_000] {
            let data = "A".repeat(len);
            let result = screenshot_result(&data).unwrap();
            let item = &result["content"][0];
            assert_eq!(item["type"], "resource");
            assert!(item.get("data").is_none());
            assert_eq!(item["resource"]["mimeType"], "image/png");
            assert_eq!(item["resource"]["blob"], data);
            assert_eq!(
                item["resource"]["uri"],
                "zeroclaw://auth-browser/screenshot.png"
            );
        }
        assert!(screenshot_result(&"A".repeat(3_000_001)).is_err());
    }

    #[test]
    fn browse_never_accepts_mutating_actions_or_arbitrary_code() {
        for a in [
            json!({"action":"click","selector":"button"}),
            json!({"action":"fill","selector":"input","text":"send"}),
            json!({"action":"execute","script":"fetch('https://linkedin.com')"}),
        ] {
            assert!(serde_json::from_value::<Browse>(a).is_err());
        }
        assert!(serde_json::from_value::<Browse>(json!({"action":"read"})).is_ok());
    }
    #[test]
    fn interaction_has_no_script_cookie_profile_or_upload_surface() {
        for action in [
            "execute", "evaluate", "cookies", "upload", "launch", "connect",
        ] {
            assert!(serde_json::from_value::<Interact>(json!({"action":action})).is_err());
        }
    }
}
