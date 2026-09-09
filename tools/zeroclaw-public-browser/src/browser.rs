use crate::{policy::validate_url, proxy};
use anyhow::{Context, Result, bail};
use reqwest::{Client, Method};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{os::unix::process::CommandExt as _, path::Path, process::Stdio, time::Duration};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    task::JoinHandle,
};

const CHROME: &str = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const ELEMENT: &str = "element-6066-11e4-a52e-4f735466cecf";
// The live DOM is authoritative; share the Safari connector's fixed traversal
// and scoped selectors instead of maintaining another component resolver.
const DOM_COMMON: &str = include_str!("../../zeroclaw-safari-browser/src/dom/common.js");
const DOM_READY: &str = include_str!("../../zeroclaw-safari-browser/src/dom/ready.js");
const CLICK_POINT: &str = include_str!("click-point.js");
const SUMMARY_SCRIPT: &str = r#"
const controls = deepQuery(controlsSelector).filter(visible);
const text = composedText(document.body,1201);
return {url:location.href,title:document.title,text:text.slice(0,1200),text_truncated:text.length>1200,
 totalControls:controls.length, offset:args.offset,
 controls:controls.slice(args.offset,args.offset+12).map(e=>{
 const control = {tag:e.tagName.toLowerCase(),text:controlText(e).slice(0,160),disabled:disabled(e),...(e.href?{href:e.href}:{})};
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
        "uri":"zeroclaw://public-browser/screenshot.png",
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
}

impl Drop for Browser {
    fn drop(&mut self) {
        self.proxy_task.abort();
        // The driver was started in its own process group, which contains only
        // the ephemeral Chrome it launched. Never target an existing browser.
        if self.driver_group > 0 {
            unsafe {
                libc::kill(-self.driver_group, libc::SIGTERM);
            }
        }
        let _ = self.watchdog.start_kill();
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
            .args(["--watch-driver-group", &driver_group.to_string()])
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
                format!("--proxy-server=http://{proxy_address}"), "--proxy-bypass-list=<-loopback>",
                "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
                "--disable-background-networking", "--disable-extensions", "--no-first-run"
            ],"prefs":{"download_restrictions":3,"profile.default_content_setting_values.notifications":2}}
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
        let response = request.send().await?;
        let status = response.status();
        let body: Value = response.json().await?;
        if !status.is_success() || body["value"]["error"].is_string() {
            let code = body["value"]["error"].as_str().unwrap_or("webdriver error");
            // Avoid emitting stack traces, process flags or filesystem paths.
            bail!(
                "Browser {code}: {}",
                body["value"]["message"]
                    .as_str()
                    .unwrap_or("navigation or interaction failed")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(240)
                    .collect::<String>()
            );
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

    async fn check_current(&self) -> Result<()> {
        let url = self.call(Method::GET, "/url", Value::Null).await?;
        validate_url(url.as_str().context("No current URL")?)?;
        Ok(())
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
            "try {{ const args = arguments[0]; {DOM_COMMON} {body} }} catch(error) {{ return {{__domError:String(error.message || error).slice(0,240)}}; }}"
        );
        let value = self
            .call(
                Method::POST,
                "/execute/sync",
                json!({"script":script,"args":[args]}),
            )
            .await?;
        if let Some(error) = value["__domError"].as_str() {
            bail!("Browser DOM operation failed: {error}");
        }
        Ok(value)
    }

    async fn summary(&self, offset: usize) -> Result<Value> {
        self.check_current().await?;
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
        let id = if matches!(args, Interact::Click { .. }) {
            None
        } else {
            let element = self
                .dom(
                    "return target(args.selector);",
                    json!({"selector":selector}),
                )
                .await?;
            let id = element[ELEMENT]
                .as_str()
                .context("No matching element")?
                .to_owned();
            if !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
            {
                bail!("Invalid element ID");
            }
            Some(id)
        };
        self.check_current().await?;
        match args {
            Interact::Click { selector } => {
                let point = self.dom(CLICK_POINT, json!({"selector":selector})).await?;
                self.call(Method::POST, "/actions", json!({"actions":[{"type":"pointer","id":"public-browser-pointer","parameters":{"pointerType":"mouse"},"actions":[{"type":"pointerMove","duration":0,"origin":"viewport","x":point["x"],"y":point["y"]},{"type":"pointerDown","button":0},{"type":"pointerUp","button":0}]}]}))
                    .await.context("Click was attempted; read the page to reconcile before any retry")?;
            }
            Interact::Fill { text, .. } => {
                let id = id.as_deref().context("Missing field target")?;
                if text.len() > 8000 {
                    bail!("Text exceeds 8000 characters");
                }
                self.call(Method::POST, &format!("/element/{id}/clear"), json!({}))
                    .await?;
                self.call(
                    Method::POST,
                    &format!("/element/{id}/value"),
                    json!({"text":text}),
                )
                .await?;
            }
            Interact::Press { key, .. } => {
                let id = id.as_deref().context("Missing key target")?;
                self.call(
                    Method::POST,
                    &format!("/element/{id}/value"),
                    json!({"text":key.wire()}),
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
                "zeroclaw://public-browser/screenshot.png"
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
