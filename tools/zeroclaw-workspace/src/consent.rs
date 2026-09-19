//! Isolated installed-app OAuth; never updates the existing Gmail/Calendar grant.
use crate::state::State;
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{IsTerminal, Write},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use zeroize::Zeroizing;
pub const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
pub const REQUIRED: &str =
    "owner incremental consent for drive.file required; no broad-scope fallback";
#[derive(Serialize, Deserialize)]
struct Credential {
    account: String,
    client_id: String,
    refresh_token: Zeroizing<String>,
}
#[derive(Deserialize)]
struct Tokens {
    access_token: Zeroizing<String>,
    refresh_token: Option<Zeroizing<String>>,
    scope: String,
    token_type: String,
}
pub fn terminal() -> Result<()> {
    ensure!(
        std::io::stdin().is_terminal()
            && std::io::stdout().is_terminal()
            && std::io::stderr().is_terminal(),
        "interactive doctor requires an owner-operated terminal"
    );
    Ok(())
}
pub fn random() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| anyhow::Error::msg("OS randomness unavailable"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}
pub fn authorization_url(
    client: &str,
    account: &str,
    redirect: &str,
    state: &str,
    verifier: &str,
) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse("https://accounts.google.com/o/oauth2/v2/auth")?;
    url.query_pairs_mut().extend_pairs([
        ("client_id", client),
        ("redirect_uri", redirect),
        ("response_type", "code"),
        ("scope", SCOPE),
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("include_granted_scopes", "false"),
        ("login_hint", account),
        ("state", state),
        (
            "code_challenge",
            &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes())),
        ),
        ("code_challenge_method", "S256"),
    ]);
    Ok(url)
}
pub fn callback(request: &str, state: &str, port: u16) -> Result<Zeroizing<String>> {
    let line = request.lines().next().context("OAuth callback missing")?;
    let parts: Vec<_> = line.split_whitespace().collect();
    ensure!(
        parts.len() == 3
            && parts[0] == "GET"
            && parts[2] == "HTTP/1.1"
            && parts[1].starts_with("/oauth?")
            && parts[1].len() < 8192,
        "invalid OAuth callback"
    );
    ensure!(
        request
            .lines()
            .any(|v| v.eq_ignore_ascii_case(&format!("Host: 127.0.0.1:{port}"))),
        "invalid callback host"
    );
    let url = reqwest::Url::parse(&format!("http://127.0.0.1:{}{}", port, parts[1]))?;
    let pairs: Vec<_> = url.query_pairs().collect();
    let single = |key: &str| -> Result<String> {
        let values: Vec<_> = pairs.iter().filter(|(k, _)| k == key).collect();
        ensure!(values.len() == 1, "invalid OAuth callback parameters");
        Ok(values[0].1.to_string())
    };
    ensure!(single("state")? == state, "OAuth state mismatch");
    ensure!(
        !pairs.iter().any(|(k, _)| k == "error"),
        "owner cancelled OAuth consent"
    );
    let code = single("code")?;
    ensure!(
        !code.is_empty() && !code.chars().any(char::is_control),
        "invalid OAuth code"
    );
    Ok(Zeroizing::new(code))
}
async fn tokens(response: reqwest::Response) -> Result<Tokens> {
    let bytes = crate::api::response_bytes(response, 64 * 1024).await?;
    let value: Tokens =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::Error::msg("invalid OAuth response"))?;
    validate_tokens(&value)?;
    Ok(value)
}
fn validate_tokens(value: &Tokens) -> Result<()> {
    ensure!(
        value.scope.split_whitespace().collect::<Vec<_>>() == [SCOPE]
            && value.token_type.eq_ignore_ascii_case("bearer"),
        "OAuth must grant exactly drive.file"
    );
    for token in std::iter::once(&value.access_token).chain(value.refresh_token.iter()) {
        ensure!(
            !token.is_empty() && !token.chars().any(char::is_control),
            "invalid OAuth credential"
        );
    }
    Ok(())
}
fn consent_error(error: anyhow::Error) -> anyhow::Error {
    if error.to_string() == REQUIRED {
        anyhow::Error::msg(format!(
            "{REQUIRED}; owner: run zeroclaw-signed-launch workspace doctor --interactive in Terminal to authorize only drive.file"
        ))
    } else {
        error
    }
}

