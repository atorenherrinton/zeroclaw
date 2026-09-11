//! Dedicated, fail-closed WebDriver adapter. Never starts or stops a browser.
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{Method, StatusCode, header},
    response::Response,
};
use serde_json::{Value, json};
use std::{
    future::Future,
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore};

const LISTEN: &str = "127.0.0.1:9516";
const DRIVER: &str = "http://127.0.0.1:9515";
const DEBUGGER: &str = "127.0.0.1:18801";
const MAX_REQUEST: usize = 32 * 1024;
const MAX_RESPONSE: usize = 16 * 1024 * 1024;
const HOSTS: &[&str] = &[
    "reddit.com",
    "www.reddit.com",
    "old.reddit.com",
    "mod.reddit.com",
    "modmail.reddit.com",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Failure {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl Failure {
    fn denied() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "unsupported operation",
            message: "Read-only browser policy rejected this request",
        }
    }
    fn invalid_session() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "invalid session id",
            message: "No matching adapter session",
        }
    }
    fn unavailable() -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: "unknown error",
            message: "Dedicated browser driver unavailable",
        }
    }
    fn busy() -> Self {
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "session not created",
            message: "Dedicated adapter is busy",
        }
    }
    fn response(self) -> Response {
        response(
            self.status,
            json!({"value":{"error":self.code,"message":self.message,"stacktrace":""}}),
        )
    }
}

type SafeResult<T> = Result<T, Failure>;

fn response(status: StatusCode, body: Value) -> Response {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
        .header(header::CACHE_CONTROL, "no-store")
        .header("X-Content-Type-Options", "nosniff")
        .body(Body::from(body.to_string()))
        .expect("static response headers")
}

