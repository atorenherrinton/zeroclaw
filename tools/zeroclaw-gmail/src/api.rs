//! Fixed-origin, bounded, single-attempt transport. No generic URLs or write paths.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{future::Future, time::Duration};
use zeroize::Zeroizing;

pub trait Api {
    fn request(
        &mut self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> impl Future<Output = Result<Value>>;
}
#[derive(Debug)]
pub struct NotFound;
impl std::fmt::Display for NotFound {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("exact Gmail object not found")
    }
}
impl std::error::Error for NotFound {}
pub struct Gmail {
    client: reqwest::Client,
    token: Zeroizing<String>,
}
pub fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(45))
        .build()?)
}
impl Gmail {
    pub async fn connect(account: &str) -> Result<Self> {
        let client = client()?;
        let token = crate::auth::access_token(&client, account).await?;
        Self::verify_identity(client, token, account).await
    }
    pub async fn connect_interactive(account: &str) -> Result<Self> {
        let client = client()?;
        let token = crate::auth::interactive_access_token(&client, account).await?;
        Self::verify_identity(client, token, account).await
    }
    async fn verify_identity(
        client: reqwest::Client,
        token: Zeroizing<String>,
        account: &str,
    ) -> Result<Self> {
        let mut api = Self { client, token };
        let profile = api.request("GET", "profile", &[], None).await?;
        ensure!(
            profile["emailAddress"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case(account)),
            "OAuth identity does not match pinned account"
        );
        Ok(api)
    }
}
pub fn allowed(method: &str, path: &str) -> bool {
    let p: Vec<_> = path.split('/').collect();
    if p.iter().any(|s| crate::model::id(s).is_err()) {
        return false;
    }
    // Gmail's OAuth compose grant also permits send. The transport must not.
    match (method, p.as_slice()) {
        ("GET", ["profile" | "drafts"]) | ("POST", ["drafts"]) => true,
        ("GET", ["messages", message]) => *message != "send",
        ("GET" | "PUT" | "DELETE", ["drafts", draft]) => *draft != "send",
        _ => false,
    }
}
impl Api for Gmail {
    async fn request(
        &mut self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        ensure!(
            allowed(method, path),
            "Gmail method denied by draft-only transport"
        );
        let mut request = self
            .client
            .request(
                reqwest::Method::from_bytes(method.as_bytes())?,
                format!("https://gmail.googleapis.com/gmail/v1/users/me/{path}"),
            )
            .bearer_auth(self.token.as_str())
            .query(query);
        if let Some(body) = body {
            ensure!(
                serde_json::to_vec(&body)?.len() <= 24 * 1024 * 1024,
                "Gmail request exceeds bound"
            );
            request = request.json(&body);
        }
        let mut response = request.send().await.map_err(|_| {
            anyhow::Error::msg("Gmail transport failed; reconcile attempted writes")
        })?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(NotFound.into());
        }
        ensure!(
            response.status().is_success(),
            "Gmail request denied; reconcile attempted writes"
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| anyhow::Error::msg("Gmail response interrupted; reconcile writes"))?
        {
            ensure!(
                bytes.len() + chunk.len() <= 24 * 1024 * 1024,
                "Gmail response exceeds bound"
            );
            bytes.extend_from_slice(&chunk);
        }
        if bytes.is_empty() {
            return Ok(serde_json::json!({}));
        }
        serde_json::from_slice(&bytes).context("Gmail response invalid; reconcile writes")
    }
}