pub async fn access(
    client: &reqwest::Client,
    account: &str,
    interactive: bool,
) -> Result<(Zeroizing<String>, String)> {
    if interactive {
        terminal()?;
    }
    let (client_id, secret) = zeroclaw_gmail::auth::workspace_client(interactive)?;
    let state = State::open(&zeroclaw_gmail::auth::root()?)?;
    let saved: Option<Credential> = state.read("oauth")?;
    if let Some(mut saved) = saved {
        ensure!(
            saved.account.eq_ignore_ascii_case(account) && saved.client_id == client_id,
            "Workspace credential account/client mismatch"
        );
        let response = client
            .post("https://oauth2.googleapis.com/token")
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", saved.refresh_token.as_str()),
                ("client_id", &client_id),
                ("client_secret", secret.as_str()),
                ("scope", SCOPE),
            ])
            .send()
            .await
            .map_err(|_| anyhow::Error::msg("OAuth refresh transport failed"))?;
        let token = tokens(response).await?;
        crate::api::verify_account(client, &token.access_token, account).await?;
        if let Some(refresh) = token.refresh_token {
            saved.refresh_token = refresh;
            state.save("oauth", &saved)?;
        }
        return Ok((token.access_token, client_id));
    }
    // Keep read compatibility for a canonical grant which already has the scope.
    if !interactive {
        let token = zeroclaw_gmail::auth::workspace_access_token(client, account, false)
            .await
            .map_err(consent_error)?;
        return Ok((token, client_id));
    }
    eprintln!(
        "Authorize isolated Workspace access to exactly {SCOPE}. Existing Gmail/Calendar credentials are preserved. This does not authorize document writes. Type AUTHORIZE to continue (anything else cancels):"
    );
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    ensure!(
        answer.trim() == "AUTHORIZE",
        "owner cancelled OAuth consent"
    );
    crate::owner::confirm(
        "Authorize Workspace OAuth for only drive.file. This does not authorize document writes.",
    )?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}/oauth");
    let nonce = random()?;
    let verifier = Zeroizing::new(random()?);
    let url = authorization_url(&client_id, account, &redirect, &nonce, &verifier)?;
    eprintln!(
        "Open this Google authorization URL in your browser. Select the configured account and allow only per-file Drive access. Expires in 180 seconds:\n{url}"
    );
    std::io::stderr().flush()?;
    let code = tokio::time::timeout(Duration::from_secs(180), async {
        let (mut socket,peer)=listener.accept().await?; ensure!(peer.ip().is_loopback(),"non-loopback callback denied");
        let mut bytes=Zeroizing::new(Vec::new());
        loop { let byte=socket.read_u8().await?; bytes.push(byte); ensure!(bytes.len()<=8192,"OAuth callback too large"); if bytes.ends_with(b"\r\n\r\n") {break;} }
        let request=std::str::from_utf8(&bytes).map_err(|_|anyhow::Error::msg("invalid OAuth callback"))?;
        let result=callback(request,&nonce,port);
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: 35\r\n\r\nReturn to Terminal for the outcome.\n").await?;
        result
    }).await.map_err(|_|anyhow::Error::msg("OAuth consent timed out; no credential saved"))??;
    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", &redirect),
            ("client_id", &client_id),
            ("client_secret", secret.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await
        .map_err(|_| {
            anyhow::Error::msg("OAuth exchange transport failed; rerun interactive doctor")
        })?;
    let token = tokens(response).await?;
    crate::api::verify_account(client, &token.access_token, account).await?;
    let refresh = token
        .refresh_token
        .context("Google did not return an offline credential; no credential saved")?;
    state.save(
        "oauth",
        &Credential {
            account: account.to_owned(),
            client_id: client_id.clone(),
            refresh_token: refresh,
        },
    )?;
    Ok((token.access_token, client_id))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_scope_regression_includes_bounded_owner_next_step() {
        let error = consent_error(anyhow::Error::msg(REQUIRED)).to_string();
        assert!(error.starts_with(REQUIRED));
        assert!(error.contains("zeroclaw-signed-launch workspace doctor --interactive"));
        let denied = consent_error(anyhow::Error::msg("Keychain denied"));
        assert_eq!(denied.to_string(), "Keychain denied");
    }
    #[test]
    fn exact_consent_request_and_callback() -> Result<()> {
        let u = authorization_url(
            "client",
            "owner@example.invalid",
            "http://127.0.0.1:1234/oauth",
            "state",
            "verifier",
        )?;
        let q: std::collections::HashMap<_, _> = u.query_pairs().collect();
        assert_eq!(q["scope"], SCOPE);
        assert_eq!(q["include_granted_scopes"], "false");
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(
            callback(
                "GET /oauth?state=state&code=secret HTTP/1.1\r\nHost: 127.0.0.1:1234\r\n\r\n",
                "state",
                1234
            )
            .is_ok()
        );
        for path in [
            "state=bad&code=secret",
            "state=state&code=a&code=b",
            "state=state&error=access_denied",
        ] {
            assert!(
                callback(
                    &format!("GET /oauth?{path} HTTP/1.1\r\nHost: 127.0.0.1:1234\r\n\r\n"),
                    "state",
                    1234
                )
                .is_err()
            );
        }
        Ok(())
    }
    #[test]
    fn broad_or_missing_scope_rejected() {
        for scope in [
            "",
            "https://www.googleapis.com/auth/drive",
            "https://www.googleapis.com/auth/drive.file openid",
        ] {
            assert!(
                validate_tokens(&Tokens {
                    access_token: Zeroizing::new("fixture".into()),
                    refresh_token: None,
                    scope: scope.into(),
                    token_type: "Bearer".into()
                })
                .is_err()
            );
        }
    }
}