fn opaque_id(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && value.len() <= 256
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn allowed_url(raw: &str) -> bool {
    if raw.len() > 4096 || raw.bytes().any(|b| b.is_ascii_control() || b == b'\\') {
        return false;
    }
    let Ok(url) = url::Url::parse(raw) else {
        return false;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || !url.host_str().is_some_and(|host| HOSTS.contains(&host))
    {
        return false;
    }
    // Keep this dedicated monitor out of authentication, posting, preferences,
    // and API routes. Modern Reddit moved private modmail into Matrix-style
    // room paths. Only the literal room identifier and its encoded colon are
    // admitted; this is not a general percent-decoding or chat-action rule.
    if url.query().is_some() || url.fragment().is_some() {
        return false;
    }
    let path = url.path().trim_end_matches('/');
    if matches!(url.host_str(), Some("reddit.com" | "www.reddit.com"))
        && (path == "/notifications" || modern_room(path))
    {
        return true;
    }
    if path.contains('%') {
        return false;
    }
    match url.host_str().unwrap_or_default() {
        "mod.reddit.com" | "modmail.reddit.com" => {
            path == "/mail/all"
                || path == "/mail/inbox"
                || path == "/mail/unread"
                || path.strip_prefix("/mail/perma/").is_some_and(simple_item)
        }
        _ => {
            ["/message/inbox", "/message/messages", "/message/unread"].contains(&path)
                || path
                    .strip_prefix("/message/messages/")
                    .is_some_and(simple_item)
        }
    }
}

fn modern_room(path: &str) -> bool {
    path.strip_prefix("/room/!")
        .and_then(|room| {
            room.strip_suffix("%3Areddit.com")
                .or_else(|| room.strip_suffix("%3areddit.com"))
                .or_else(|| room.strip_suffix(":reddit.com"))
        })
        .is_some_and(|id| {
            (8..=128).contains(&id.len())
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
}

fn simple_item(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'/')
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Status,
    NewSession,
    Close,
    Read,
    CurrentUrl,
    Navigate,
    SetTimeouts,
}

fn classify(method: &Method, route: &str, body: &Value) -> SafeResult<Command> {
    if route.contains('%')
        || route.contains('?')
        || route.contains('\\')
        || route.contains("//")
        || route.ends_with('/')
    {
        return Err(Failure::denied());
    }
    let parts: Vec<_> = route.split('/').collect();
    let command = match (method.as_str(), parts.as_slice()) {
        ("GET", ["", "status"]) => Command::Status,
        ("POST", ["", "session"])
            if body.is_object() && body.get("capabilities").is_some_and(Value::is_object) =>
        {
            Command::NewSession
        }
        ("DELETE", ["", "session", sid]) if opaque_id(sid) => Command::Close,
        (
            "GET",
            [
                "",
                "session",
                sid,
                "title" | "source" | "window" | "timeouts",
            ],
        ) if opaque_id(sid) => Command::Read,
        ("GET", ["", "session", sid, "url"]) if opaque_id(sid) => Command::CurrentUrl,
        ("POST", ["", "session", sid, "url"])
            if opaque_id(sid)
                && exact_keys(body, &["url"])
                && body["url"].as_str().is_some_and(allowed_url) =>
        {
            Command::Navigate
        }
        ("POST", ["", "session", sid, "timeouts"]) if opaque_id(sid) && valid_timeouts(body) => {
            Command::SetTimeouts
        }
        ("POST", ["", "session", sid, "element" | "elements"])
            if opaque_id(sid) && valid_locator(body) =>
        {
            Command::Read
        }
        ("POST", ["", "session", sid, "element", eid, "element" | "elements"])
            if opaque_id(sid) && opaque_id(eid) && valid_locator(body) =>
        {
            Command::Read
        }
        (
            "GET",
            [
                "",
                "session",
                sid,
                "element",
                eid,
                "text" | "name" | "rect" | "enabled" | "selected" | "displayed",
            ],
        ) if opaque_id(sid) && opaque_id(eid) => Command::Read,
        ("GET", ["", "session", sid, "element", eid, "attribute", name])
            if opaque_id(sid) && opaque_id(eid) && allowed_attribute(name) =>
        {
            Command::Read
        }
        _ => return Err(Failure::denied()),
    };
    if *method != Method::POST && !body.is_null() {
        return Err(Failure::denied());
    }
    Ok(command)
}

fn exact_keys(body: &Value, keys: &[&str]) -> bool {
    body.as_object()
        .is_some_and(|o| o.len() == keys.len() && keys.iter().all(|key| o.contains_key(*key)))
}

fn valid_locator(body: &Value) -> bool {
    exact_keys(body, &["using", "value"])
        && matches!(body["using"].as_str(), Some("css selector" | "xpath"))
        && body["value"]
            .as_str()
            .is_some_and(|v| !v.is_empty() && v.len() <= 2048 && !v.contains('\0'))
}

fn allowed_attribute(name: &str) -> bool {
    matches!(
        name,
        "href"
            | "title"
            | "role"
            | "id"
            | "class"
            | "aria-label"
            | "aria-labelledby"
            | "aria-describedby"
            | "data-testid"
            | "data-zc-ref"
    )
}

fn valid_timeouts(body: &Value) -> bool {
    body.as_object().is_some_and(|o| {
        !o.is_empty()
            && o.iter().all(|(key, value)| {
                let limit = match key.as_str() {
                    "implicit" => 1000,
                    "pageLoad" => 30000,
                    "script" => 0,
                    _ => return false,
                };
                value.as_u64().is_some_and(|n| n <= limit)
            })
    })
}

fn fixed_capabilities() -> Value {
    // Remote-attach ChromeDriver rejects launch-only `detach`. Browser ownership
    // is protected by our logical close, which never forwards session DELETE.
    json!({"capabilities":{"alwaysMatch":{
        "browserName":"chrome",
        "acceptInsecureCerts":false,
        "pageLoadStrategy":"normal",
        "unhandledPromptBehavior":"ignore",
        "timeouts":{"implicit":0,"pageLoad":30000,"script":0},
        "goog:chromeOptions":{"debuggerAddress":DEBUGGER}
    },"firstMatch":[{}]}})
}

trait DriverApi: Send + Sync {
    fn call(
        &self,
        method: Method,
        route: String,
        body: Value,
    ) -> impl Future<Output = SafeResult<Value>> + Send;
}

struct HttpDriver {
    client: reqwest::Client,
}

impl DriverApi for HttpDriver {
    async fn call(&self, method: Method, route: String, body: Value) -> SafeResult<Value> {
        let mut request = self.client.request(method, format!("{DRIVER}{route}"));
        if !body.is_null() {
            request = request.json(&body);
        }
        let mut response = request.send().await.map_err(|_| Failure::unavailable())?;
        let status = response.status();
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(Failure::unavailable());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| Failure::unavailable())? {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(Failure::unavailable());
            }
            bytes.extend_from_slice(&chunk);
        }
        let parsed: Value = serde_json::from_slice(&bytes).map_err(|_| Failure::unavailable())?;
        let value = parsed.get("value").ok_or_else(Failure::unavailable)?;
        if !status.is_success() || value.get("error").is_some() {
            return Err(match value["error"].as_str() {
                Some("invalid session id") => Failure::invalid_session(),
                Some("no such element") => Failure {
                    status: StatusCode::NOT_FOUND,
                    code: "no such element",
                    message: "No matching element",
                },
                Some("stale element reference") => Failure {
                    status: StatusCode::NOT_FOUND,
                    code: "stale element reference",
                    message: "Element reference expired",
                },
                Some("invalid selector") => Failure {
                    status: StatusCode::BAD_REQUEST,
                    code: "invalid selector",
                    message: "Invalid read-only selector",
                },
                Some("timeout") => Failure {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    code: "timeout",
                    message: "Read-only browser operation timed out",
                },
                _ => Failure::unavailable(),
            });
        }
        Ok(value.clone())
    }
}

#[derive(Default)]
struct Session {
    upstream_id: Option<String>,
    public_id: Option<String>,
    capabilities: Value,
    last_used: Option<Instant>,
}

impl Session {
    fn observe<T>(&mut self, result: SafeResult<T>) -> SafeResult<T> {
        if result
            .as_ref()
            .is_err_and(|error| error.code == "invalid session id")
        {
            self.upstream_id = None;
            self.public_id = None;
        }
        result
    }
}

struct Proxy<D> {
    driver: D,
    session: Mutex<Session>,
}

impl<D: DriverApi> Proxy<D> {
    async fn approved_current(&self, sid: &str) -> SafeResult<()> {
        let current = self
            .driver
            .call(Method::GET, format!("/session/{sid}/url"), Value::Null)
            .await?;
        if current.as_str().is_some_and(allowed_url) {
            Ok(())
        } else {
            Err(Failure::denied())
        }
    }

    async fn dispatch(&self, method: Method, route: &str, body: Value) -> SafeResult<Value> {
        let command = classify(&method, route, &body)?;
        if command == Command::Status {
            return Ok(json!({"ready":true,"message":"Read-only adapter ready"}));
        }
        // No queued requests can race a navigation with a data read.
        let mut session = self.session.try_lock().map_err(|_| Failure::busy())?;
        if command == Command::NewSession {
            if session
                .last_used
                .is_some_and(|used| used.elapsed() >= Duration::from_secs(300))
            {
                // A crashed local client must not reserve this profile forever.
                // This expires only the adapter lease, never the browser.
                session.public_id = None;
            }
            if session.public_id.is_some() {
                return Err(Failure::busy());
            }
            if session.upstream_id.is_none() {
                let value = self
                    .driver
                    .call(Method::POST, "/session".into(), fixed_capabilities())
                    .await?;
                let sid = value["sessionId"]
                    .as_str()
                    .filter(|s| opaque_id(s))
                    .ok_or_else(Failure::unavailable)?;
                session.upstream_id = Some(sid.to_owned());
                let mut caps = serde_json::Map::new();
                for key in ["browserName", "browserVersion", "platformName"] {
                    if let Some(value) = value["capabilities"][key]
                        .as_str()
                        .filter(|s| s.len() <= 100)
                    {
                        caps.insert(key.into(), json!(value));
                    }
                }
                caps.insert("setWindowRect".into(), json!(false));
                session.capabilities = Value::Object(caps);
            }
            let sid = uuid::Uuid::new_v4().simple().to_string();
            session.public_id = Some(sid.clone());
            session.last_used = Some(Instant::now());
            return Ok(json!({"sessionId":sid,"capabilities":session.capabilities}));
        }
        let mut parts = route.split('/');
        let requested = parts.nth(2).ok_or_else(Failure::invalid_session)?;
        if session.public_id.as_deref() != Some(requested) {
            return Err(Failure::invalid_session());
        }
        session.last_used = Some(Instant::now());
        if command == Command::Close {
            // Intentionally no upstream DELETE: closing the attached session
            // would close the user's independently running Chrome profile.
            session.public_id = None;
            return Ok(Value::Null);
        }
        let sid = session
            .upstream_id
            .clone()
            .ok_or_else(Failure::invalid_session)?;
        // Fantoccini goto() reads current_url even for an absolute URL. Allow
        // only the harmless about:blank bootstrap, never a foreign/login URL.
        // No page-content reads are permitted from about:blank.
        if command == Command::CurrentUrl {
            let result = self
                .driver
                .call(Method::GET, format!("/session/{sid}/url"), Value::Null)
                .await;
            let value = session.observe(result)?;
            return if value
                .as_str()
                .is_some_and(|s| s == "about:blank" || allowed_url(s))
            {
                Ok(value)
            } else {
                Err(Failure::denied())
            };
        }
        if command == Command::Read {
            session.observe(self.approved_current(&sid).await)?;
        }
        let suffix = route
            .strip_prefix(&format!("/session/{requested}"))
            .ok_or_else(Failure::denied)?;
        let result = self
            .driver
            .call(method, format!("/session/{sid}{suffix}"), body)
            .await;
        let value = session.observe(result)?;
        if matches!(command, Command::Read | Command::Navigate) {
            session.observe(self.approved_current(&sid).await)?;
        }
        // The driver result is not released until the post-read origin check.
        Ok(value)
    }
}

struct AppState {
    proxy: Proxy<HttpDriver>,
    token: String,
    capacity: Semaphore,
}

fn authenticated_route<'a>(request: &'a Request, token: &str) -> SafeResult<&'a str> {
    if request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        != Some(LISTEN)
        || request.headers().contains_key(header::ORIGIN)
        || request.headers().contains_key(header::REFERER)
        || request.headers().contains_key("sec-fetch-site")
        || request.uri().query().is_some()
    {
        return Err(Failure::denied());
    }
    let route = request
        .uri()
        .path()
        .strip_prefix('/')
        .and_then(|p| p.split_once('/'))
        .ok_or_else(Failure::denied)?;
    let equal = route.0.len() == token.len()
        && route
            .0
            .bytes()
            .zip(token.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0;
    if !equal {
        return Err(Failure::denied());
    }
    request
        .uri()
        .path()
        .strip_prefix(&format!("/{token}"))
        .ok_or_else(Failure::denied)
}

