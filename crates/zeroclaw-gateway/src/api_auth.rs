//! Fixed, credential-free OAuth maintenance for native local companions.

use crate::AppState;
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use std::{
    future::Future,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::sync::Semaphore;
use zeroclaw_config::schema::{Config, WireApi};
use zeroclaw_providers::auth::{AuthService, NativeOpenAiFreshness};

pub(crate) const ENSURE_FRESH_PATH: &str = "/api/auth/openai-codex/zeroclaw-native/ensure-fresh";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Status {
    Ready,
    Pending,
    ReauthRequired,
    Unavailable,
    Deferred,
}

impl Status {
    fn response(self) -> Response {
        let (code, status) = match self {
            Self::Ready => (StatusCode::OK, "ready"),
            Self::Pending => (StatusCode::ACCEPTED, "pending"),
            Self::ReauthRequired => (StatusCode::CONFLICT, "reauth_required"),
            Self::Unavailable => (StatusCode::BAD_REQUEST, "unavailable"),
            Self::Deferred => (StatusCode::SERVICE_UNAVAILABLE, "deferred"),
        };
        (
            code,
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({"status": status})),
        )
            .into_response()
    }
}

fn configured_native_auth(config: &Config) -> bool {
    config.secrets.encrypt
        && config
            .providers
            .models
            .openai
            .get("sol")
            .is_some_and(|provider| {
                let provider = &provider.base;
                provider.requires_openai_auth
                    && provider.wire_api == Some(WireApi::Responses)
                    && provider.api_key.is_none()
                    && provider.uri.is_none()
                    && provider.kind.is_none()
                    && provider
                        .model
                        .as_ref()
                        .is_some_and(|model| !model.trim().is_empty())
            })
}

/// Keep the accepted job independent of the handler/HTTP timeout. The permit
/// lives inside the job, not the waiting request, so disconnects cannot create
/// another refresh or interrupt persistence after a successful rotation.
async fn single_flight<F>(slot: Arc<Semaphore>, work: F, response_wait: Duration) -> Status
where
    F: Future<Output = Status> + Send + 'static,
{
    let Ok(permit) = slot.try_acquire_owned() else {
        return Status::Pending;
    };
    let mut job = zeroclaw_spawn::spawn!(async move {
        let _permit = permit;
        work.await
    });
    match tokio::time::timeout(response_wait, &mut job).await {
        Ok(Ok(status)) => status,
        Ok(Err(_)) => Status::Deferred,
        Err(_) => Status::Pending,
    }
}

