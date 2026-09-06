//! Private loopback dashboard and authenticated event ingress. The worker owns
//! no provider truth: it refreshes source projections and wakes durable queues.
use crate::Ops;
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use notify::Watcher;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
#[derive(Clone)]
struct AppState {
    root: PathBuf,
    key: Arc<String>,
    wake: mpsc::Sender<()>,
}
fn authorized(headers: &HeaderMap, key: &str) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "));
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            s.split(';')
                .map(str::trim)
                .find_map(|v| v.strip_prefix("zc_ops="))
        });
    bearer.or(cookie) == Some(key)
}
async fn home(
    State(state): State<AppState>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let mut response = Html(include_str!("../web/activity.html")).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("constant header"),
    );
    response
        .headers_mut()
        .insert("x-frame-options", "DENY".parse().expect("constant header"));
    response.headers_mut().insert(
        "referrer-policy",
        "no-referrer".parse().expect("constant header"),
    );
    if query.get("access").is_some_and(|s| s == state.key.as_str())
        && let Ok(value) = format!(
            "zc_ops={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=31536000",
            state.key
        )
        .parse()
    {
        response.headers_mut().insert(header::SET_COOKIE, value);
    }
    response
}
async fn activity(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !authorized(&headers, &state.key) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let result = tokio::task::spawn_blocking(move || Ops::open(&state.root)?.activity()).await;
    match result {
        Ok(Ok(v)) => {
            let mut r = Json(v).into_response();
            r.headers_mut().insert(
                header::CACHE_CONTROL,
                "no-store".parse().expect("constant header"),
            );
            r
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}
async fn event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(value): Json<Value>,
) -> Response {
    // External producers must use the bearer token, never a browser cookie.
    if headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        != Some(&format!("Bearer {}", state.key))
    {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let root = state.root.clone();
    let result = tokio::task::spawn_blocking(move || Ops::open(&root)?.event_ingest(&value)).await;
    match result {
        Ok(Ok(v)) => {
            let _ = state.wake.send(());
            Json(v).into_response()
        }
        _ => StatusCode::BAD_REQUEST.into_response(),
    }
}
pub fn access_key(root: &Path) -> Result<String> {
    let path = root.join("extensions/personal-ops/dashboard.key");
    crate::private_dir(path.parent().context("key directory")?)?;
    if !path.exists() {
        let key = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        crate::private_write(&path, key.as_bytes())?;
    }
    let key = std::fs::read_to_string(path)?;
    ensure!(
        key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid dashboard key"
    );
    Ok(key)
}
pub fn dashboard_url(root: &Path) -> Result<String> {
    Ok(format!(
        "http://127.0.0.1:42619/?access={}",
        access_key(root)?
    ))
}
fn worker(root: PathBuf, rx: mpsc::Receiver<()>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let ops = Ops::open(&root)?;
    let mut last_refresh = Instant::now() - Duration::from_secs(901);
    let mut last_probe = Instant::now() - Duration::from_secs(3601);
    let mut last_cron = Instant::now() - Duration::from_secs(61);
    ops.health_record("gmail_push","not_configured","No Gmail Pub/Sub watch installed; timed reconciliation remains active. Authenticated /events accepts configured producer notifications.")?;
    ops.health_record("calendar_push","not_configured","No externally reachable watch installed; timed reconciliation remains active. No public endpoint was opened.")?;
    ops.health_record("package_carriers","not_configured","Carrier API credentials are not configured. Uses attributed shipping emails and direct carrier tracking links.")?;
    loop {
        // Source events coalesce into a read refresh. Periodic reconciliation repairs
        // dropped upstream notifications; it does not replay writes.
        let wake = rx.recv_timeout(Duration::from_secs(30)).is_ok();
        while rx.try_recv().is_ok() {}
        runtime.block_on(async {
    if wake{let _=ops.event_ingest(&json!({"event_id":format!("filesystem:{}",chrono::Utc::now().timestamp()/30),"source":"runtime","kind":"reconcile"}));}
    if last_cron.elapsed()>=Duration::from_secs(60){
      if let Err(e)=ops.refresh_cron(){ops.snapshot("scheduled_jobs",&json!({}),Some(&e.to_string()))?;}

      last_cron=Instant::now();
    }
    if last_probe.elapsed()>=Duration::from_secs(3600) {if let Err(e)=ops.refresh_routing_health().await{ops.health_record("conversation_routing","temporary_outage",&e.to_string())?;}last_probe=Instant::now();}
    ops.reminder_due_events()?;
    ops.process_events().await?;
    if last_refresh.elapsed()>=Duration::from_secs(900){
      if let Err(e)=ops.refresh_google().await{ops.health_record("google","temporary_outage",&e.to_string())?;}
      if let Err(e)=ops.refresh_github().await{ops.health_record("github","temporary_outage",&e.to_string())?;}
      if let Err(e)=ops.shipment_discover().await{ops.snapshot("shipment_discovery",&json!({}),Some(&e.to_string()))?;}
      if let Err(e)=ops.refresh_reminders().await{ops.snapshot("overdue_reminders",&json!({}),Some(&e.to_string()))?;}
      last_refresh=Instant::now();
    }
    // Operator settings allow alerts only after installation verification.
    let settings=root.join("extensions/personal-ops/service.json");
    if let Ok(bytes)=std::fs::read(settings)&& serde_json::from_slice::<Value>(&bytes).is_ok_and(|v|v["alerts_enabled"]==true){ops.notify_exceptions().await?;}
    Ok::<_,anyhow::Error>(())
  }).unwrap_or_else(|e|eprintln!("operations_worker: {e}"));
    }
}
// Separate queue ownership prevents slow provider reads delaying scheduled sends.
fn dispatch_worker(root: PathBuf) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let ops = Ops::open(&root)?;
    loop {
        match runtime.block_on(ops.dispatch_operations()) {
            Ok(_) => ops.health_record("outbox_dispatch", "healthy", "durable queue checked")?,
            Err(e) => ops.health_record("outbox_dispatch", "temporary_outage", &e.to_string())?,
        }
        std::thread::sleep(Duration::from_secs(15));
    }
}
pub async fn serve(root: &Path) -> Result<()> {
    let key = Arc::new(access_key(root)?);
    let (wake, rx) = mpsc::channel();
    let dispatch_root = root.to_owned();
    std::thread::Builder::new()
        .name("operations-outbox".into())
        .spawn(move || {
            if let Err(error) = dispatch_worker(dispatch_root) {
                eprintln!("outbox_worker_stopped: {error}");
            }
        })?;
    let root_owned = root.to_owned();
    std::thread::Builder::new()
        .name("personal-operations".into())
        .spawn(move || {
            if let Err(e) = worker(root_owned, rx) {
                eprintln!("operations_worker_stopped: {e}");
            }
        })?;
    let notify_tx = wake.clone();
    let notify_root = root.to_owned();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event {
            if event
                .paths
                .iter()
                .any(|p| p.to_string_lossy().contains("reminders"))
                && let Ok(ops) = Ops::open(&notify_root)
            {
                let _=ops.event_ingest(&json!({"event_id":format!("reminders:{}",chrono::Utc::now().timestamp()/30),"source":"reminders","kind":"reminder_overdue"}));
            }
            let _ = notify_tx.send(());
        }
    })?;
    let cron = root.join("data/cron");
    if cron.exists() {
        watcher.watch(&cron, notify::RecursiveMode::NonRecursive)?;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let reminders = PathBuf::from(home)
            .join("Library/Group Containers/group.com.apple.reminders/Container_v1/Stores");
        if reminders.exists()
            && let Err(error) = watcher.watch(&reminders, notify::RecursiveMode::NonRecursive)
        {
            Ops::open(root)?.health_record(
                "reminders_events",
                "temporary_outage",
                &error.to_string(),
            )?;
        }
    }
    let app = Router::new()
        .route("/", get(home))
        .route("/api/activity", get(activity))
        .route("/events", post(event))
        .layer(axum::extract::DefaultBodyLimit::max(65536))
        .with_state(AppState {
            root: root.to_owned(),
            key,
            wake,
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:42619").await?;
    axum::serve(listener, app).await?;
    drop(watcher);
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn api_does_not_accept_missing_or_wrong_token() {
        let mut h = HeaderMap::new();
        assert!(!authorized(&h, "fixture"));
        h.insert(header::AUTHORIZATION, "Bearer wrong".parse().unwrap());
        assert!(!authorized(&h, "fixture"));
        h.insert(header::AUTHORIZATION, "Bearer fixture".parse().unwrap());
        assert!(authorized(&h, "fixture"));
    }
}