async fn handle(State(state): State<Arc<AppState>>, request: Request) -> Response {
    let result = async {
        let _permit = state.capacity.try_acquire().map_err(|_| Failure::busy())?;
        let route = authenticated_route(&request, &state.token)?.to_owned();
        let method = request.method().clone();
        if method == Method::POST
            && request
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.split(';').next().unwrap_or("").trim())
                != Some("application/json")
        {
            return Err(Failure::denied());
        }
        let bytes = tokio::time::timeout(
            Duration::from_secs(5),
            to_bytes(request.into_body(), MAX_REQUEST),
        )
        .await
        .map_err(|_| Failure::denied())?
        .map_err(|_| Failure::denied())?;
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).map_err(|_| Failure::denied())?
        };
        state.proxy.dispatch(method, &route, body).await
    }
    .await;
    match result {
        Ok(value) => response(StatusCode::OK, json!({"value":value})),
        Err(error) => error.response(),
    }
}

fn read_token(path: &Path) -> Result<String, &'static str> {
    if !path.is_absolute() {
        return Err("Token file must be absolute");
    }
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| "Token file unavailable")?;
    let meta = file.metadata().map_err(|_| "Token file unavailable")?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o077 != 0
        || meta.nlink() != 1
        || meta.len() > 65
    {
        return Err("Token file must be private and regular");
    }
    let mut value = String::new();
    file.read_to_string(&mut value)
        .map_err(|_| "Token file invalid")?;
    let value = value.trim_end_matches('\n');
    if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("Token file must contain 32 random bytes encoded as hex");
    }
    Ok(value.to_owned())
}