pub(crate) async fn handle_ensure_fresh(
    State(state): State<AppState>,
    request: Request,
) -> Response {
    // Unlike ordinary API reads this maintenance action is never open when
    // pairing is disabled. No body/config/profile/endpoint override is accepted.
    let token = crate::api::extract_bearer_token(request.headers()).unwrap_or("");
    if !state.pairing.require_pairing()
        || token.is_empty()
        || !state.pairing.is_authenticated(token)
    {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({"status": "unavailable"})),
        )
            .into_response();
    }
    if !request
        .extensions()
        .get::<ConnectInfo<std::net::SocketAddr>>()
        .is_some_and(|peer| peer.0.ip().is_loopback())
    {
        return (
            StatusCode::FORBIDDEN,
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({"status":"unavailable"})),
        )
            .into_response();
    }
    if request.uri().query().is_some() || state.reload_tx.is_none() {
        return Status::Unavailable.response();
    }
    let empty = tokio::time::timeout(
        Duration::from_secs(1),
        axum::body::to_bytes(request.into_body(), 0),
    )
    .await;
    if !matches!(empty, Ok(Ok(ref bytes)) if bytes.is_empty()) {
        return Status::Unavailable.response();
    }
    let service = {
        let config = state.config.read();
        if !configured_native_auth(&config) {
            return Status::Unavailable.response();
        }
        AuthService::from_config(&config)
    };
    // This endpoint has exactly one native profile and one maintenance slot for
    // the entire daemon, including a gateway hot reload. No request queue.
    static SLOT: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let slot = SLOT.get_or_init(|| Arc::new(Semaphore::new(1))).clone();
    single_flight(
        slot,
        async move {
            match service.ensure_native_openai_fresh().await {
                NativeOpenAiFreshness::Ready => Status::Ready,
                NativeOpenAiFreshness::ReauthRequired => Status::ReauthRequired,
                NativeOpenAiFreshness::Deferred => Status::Deferred,
            }
        },
        Duration::from_secs(4),
    )
    .await
    .response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request, routing::post};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt;
    use zeroclaw_config::schema::{ModelProviderConfig, OpenAIModelProviderConfig};
    use zeroclaw_runtime::security::pairing::PairingGuard;

    const TOKEN: &str = "native-maintenance-test-bearer";

    fn state(path: &std::path::Path) -> AppState {
        let mut config = Config {
            config_path: path.join("config.toml"),
            ..Config::default()
        };
        config.secrets.encrypt = true;
        config.providers.models.openai.insert(
            "sol".into(),
            OpenAIModelProviderConfig {
                base: ModelProviderConfig {
                    requires_openai_auth: true,
                    wire_api: Some(WireApi::Responses),
                    model: Some("synthetic-model".into()),
                    ..Default::default()
                },
            },
        );
        let mut state = crate::api::tests::test_state(config);
        state.pairing = Arc::new(PairingGuard::new(true, &[TOKEN.into()]));
        state.reload_tx = Some(tokio::sync::watch::channel(false).0);
        state
    }

    async fn request(
        state: AppState,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &str,
    ) -> (StatusCode, serde_json::Value) {
        let router = Router::new()
            .route(ENSURE_FRESH_PATH, post(handle_ensure_fresh))
            .with_state(state);
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .extension(ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                40000,
            ))));
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let response = router
            .oneshot(request.body(Body::from(body.to_owned())).unwrap())
            .await
            .unwrap();
        let code = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(TOKEN));
        (
            code,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    #[tokio::test]
    async fn ensure_fresh_rejects_unauthorized_and_pairing_disabled_before_store_access() {
        let dir = tempfile::tempdir().unwrap();
        for token in [None, Some(""), Some("wrong")] {
            let (code, value) =
                request(state(dir.path()), "POST", ENSURE_FRESH_PATH, token, "").await;
            assert_eq!(code, StatusCode::UNAUTHORIZED);
            assert_eq!(value, serde_json::json!({"status":"unavailable"}));
        }
        let mut disabled = state(dir.path());
        disabled.pairing = Arc::new(PairingGuard::new(false, &[TOKEN.into()]));
        assert_eq!(
            request(disabled, "POST", ENSURE_FRESH_PATH, Some(TOKEN), "")
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn ensure_fresh_rejects_overrides_nonempty_body_and_non_post() {
        let dir = tempfile::tempdir().unwrap();
        for (path, body) in [
            (format!("{ENSURE_FRESH_PATH}?profile=default"), ""),
            (ENSURE_FRESH_PATH.into(), "{}"),
            (ENSURE_FRESH_PATH.into(), " "),
        ] {
            assert_eq!(
                request(state(dir.path()), "POST", &path, Some(TOKEN), body)
                    .await
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            request(state(dir.path()), "GET", ENSURE_FRESH_PATH, Some(TOKEN), "")
                .await
                .0,
            StatusCode::METHOD_NOT_ALLOWED
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn ensure_fresh_requires_native_configuration_without_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let config = state(dir.path()).config.read().clone();
        assert!(configured_native_auth(&config));
        for field in ["requires_openai_auth", "api_key", "uri", "kind", "wire_api"] {
            let mut config = config.clone();
            let provider = config.providers.models.openai.get_mut("sol").unwrap();
            let provider = &mut provider.base;
            match field {
                "requires_openai_auth" => provider.requires_openai_auth = false,
                "api_key" => provider.api_key = Some("synthetic-secret".into()),
                "uri" => provider.uri = Some("http://invalid.test".into()),
                "kind" => provider.kind = Some("openai-compatible".into()),
                _ => provider.wire_api = None,
            }
            assert!(!configured_native_auth(&config));
        }
    }

    #[tokio::test]
    async fn ensure_fresh_configuration_and_daemon_gates_precede_store_access() {
        let dir = tempfile::tempdir().unwrap();
        let mut standalone = state(dir.path());
        standalone.reload_tx = None;
        assert_eq!(
            request(standalone, "POST", ENSURE_FRESH_PATH, Some(TOKEN), "")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        let disabled = state(dir.path());
        disabled.config.write().providers.models.openai.clear();
        assert_eq!(
            request(disabled, "POST", ENSURE_FRESH_PATH, Some(TOKEN), "")
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn ensure_fresh_returns_static_reauth_for_missing_exact_profile() {
        let dir = tempfile::tempdir().unwrap();
        for _ in 0..20 {
            let (code, value) = request(
                state(dir.path()),
                "POST",
                ENSURE_FRESH_PATH,
                Some(TOKEN),
                "",
            )
            .await;
            if code == StatusCode::ACCEPTED {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            assert_eq!(code, StatusCode::CONFLICT);
            assert_eq!(value, serde_json::json!({"status":"reauth_required"}));
            return;
        }
        panic!("synthetic native maintenance slot did not become available");
    }

    #[tokio::test]
    async fn ensure_fresh_rejects_nonlocal_and_missing_peer_even_with_forwarded_loopback() {
        let dir = tempfile::tempdir().unwrap();
        for peer in [
            None,
            Some(std::net::SocketAddr::from(([203, 0, 113, 1], 40000))),
        ] {
            let router = Router::new()
                .route(ENSURE_FRESH_PATH, post(handle_ensure_fresh))
                .with_state(state(dir.path()));
            let mut request = Request::builder()
                .method("POST")
                .uri(ENSURE_FRESH_PATH)
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .header("X-Forwarded-For", "127.0.0.1");
            if let Some(peer) = peer {
                request = request.extension(ConnectInfo(peer));
            }
            let response = router
                .oneshot(request.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN);
        }
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn ensure_fresh_axum_serve_injects_real_loopback_connection_context() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = tempfile::tempdir().unwrap();
        let router = Router::new()
            .route(ENSURE_FRESH_PATH, post(handle_ensure_fresh))
            .with_state(state(dir.path()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = zeroclaw_spawn::spawn!(async move {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        let wire = format!(
            "POST {ENSURE_FRESH_PATH} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {TOKEN}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(wire.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(
            Duration::from_secs(6),
            stream.take(1024).read_to_end(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
        let _ = server.await;
        let response = String::from_utf8(response).unwrap();
        assert!(!response.contains("403 Forbidden"));
        assert!(response.contains("409 Conflict") || response.contains("202 Accepted"));
        assert!(!response.contains(TOKEN));
    }

    #[tokio::test]
    async fn ensure_fresh_ready_uses_native_saved_profile_without_returning_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let state = state(dir.path());
        let service = AuthService::from_config(&state.config.read());
        service
            .store_openai_tokens(
                "zeroclaw-native",
                zeroclaw_providers::auth::profiles::TokenSet {
                    access_token: "synthetic-private-access".into(),
                    refresh_token: Some("synthetic-private-refresh".into()),
                    id_token: Some("synthetic-private-id".into()),
                    expires_at: Some(chrono::Utc::now() + chrono::Duration::hours(1)),
                    token_type: Some("Bearer".into()),
                    scope: None,
                },
                Some("synthetic-account".into()),
                true,
            )
            .await
            .unwrap();
        // Gateway tests can overlap on the intentionally process-wide slot.
        for _ in 0..20 {
            let (code, value) =
                request(state.clone(), "POST", ENSURE_FRESH_PATH, Some(TOKEN), "").await;
            if code == StatusCode::ACCEPTED {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            }
            assert_eq!(code, StatusCode::OK);
            assert_eq!(value, serde_json::json!({"status":"ready"}));
            return;
        }
        panic!("synthetic native maintenance slot did not become available");
    }

    #[tokio::test]
    async fn ensure_fresh_single_flight_survives_request_cancellation() {
        let slot = Arc::new(Semaphore::new(1));
        let completed = Arc::new(AtomicUsize::new(0));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let work_completed = completed.clone();
        let request_slot = slot.clone();
        let waiter = zeroclaw_spawn::spawn!(single_flight(
            request_slot,
            async move {
                started_tx.send(()).unwrap();
                finish_rx.await.unwrap();
                work_completed.fetch_add(1, Ordering::SeqCst);
                Status::Ready
            },
            Duration::from_secs(30)
        ));
        started_rx.await.unwrap();
        waiter.abort();
        let _ = waiter.await;
        let overlap = single_flight(
            slot.clone(),
            async { panic!("overlap must not run") },
            Duration::ZERO,
        )
        .await;
        assert_eq!(overlap, Status::Pending);
        finish_tx.send(()).unwrap();
        let _permit = tokio::time::timeout(Duration::from_secs(1), slot.acquire())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ensure_fresh_pending_timeout_does_not_cancel_work() {
        let slot = Arc::new(Semaphore::new(1));
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let result = single_flight(
            slot.clone(),
            async move {
                finish_rx.await.unwrap();
                Status::Ready
            },
            Duration::from_millis(1),
        )
        .await;
        assert_eq!(result, Status::Pending);
        assert_eq!(slot.available_permits(), 0);
        finish_tx.send(()).unwrap();
        let _permit = tokio::time::timeout(Duration::from_secs(1), slot.acquire())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn ensure_fresh_all_outcomes_are_static_and_noncacheable() {
        for status in [
            Status::Ready,
            Status::Pending,
            Status::ReauthRequired,
            Status::Unavailable,
            Status::Deferred,
        ] {
            let response = status.response();
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let bytes = axum::body::to_bytes(response.into_body(), 64)
                .await
                .unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value.as_object().unwrap().len(), 1);
            assert!(value["status"].is_string());
        }
    }
}
