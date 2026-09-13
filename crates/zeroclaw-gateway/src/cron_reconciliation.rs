//! Paired local operator surface; native CLI is available without API credentials.
//! Never registered as a model tool or enabled by a job's tool allowlist.
use super::AppState;
use axum::{
    Json,
    extract::{ConnectInfo, Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use std::net::SocketAddr;
use zeroclaw_runtime::cron::{self, ReconciliationRequest};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StatusQuery {
    occurrence_id: String,
}

fn operator_allowed(peer: SocketAddr, headers: &HeaderMap) -> bool {
    // No remote-admin opt-in, forwarded-IP trust, browser/CSRF access, or
    // anonymous pairing-disabled fallback. Privileged local maintenance only,
    // the same OS-user trust boundary as /admin/reload and the native CLI.
    peer.ip().is_loopback()
        && !headers.contains_key("origin")
        && !headers.contains_key("sec-fetch-site")
        && headers
            .get("x-zeroclaw-operator")
            .and_then(|v| v.to_str().ok())
            == Some("cron-reconcile")
}

fn paired_operator(pairing: &zeroclaw_config::pairing::PairingGuard, headers: &HeaderMap) -> bool {
    // Fail closed even when ordinary dashboard pairing is disabled. A loopback
    // reverse proxy must not turn a remote caller into a local operator.
    pairing.require_pairing()
        && super::api::extract_bearer_token(headers)
            .is_some_and(|token| !token.is_empty() && pairing.is_authenticated(token))
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(serde_json::json!({"error":
            zeroclaw_runtime::i18n::get_required_cli_string("cron-reconcile-rejected")
        })),
    )
        .into_response()
}

pub(crate) async fn status(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((job_id, run_id)): Path<(String, i64)>,
    Query(query): Query<StatusQuery>,
) -> Response {
    if !operator_allowed(peer, &headers) || !paired_operator(&state.pairing, &headers) {
        return forbidden();
    }
    let config = state.config.read().clone();
    match tokio::task::spawn_blocking(move || {
        cron::reconciliation_status(&config, &job_id, run_id, &query.occurrence_id)
    })
    .await
    {
        Ok(Ok(status)) => Json(status).into_response(),
        Ok(Err(_)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":
                zeroclaw_runtime::i18n::get_required_cli_string("cron-reconcile-rejected")
            })),
        )
            .into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

pub(crate) async fn reconcile(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Path((job_id, run_id)): Path<(String, i64)>,
    Json(request): Json<ReconciliationRequest>,
) -> Response {
    if !operator_allowed(peer, &headers) || !paired_operator(&state.pairing, &headers) {
        return forbidden();
    }
    let config = state.config.read().clone();
    match tokio::task::spawn_blocking(move || {
        cron::reconcile_no_external_effect(
            &config,
            &job_id,
            run_id,
            &request,
            "local_operator:paired_loopback_admin",
        )
    })
    .await
    {
        Ok(Ok(receipt)) => Json(receipt).into_response(),
        Ok(Err(_)) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":
                zeroclaw_runtime::i18n::get_required_cli_string("cron-reconcile-rejected")
            })),
        )
            .into_response(),
        Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn local_operator_boundary_rejects_remote_browser_and_missing_intent() {
        let local = "127.0.0.1:1234".parse().unwrap();
        let mut headers = HeaderMap::new();
        assert!(!operator_allowed(local, &headers));
        headers.insert("x-zeroclaw-operator", "cron-reconcile".parse().unwrap());
        assert!(operator_allowed(local, &headers));
        assert!(!operator_allowed(
            "192.0.2.1:1234".parse().unwrap(),
            &headers
        ));
        headers.insert("origin", "http://localhost".parse().unwrap());
        assert!(!operator_allowed(local, &headers));
        headers.remove("origin");
        headers.insert("sec-fetch-site", "same-origin".parse().unwrap());
        assert!(!operator_allowed(local, &headers));
    }
    #[test]
    fn pairing_is_required_even_when_dashboard_pairing_is_disabled() {
        use zeroclaw_config::pairing::PairingGuard;
        let token = "test-operator-token".to_string();
        let mut headers = HeaderMap::new();
        let pairing = PairingGuard::new(true, std::slice::from_ref(&token));
        assert!(!paired_operator(&pairing, &headers));
        headers.insert("authorization", "Bearer wrong-token".parse().unwrap());
        assert!(!paired_operator(&pairing, &headers));
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        assert!(paired_operator(&pairing, &headers));
        assert!(!paired_operator(
            &PairingGuard::new(false, &[token]),
            &headers
        ));
    }

    #[test]
    fn schema_rejects_uncertain_dispositions_and_forged_sources() {
        let mut v = serde_json::json!({"occurrence_id":"manual:key:example", "expected_state":"0".repeat(64), "disposition":"no_external_effect", "evidence":"operator assertion"});
        assert!(serde_json::from_value::<ReconciliationRequest>(v.clone()).is_ok());
        v["source"] = "main".into();
        assert!(serde_json::from_value::<ReconciliationRequest>(v.clone()).is_err());
        v.as_object_mut().unwrap().remove("source");
        v["disposition"] = "uncertain_effect".into();
        assert!(serde_json::from_value::<ReconciliationRequest>(v).is_err());
    }
    #[test]
    fn operator_api_is_registered_without_model_tool_exposure() {
        let routes = include_str!("lib.rs");
        assert!(routes.contains("/admin/cron/{id}/runs/{run_id}/reconciliation"));
        let tools = include_str!("../../zeroclaw-runtime/src/tools/mod.rs");
        assert!(!tools.contains("cron_reconcile"));
    }
}
