use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::path::PathBuf;
use zeroize::Zeroizing;

pub fn root() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("ZEROCLAW_CONFIG_DIR") {
        let p = PathBuf::from(p);
        ensure!(p.is_absolute(), "configuration root must be absolute");
        return Ok(p);
    }
    Ok(PathBuf::from(std::env::var_os("HOME").context("HOME missing")?).join(".zeroclaw"))
}

/// Resolve the pinned identity and sharing policy from canonical configuration at
/// use time, never from model arguments or email content.
pub fn configuration() -> Result<(String, Vec<PathBuf>)> {
    let value: toml::Value = toml::from_str(
        &std::fs::read_to_string(root()?.join("config.toml"))
            .map_err(|_| anyhow::Error::msg("configuration unavailable"))?,
    )
    .map_err(|_| anyhow::Error::msg("invalid runtime configuration"))?;
    let value = serde_json::to_value(value)?;
    let servers = value["mcp"]["servers"]
        .as_array()
        .context("MCP servers missing")?;
    let account = servers
        .iter()
        .find(|s| s["name"] == "google_write")
        .and_then(|s| s["env"]["GOG_ACCOUNT"].as_str())
        .context("Google writer account must be pinned")?;
    crate::model::mailbox(account)?;
    // Sharing roots are the existing operator-owned personal-ops policy.
    let policy = root()?.join("extensions/personal-ops/sharing.json");
    let policy: Value = serde_json::from_slice(
        &std::fs::read(policy)
            .map_err(|_| anyhow::Error::msg("operator file-sharing policy unavailable"))?,
    )?;
    let roots = policy["allowed_roots"]
        .as_array()
        .context("operator file-sharing roots missing")?
        .iter()
        .map(|v| {
            v.as_str()
                .map(PathBuf::from)
                .context("invalid sharing root")
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((account.to_owned(), roots))
}

#[cfg(target_os = "macos")]
fn stored_token(account: &str, interactive: bool) -> Result<Zeroizing<Vec<u8>>> {
    // Never trigger or approve a native permission dialog from a background tool.
    // A missing grant must be resolved by the owner, not by changing ACLs.
    let status = unsafe {
        security_framework_sys::keychain::SecKeychainSetUserInteractionAllowed(u8::from(
            interactive,
        ))
    };
    ensure!(status == 0, "cannot set Keychain interaction policy");
    let result = security_framework::passwords::get_generic_password(
        "gogcli",
        &format!("token:default:{account}"),
    );
    let reset =
        unsafe { security_framework_sys::keychain::SecKeychainSetUserInteractionAllowed(0) };
    ensure!(reset == 0, "cannot restore noninteractive Keychain policy");
    result.map(Zeroizing::new).map_err(|e| anyhow::Error::msg(format!("Gmail Keychain access unavailable (OSStatus {}); owner native approval/setup may be required", e.code())))
}
#[cfg(not(target_os = "macos"))]
fn stored_token(_: &str, _: bool) -> Result<Zeroizing<Vec<u8>>> {
    bail!("Gmail credential adapter is configured for macOS Keychain only")
}

/// No shell, credential output, Keychain writes, OAuth scope expansion, or stored
/// access-token cache. Existing refresh credentials are only sent to Google OAuth.
pub async fn access_token(client: &reqwest::Client, account: &str) -> Result<Zeroizing<String>> {
    access_token_with_interaction(client, account, false).await
}

/// Explicit operator-only setup. MCP never takes this path; a pipe cannot opt in
/// to native permission dialogs. The owner decides in macOS's own prompt.
pub async fn interactive_access_token(
    client: &reqwest::Client,
    account: &str,
) -> Result<Zeroizing<String>> {
    use std::io::IsTerminal;
    ensure!(
        std::io::stdin().is_terminal(),
        "interactive doctor requires an owner-operated terminal"
    );
    access_token_with_interaction(client, account, true).await
}

async fn access_token_with_interaction(
    client: &reqwest::Client,
    account: &str,
    interactive: bool,
) -> Result<Zeroizing<String>> {
    let bytes = stored_token(account, interactive)?;
    let token: Value = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::Error::msg("invalid stored OAuth record"))?;
    let refresh = Zeroizing::new(
        token["refresh_token"]
            .as_str()
            .context("refresh credential unavailable")?
            .to_owned(),
    );
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME missing")?);
    let path = home.join("Library/Application Support/gogcli/credentials.json");
    let creds = Zeroizing::new(
        std::fs::read(path)
            .map_err(|_| anyhow::Error::msg("OAuth client configuration unavailable"))?,
    );
    let creds: Value = serde_json::from_slice(&creds)
        .map_err(|_| anyhow::Error::msg("invalid OAuth client configuration"))?;
    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "refresh_token"),
            ("scope", "https://www.googleapis.com/auth/gmail.compose https://www.googleapis.com/auth/gmail.readonly"),
            ("refresh_token", refresh.as_str()),
            (
                "client_id",
                creds["client_id"]
                    .as_str()
                    .context("OAuth client ID missing")?,
            ),
            (
                "client_secret",
                creds["client_secret"]
                    .as_str()
                    .context("OAuth client secret missing")?,
            ),
        ])
        .send()
        .await
        .map_err(|_| anyhow::Error::msg("OAuth transport failed"))?;
    ensure!(
        response.status().is_success(),
        "OAuth renewal denied; owner reauthorization may be required"
    );
    let mut response = response;
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::Error::msg("OAuth response interrupted"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= 64 * 1024,
            "OAuth response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::Error::msg("invalid OAuth response"))?;
    validate_scopes(
        value["scope"]
            .as_str()
            .context("OAuth scope evidence missing; owner reauthorization required")?,
    )?;
    match value["access_token"].as_str() {
        Some(s) if !s.is_empty() => Ok(Zeroizing::new(s.to_owned())),
        _ => bail!("OAuth access credential missing"),
    }
}

/// Gmail offers no draft-only OAuth scope. Compose includes send permission,
/// which the transport independently denies. Never accept full mailbox grants.
pub fn validate_scopes(scopes: &str) -> Result<()> {
    let expected = [
        "https://www.googleapis.com/auth/gmail.compose",
        "https://www.googleapis.com/auth/gmail.readonly",
    ];
    let actual = scopes
        .split_whitespace()
        .collect::<std::collections::HashSet<_>>();
    ensure!(
        actual.len() == 2 && expected.iter().all(|s| actual.contains(s)),
        "OAuth scopes must be exactly gmail.compose and gmail.readonly; owner reauthorization required"
    );
    Ok(())
}