#[tokio::main]
async fn main() {
    if run().await.is_err() {
        // Deliberately no request URLs, driver responses, page contents, or token.
        eprintln!("Read-only browser adapter stopped: configuration or listener unavailable");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), &'static str> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() != 2 || args[0] != "--token-file" {
        return Err("Usage: zeroclaw-reddit-browser --token-file PRIVATE_FILE");
    }
    let token = read_token(Path::new(&args[1]))?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(35))
        .build()
        .map_err(|_| "HTTP client unavailable")?;
    let state = Arc::new(AppState {
        proxy: Proxy {
            driver: HttpDriver { client },
            session: Mutex::new(Session::default()),
        },
        token,
        capacity: Semaphore::new(4),
    });
    let listener = tokio::net::TcpListener::bind(LISTEN)
        .await
        .map_err(|_| "Listener unavailable")?;
    axum::serve(listener, Router::new().fallback(handle).with_state(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|_| "Listener stopped")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Default)]
    struct MemoryDriver {
        calls: std::sync::Mutex<Vec<(Method, String, Value)>>,
        current: std::sync::Mutex<String>,
        escape_after_data: AtomicBool,
        invalid_next: AtomicBool,
    }

    impl DriverApi for MemoryDriver {
        async fn call(&self, method: Method, route: String, body: Value) -> SafeResult<Value> {
            self.calls
                .lock()
                .unwrap()
                .push((method.clone(), route.clone(), body.clone()));
            if self.invalid_next.swap(false, Ordering::SeqCst) {
                return Err(Failure::invalid_session());
            }
            if route == "/session" {
                return Ok(
                    json!({"sessionId":"upstream-1","capabilities":{"browserName":"chrome","browserVersion":"fixture","platformName":"mac","chrome":{"userDataDir":"DO-NOT-EXPOSE"},"goog:chromeOptions":{"debuggerAddress":"DO-NOT-EXPOSE"}}}),
                );
            }
            if route.ends_with("/url") {
                if method == Method::POST {
                    *self.current.lock().unwrap() = body["url"].as_str().unwrap().into();
                    return Ok(Value::Null);
                }
                return Ok(json!(*self.current.lock().unwrap()));
            }
            if self.escape_after_data.swap(false, Ordering::SeqCst) {
                *self.current.lock().unwrap() = "https://foreign.invalid/private".into();
            }
            if route.ends_with("/element") {
                return Ok(
                    json!({"element-6066-11e4-a52e-4f735466cecf":"f.fixture.d.fixture.e.1"}),
                );
            }
            if route.ends_with("/text") {
                return Ok(json!("Synthetic inbox fixture"));
            }
            if route.ends_with("/title") {
                return Ok(json!("Fixture inbox"));
            }
            if route.ends_with("/source") {
                return Ok(json!("<body>Synthetic inbox fixture</body>"));
            }
            if route.ends_with("/screenshot") {
                return Ok(json!("AQI="));
            }
            if route.ends_with("/attribute/role") {
                return Ok(json!("main"));
            }
            if route.ends_with("/displayed") {
                return Ok(json!(true));
            }
            if route.ends_with("/window") {
                return Ok(json!("fixture-window"));
            }
            Err(Failure::denied())
        }
    }

    fn proxy(url: &str) -> Proxy<MemoryDriver> {
        Proxy {
            driver: MemoryDriver {
                current: std::sync::Mutex::new(url.into()),
                ..Default::default()
            },
            session: Mutex::new(Session::default()),
        }
    }

    async fn session(proxy: &Proxy<MemoryDriver>) -> String {
        proxy.dispatch(Method::POST, "/session", json!({"capabilities":{"alwaysMatch":{"goog:chromeOptions":{"debuggerAddress":"evil.invalid:80","args":["--disable-web-security"],"prefs":{"unsafe":true}}}}})).await.unwrap()["sessionId"].as_str().unwrap().into()
    }

    #[test]
    fn destinations_are_exact_https_and_inbox_only() {
        for url in [
            "https://www.reddit.com/message/messages/",
            "https://old.reddit.com/message/inbox",
            "https://reddit.com/message/messages/abc123",
            "https://mod.reddit.com/mail/all",
            "https://modmail.reddit.com/mail/perma/abc123",
            "https://www.reddit.com/notifications",
            "https://www.reddit.com/room/!SyntheticRoom0123%3Areddit.com",
            "https://reddit.com/room/!SyntheticRoom0123:reddit.com/",
        ] {
            assert!(allowed_url(url), "{url}");
        }
        for url in [
            "http://www.reddit.com/message/messages",
            "https://reddit.com.evil.invalid/message/inbox",
            "https://evil.reddit.com/message/inbox",
            "https://www.reddit.com:444/message/inbox",
            "https://user@www.reddit.com/message/inbox",
            "https://www.reddit.com/logout",
            "https://www.reddit.com/message/compose",
            "https://www.reddit.com/api/read_message",
            "https://www.reddit.com/message/messages?after=abc",
            "https://www.reddit.com/message/messages#fragment",
            "https://www.reddit.com/message/%69nbox",
            "javascript:alert(1)",
            "file:///etc/passwd",
            "https://127.0.0.1/message/inbox",
            "https://www.reddit.com/room/create",
            "https://www.reddit.com/room/!SyntheticRoom0123%3Areddit.com/settings",
            "https://www.reddit.com/room/!SyntheticRoom0123%3Aevil.invalid",
            "https://www.reddit.com/room/!Synthetic%2FRoom%3Areddit.com",
            "https://www.reddit.com/room/!SyntheticRoom0123%3Areddit.com?send=yes",
            "https://www.reddit.com/notifications#unsafe",
            "https://www.reddit.com/notifications/settings",
            "https://old.reddit.com/room/!SyntheticRoom0123%3Areddit.com",
        ] {
            assert!(!allowed_url(url), "{url}");
        }
    }

    #[test]
    fn unknown_mutating_and_credential_commands_are_denied() {
        for (method, route, body) in [
            (
                Method::POST,
                "/session/x/execute/sync",
                json!({"script":"return document.body.innerText","args":[]}),
            ),
            (Method::POST, "/session/x/execute/async", json!({})),
            (Method::POST, "/session/x/element/y/click", json!({})),
            (
                Method::POST,
                "/session/x/element/y/value",
                json!({"text":"hello"}),
            ),
            (Method::POST, "/session/x/element/y/clear", json!({})),
            (Method::POST, "/session/x/actions", json!({})),
            (Method::GET, "/session/x/cookie", Value::Null),
            (Method::POST, "/session/x/cookie", json!({})),
            (
                Method::GET,
                "/session/x/element/y/property/value",
                Value::Null,
            ),
            (
                Method::GET,
                "/session/x/element/y/attribute/value",
                Value::Null,
            ),
            (Method::POST, "/session/x/goog/cdp/execute", json!({})),
            (Method::POST, "/session/x/chromium/send_command", json!({})),
            (Method::POST, "/session/x/window", json!({"handle":"other"})),
            (Method::DELETE, "/session/x/window", Value::Null),
            (Method::POST, "/session/x/frame", json!({"id":0})),
            (Method::GET, "/session/x/window/handles", Value::Null),
            (Method::GET, "/session/x/screenshot", Value::Null),
            (Method::POST, "/session/x/refresh", json!({})),
            (Method::GET, "/session/x/%2e%2e/url", Value::Null),
        ] {
            assert_eq!(
                classify(&method, route, &body),
                Err(Failure::denied()),
                "{route}"
            );
        }
        assert!(
            classify(
                &Method::POST,
                "/session/x/element",
                &json!({"using":"css selector","value":"body"})
            )
            .is_ok()
        );
        assert!(
            classify(
                &Method::POST,
                "/session/x/element",
                &json!({"using":"javascript","value":"evil"})
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn fixed_attach_capabilities_replace_all_caller_options() {
        let p = proxy("about:blank");
        let sid = session(&p).await;
        {
            let calls = p.driver.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].2, fixed_capabilities());
        }
        let stored = p.session.lock().await;
        assert_eq!(stored.public_id.as_deref(), Some(sid.as_str()));
        assert!(!stored.capabilities.to_string().contains("DO-NOT-EXPOSE"));
        assert_eq!(
            fixed_capabilities()["capabilities"]["alwaysMatch"]["goog:chromeOptions"]["debuggerAddress"],
            DEBUGGER
        );
        // ChromeDriver ParseChromeOptions' remote branch rejects launch-only
        // detach, even though detach is valid when creating a new Chrome process.
        assert_eq!(
            fixed_capabilities()["capabilities"]["alwaysMatch"]["goog:chromeOptions"],
            json!({"debuggerAddress": DEBUGGER})
        );
    }

    #[tokio::test]
    async fn logical_close_never_quits_or_restarts_the_browser() {
        let p = proxy("about:blank");
        let sid = session(&p).await;
        p.dispatch(Method::DELETE, &format!("/session/{sid}"), Value::Null)
            .await
            .unwrap();
        let second = session(&p).await;
        assert_ne!(sid, second);
        assert_eq!(p.driver.calls.lock().unwrap().len(), 1);
        assert_eq!(
            p.dispatch(Method::GET, &format!("/session/{sid}/url"), Value::Null)
                .await,
            Err(Failure::invalid_session())
        );
    }

    #[tokio::test]
    async fn unapproved_origin_and_redirect_never_release_page_data() {
        let p = proxy("https://foreign.invalid/private");
        let sid = session(&p).await;
        assert_eq!(
            p.dispatch(Method::GET, &format!("/session/{sid}/source"), Value::Null)
                .await,
            Err(Failure::denied())
        );
        assert!(
            p.driver
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(_, route, _)| !route.ends_with("/source"))
        );
        *p.driver.current.lock().unwrap() = "https://www.reddit.com/message/inbox".into();
        p.driver.escape_after_data.store(true, Ordering::SeqCst);
        assert_eq!(
            p.dispatch(Method::GET, &format!("/session/{sid}/source"), Value::Null)
                .await,
            Err(Failure::denied())
        );
    }

    #[tokio::test]
    async fn no_driver_request_for_rejected_navigation_or_scripts() {
        let p = proxy("about:blank");
        let sid = session(&p).await;
        let before = p.driver.calls.lock().unwrap().len();
        for (route, body) in [
            (
                format!("/session/{sid}/url"),
                json!({"url":"https://www.reddit.com/logout"}),
            ),
            (
                format!("/session/{sid}/execute/sync"),
                json!({"script":"return 1;","args":[]}),
            ),
        ] {
            assert!(p.dispatch(Method::POST, &route, body).await.is_err());
        }
        assert_eq!(p.driver.calls.lock().unwrap().len(), before);
    }

    #[test]
    fn token_host_origin_and_query_checks_fail_closed() {
        let token = "a".repeat(64);
        let valid = Request::builder()
            .uri(format!("/{token}/status"))
            .header(header::HOST, LISTEN)
            .body(Body::empty())
            .unwrap();
        assert_eq!(authenticated_route(&valid, &token).unwrap(), "/status");
        for (uri, host, origin) in [
            (format!("/{}/status", "b".repeat(64)), LISTEN, None),
            (format!("/{token}/status"), "evil.invalid:9516", None),
            (
                format!("/{token}/status"),
                LISTEN,
                Some("https://www.reddit.com"),
            ),
            (format!("/{token}/status?query=yes"), LISTEN, None),
        ] {
            let mut request = Request::builder().uri(uri).header(header::HOST, host);
            if let Some(origin) = origin {
                request = request.header(header::ORIGIN, origin);
            }
            assert!(authenticated_route(&request.body(Body::empty()).unwrap(), &token).is_err());
        }
    }

    #[test]
    fn token_requires_private_regular_owned_file() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("token");
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        file.write_all("a".repeat(64).as_bytes()).unwrap();
        assert!(read_token(&path).is_ok());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_token(&path).is_err());
        let symlink = directory.path().join("symlink");
        std::os::unix::fs::symlink(&path, &symlink).unwrap();
        assert!(read_token(&symlink).is_err());
    }

    // This only starts a synthetic in-memory loopback server. No ChromeDriver,
    // CDP, live browser, website, credentials, or user files are accessed.
    #[tokio::test]
    async fn actual_fantoccini_native_action_routes_work_and_mutations_fail() {
        async fn fake_http(
            State(proxy): State<Arc<Proxy<MemoryDriver>>>,
            request: Request,
        ) -> Response {
            let method = request.method().clone();
            let route = request
                .uri()
                .path()
                .strip_prefix("/fixture")
                .unwrap_or("/denied")
                .to_owned();
            let bytes = to_bytes(request.into_body(), MAX_REQUEST).await.unwrap();
            let body = if bytes.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&bytes).unwrap()
            };
            match proxy.dispatch(method, &route, body).await {
                Ok(value) => response(StatusCode::OK, json!({"value":value})),
                Err(error) => error.response(),
            }
        }
        let proxy = Arc::new(proxy("about:blank"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().fallback(fake_http).with_state(proxy.clone());
        let server = zeroclaw_spawn::spawn!(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = fantoccini::ClientBuilder::rustls()
            .unwrap()
            .connect(&format!("http://{address}/fixture/"))
            .await
            .unwrap();
        client
            .goto("https://www.reddit.com/message/messages/")
            .await
            .unwrap();
        assert_eq!(
            client.current_url().await.unwrap().as_str(),
            "https://www.reddit.com/message/messages/"
        );
        assert_eq!(client.title().await.unwrap(), "Fixture inbox");
        let body = client.find(fantoccini::Locator::Css("body")).await.unwrap();
        assert_eq!(body.text().await.unwrap(), "Synthetic inbox fixture");
        assert_eq!(body.attr("role").await.unwrap().as_deref(), Some("main"));
        assert!(body.is_displayed().await.unwrap());
        assert_eq!(
            client.source().await.unwrap(),
            "<body>Synthetic inbox fixture</body>"
        );
        assert!(client.screenshot().await.is_err());
        assert!(
            client
                .execute("return document.body.innerText", vec![])
                .await
                .is_err()
        );
        assert!(body.click().await.is_err());
        assert!(body.send_keys("do not send").await.is_err());
        assert!(client.get_all_cookies().await.is_err());
        client.close().await.unwrap();
        assert!(
            proxy
                .driver
                .calls
                .lock()
                .unwrap()
                .iter()
                .all(|(method, _, _)| *method != Method::DELETE)
        );
        server.abort();
    }

    #[tokio::test]
    async fn crashed_client_lease_expires_without_new_driver_or_browser() {
        let p = proxy("about:blank");
        let first = session(&p).await;
        p.session.lock().await.last_used = Some(Instant::now() - Duration::from_secs(301));
        let next = session(&p).await;
        assert_ne!(first, next);
        assert_eq!(p.driver.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn restarted_driver_invalidates_cached_session_on_url_preflight() {
        let p = proxy("about:blank");
        let first = session(&p).await;
        p.driver.invalid_next.store(true, Ordering::SeqCst);
        assert_eq!(
            p.dispatch(Method::GET, &format!("/session/{first}/url"), Value::Null)
                .await,
            Err(Failure::invalid_session())
        );
        session(&p).await;
        assert_eq!(
            p.driver
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, route, _)| route == "/session")
                .count(),
            2
        );
    }
}
