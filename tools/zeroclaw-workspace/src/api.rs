//! Only internally constructed, typed operations can reach fixed Google origins.
use crate::model::Read;
use anyhow::{Result, ensure};
use serde_json::Value;
use std::future::Future;
use zeroize::Zeroizing;

pub trait Api {
    fn read(&mut self, request: &Read) -> impl Future<Output = Result<Value>>;
}
pub struct Workspace {
    client: reqwest::Client,
    token: Zeroizing<String>,
    pub account: String,
    pub client_id: String,
}
impl Workspace {
    pub async fn connect(interactive: bool) -> Result<Self> {
        let account = zeroclaw_gmail::auth::pinned_account()?;
        let client = zeroclaw_gmail::api::client()?;
        let (token, client_id) = crate::consent::access(&client, &account, interactive).await?;
        verify_account(&client, &token, &account).await?;
        Ok(Self {
            client,
            token,
            account,
            client_id,
        })
    }
    async fn send(&self, url: &str, query: &[(&str, String)]) -> Result<Value> {
        // Private method; every resource path comes from the typed match below.
        let req = self
            .client
            .get(url)
            .bearer_auth(self.token.as_str())
            .query(query);
        let mut res = req
            .send()
            .await
            .map_err(|_| anyhow::Error::msg("Workspace read transport failed"))?;
        ensure!(
            res.status().is_success(),
            "Workspace API denied read request (HTTP {})",
            res.status().as_u16()
        );
        let mut bytes = Vec::new();
        while let Some(chunk) = res
            .chunk()
            .await
            .map_err(|_| anyhow::Error::msg("Workspace response interrupted"))?
        {
            ensure!(
                bytes.len() + chunk.len() <= 4 * 1024 * 1024,
                "response exceeds bound"
            );
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).map_err(|_| anyhow::Error::msg("invalid Workspace response"))
    }
}
impl Api for Workspace {
    async fn read(&mut self, request: &Read) -> Result<Value> {
        let (url, query) = read_target(request)?;
        self.send(&url, &query).await
    }
}

/// Fixed-origin GET target, also exercised by hermetic wire-shape tests.
pub fn read_target(request: &Read) -> Result<(String, Vec<(&'static str, String)>)> {
    request.validate()?;
    Ok(match request {
        Read::DriveList {
            name_contains,
            page_token,
        } => {
            let mut q =
                "trashed = false and mimeType = 'application/vnd.google-apps.document'".to_string();
            if let Some(name) = name_contains {
                q.push_str(&format!(
                    " and name contains '{}'",
                    name.replace('\\', "\\\\").replace('\'', "\\'")
                ));
            }
            let mut query = vec![
                ("q", q),
                ("pageSize", "20".into()),
                (
                    "fields",
                    "nextPageToken,files(id,name,mimeType,modifiedTime,version,webViewLink)".into(),
                ),
            ];
            if let Some(token) = page_token {
                query.push(("pageToken", token.clone()));
            }
            ("https://www.googleapis.com/drive/v3/files".into(), query)
        }
        Read::DriveMetadata { file_id } => (
            format!("https://www.googleapis.com/drive/v3/files/{file_id}"),
            vec![(
                "fields",
                "id,name,mimeType,modifiedTime,version,webViewLink,trashed".into(),
            )],
        ),
        Read::DocsRead { document_id } | Read::DocsVerify { document_id, .. } => (
            format!("https://docs.googleapis.com/v1/documents/{document_id}"),
            vec![
                ("includeTabsContent", "true".into()),
                ("suggestionsViewMode", "SUGGESTIONS_INLINE".into()),
            ],
        ),
    })
}

pub async fn response_bytes(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Zeroizing<Vec<u8>>> {
    ensure!(
        response.status().is_success(),
        "Google request denied (HTTP {})",
        response.status().as_u16()
    );
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::Error::msg("Google response interrupted"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= limit,
            "Google response exceeds bound"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}
pub async fn verify_account(client: &reqwest::Client, token: &str, account: &str) -> Result<()> {
    let response = client
        .get("https://www.googleapis.com/drive/v3/about")
        .bearer_auth(token)
        .query(&[("fields", "user(emailAddress)")])
        .send()
        .await
        .map_err(|_| anyhow::Error::msg("account verification transport failed"))?;
    let value: Value = serde_json::from_slice(&response_bytes(response, 64 * 1024).await?)
        .map_err(|_| anyhow::Error::msg("invalid account response"))?;
    ensure!(
        value["user"]["emailAddress"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(account)),
        "OAuth account mismatch"
    );
    Ok(())
}
impl Workspace {
    pub(crate) async fn mutation(&self, url: &str, body: &Value) -> Result<Value> {
        // Only the Docs-only typed WriteApi implementation calls this method.
        let bytes = serde_json::to_vec(body)?;
        ensure!(bytes.len() <= 256 * 1024, "write body exceeds bound");
        let response = self
            .client
            .post(url)
            .bearer_auth(self.token.as_str())
            .json(body)
            .send()
            .await
            .map_err(|_| {
                anyhow::Error::msg("Workspace mutation uncertain; reconcile the same operation")
            })?;
        serde_json::from_slice(&response_bytes(response, 4 * 1024 * 1024).await?).map_err(|_| {
            anyhow::Error::msg("invalid mutation response; reconcile the same operation")
        })
    }
    pub(crate) async fn lookup(&self, marker: &str) -> Result<Value> {
        crate::model::id(marker)?;
        self.send("https://www.googleapis.com/drive/v3/files", &[("q",format!("trashed = false and mimeType = 'application/vnd.google-apps.document' and appProperties has {{ key='zeroclaw_intent' and value='{marker}' }}")),("pageSize","2".into()),("fields","nextPageToken,files(id,name,mimeType,appProperties,ownedByMe,trashed)".into())]).await
    }
}
