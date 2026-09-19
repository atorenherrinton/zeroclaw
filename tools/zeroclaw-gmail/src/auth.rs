use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
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
    let account = pinned_account()?;
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
    Ok((account, roots))
}

/// Resolve the existing sole Google account without loading Gmail sharing policy.
pub fn pinned_account() -> Result<String> {
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
    Ok(account.to_owned())
}

const GOG_KEYCHAIN_SERVICE: &str = "gogcli";
const DEFAULT_CLIENT_SECRET_KEY: &str = "client/default/client-secret";
const COMPOSE_SCOPE: &str = "https://www.googleapis.com/auth/gmail.compose";
const READONLY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.readonly";
const MODIFY_SCOPE: &str = "https://www.googleapis.com/auth/gmail.modify";
const MAX_OAUTH_RESPONSE: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OAuthScopes {
    ComposeReadonly,
    Modify,
    DriveFile,
}

impl OAuthScopes {
    fn request(self) -> &'static str {
        match self {
            Self::ComposeReadonly => {
                "https://www.googleapis.com/auth/gmail.compose https://www.googleapis.com/auth/gmail.readonly"
            }
            Self::Modify => MODIFY_SCOPE,
            Self::DriveFile => "https://www.googleapis.com/auth/drive.file",
        }
    }
}

#[derive(Deserialize)]
struct StoredToken {
    refresh_token: Zeroizing<String>,
    scopes: Vec<String>,
}

fn refresh_credentials(bytes: &[u8]) -> Result<(Zeroizing<String>, OAuthScopes)> {
    let token: StoredToken = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::Error::msg("invalid stored OAuth record or scope evidence"))?;
    ensure!(
        !token.refresh_token.trim().is_empty()
            && !token.refresh_token.chars().any(char::is_control),
        "refresh credential unavailable or malformed"
    );
    // The canonical stored grant owns these literal scopes. A scope with fewer
    // capabilities is not necessarily a member of that grant.
    ensure!(
        token.scopes.iter().all(|scope| !scope.is_empty()
            && scope
                .bytes()
                .all(|b| (0x21..=0x7e).contains(&b) && b != b'"' && b != b'\\')),
        "stored OAuth scope evidence is malformed"
    );
    let contains = |scope: &str| token.scopes.iter().any(|value| value == scope);
    let scopes = if contains(COMPOSE_SCOPE) && contains(READONLY_SCOPE) {
        OAuthScopes::ComposeReadonly
    } else if contains(MODIFY_SCOPE) {
        OAuthScopes::Modify
    } else {
        bail!("stored OAuth grant lacks supported Gmail scopes")
    };
    Ok((token.refresh_token, scopes))
}

#[derive(Deserialize)]
struct ClientCredentials {
    client_id: String,
    #[serde(default)]
    client_secret: Zeroizing<String>,
}

fn client_credentials(
    metadata: &[u8],
    interactive: bool,
    read_secret: impl FnOnce(&str, &str, bool) -> Result<Zeroizing<Vec<u8>>>,
) -> Result<ClientCredentials> {
    // Match gog's metadata-first lookup. Malformed metadata must never trigger
    // a fallback read, and its parse error must not include credential content.
    let mut credentials: ClientCredentials = serde_json::from_slice(metadata)
        .map_err(|_| anyhow::Error::msg("invalid OAuth client configuration"))?;
    ensure!(
        !credentials.client_id.trim().is_empty()
            && !credentials.client_id.chars().any(char::is_control),
        "OAuth client ID missing or invalid"
    );
    if !credentials.client_secret.trim().is_empty() {
        ensure!(
            !credentials.client_secret.chars().any(char::is_control),
            "OAuth client secret is malformed"
        );
        return Ok(credentials);
    }
    let bytes = read_secret(GOG_KEYCHAIN_SERVICE, DEFAULT_CLIENT_SECRET_KEY, interactive)
        .map_err(|_| anyhow::Error::msg("OAuth client secret Keychain access unavailable; owner native approval/setup may be required"))?;
    let secret = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow::Error::msg("OAuth client secret is not valid UTF-8"))?;
    credentials.client_secret = normalized_client_secret(secret)?;
    Ok(credentials)
}

fn normalized_client_secret(secret: &str) -> Result<Zeroizing<String>> {
    let secret = secret.trim();
    ensure!(
        !secret.is_empty() && !secret.chars().any(char::is_control),
        "OAuth client secret is empty or malformed"
    );
    Ok(Zeroizing::new(secret.to_owned()))
}

#[cfg(any(target_os = "macos", test))]
fn with_keychain_interaction(
    interactive: bool,
    mut set_interaction: impl FnMut(bool) -> Result<()>,
    read: impl FnOnce() -> Result<Zeroizing<Vec<u8>>>,
) -> Result<Zeroizing<Vec<u8>>> {
    set_interaction(interactive)?;
    let result = read();
    // Reset before returning either the secret or an access error. Wrapping the
    // bytes inside read also erases them if restoring this policy fails.
    set_interaction(false)?;
    result
}

#[cfg(target_os = "macos")]
fn keychain_record(service: &str, key: &str, interactive: bool) -> Result<Zeroizing<Vec<u8>>> {
    with_keychain_interaction(
        interactive,
        |allowed| {
            let status = unsafe {
                security_framework_sys::keychain::SecKeychainSetUserInteractionAllowed(u8::from(
                    allowed,
                ))
            };
            ensure!(status == 0, "cannot set Keychain interaction policy");
            Ok(())
        },
        || {
            security_framework::passwords::get_generic_password(service, key)
                .map(Zeroizing::new)
                .map_err(|e| anyhow::Error::msg(format!("Gmail Keychain access unavailable (OSStatus {}); owner native approval/setup may be required", e.code())))
        },
    )
}
#[cfg(not(target_os = "macos"))]
fn keychain_record(_: &str, _: &str, _: bool) -> Result<Zeroizing<Vec<u8>>> {
    bail!("Gmail credential adapter is configured for macOS Keychain only")
}

fn stored_token(account: &str, interactive: bool) -> Result<Zeroizing<Vec<u8>>> {
    keychain_record(
        GOG_KEYCHAIN_SERVICE,
        &format!("token:default:{account}"),
        interactive,
    )
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
    let (refresh, scopes) = refresh_credentials(&bytes)?;
    renew(client, refresh, scopes, interactive).await
}

/// Workspace never falls back to full Drive or unrelated account scopes.
pub async fn workspace_access_token(
    client: &reqwest::Client,
    account: &str,
    interactive: bool,
) -> Result<Zeroizing<String>> {
    if interactive {
        use std::io::IsTerminal;
        ensure!(
            std::io::stdin().is_terminal(),
            "interactive doctor requires an owner-operated terminal"
        );
    }
    let bytes = stored_token(account, interactive)?;
    let refresh = workspace_refresh_credentials(&bytes)?;
    renew(client, refresh, OAuthScopes::DriveFile, interactive).await
}

fn workspace_refresh_credentials(bytes: &[u8]) -> Result<Zeroizing<String>> {
    let token: StoredToken = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::Error::msg("invalid stored OAuth scope evidence"))?;
    ensure!(
        token
            .scopes
            .iter()
            .any(|s| s == OAuthScopes::DriveFile.request()),
        "owner incremental consent for drive.file required; no broad-scope fallback"
    );
    ensure!(
        !token.refresh_token.trim().is_empty()
            && !token.refresh_token.chars().any(char::is_control),
        "refresh credential unavailable or malformed"
    );
    ensure!(
        token.scopes.iter().all(|scope| !scope.is_empty()
            && scope
                .bytes()
                .all(|b| (0x21..=0x7e).contains(&b) && b != b'"' && b != b'\\')),
        "malformed stored scope evidence"
    );
    Ok(token.refresh_token)
}

/// Read the established client without changing any canonical credential record.
/// Used by the isolated Workspace authorization flow; callers must not log values.
pub fn workspace_client(interactive: bool) -> Result<(String, Zeroizing<String>)> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME missing")?);
    let bytes = Zeroizing::new(
        std::fs::read(home.join("Library/Application Support/gogcli/credentials.json"))
            .map_err(|_| anyhow::Error::msg("OAuth client configuration unavailable"))?,
    );
    let creds = client_credentials(&bytes, interactive, keychain_record)?;
    Ok((creds.client_id, creds.client_secret))
}

async fn renew(
    client: &reqwest::Client,
    refresh: Zeroizing<String>,
    scopes: OAuthScopes,
    interactive: bool,
) -> Result<Zeroizing<String>> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME missing")?);
    let path = home.join("Library/Application Support/gogcli/credentials.json");
    let creds = Zeroizing::new(
        std::fs::read(path)
            .map_err(|_| anyhow::Error::msg("OAuth client configuration unavailable"))?,
    );
    let creds = client_credentials(&creds, interactive, keychain_record)?;
    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "refresh_token"),
            ("scope", scopes.request()),
            ("refresh_token", refresh.as_str()),
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
        ])
        .send()
        .await
        .map_err(|_| anyhow::Error::msg("OAuth transport failed"))?;
    let success = response.status().is_success();
    let mut response = response;
    let mut bytes = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::Error::msg("OAuth response interrupted"))?
    {
        ensure!(
            bytes.len() + chunk.len() <= MAX_OAUTH_RESPONSE,
            "OAuth response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    oauth_token_response(&bytes, success, scopes)
}

fn oauth_token_response(
    bytes: &[u8],
    success: bool,
    scopes: OAuthScopes,
) -> Result<Zeroizing<String>> {
    ensure!(
        bytes.len() <= MAX_OAUTH_RESPONSE,
        "OAuth response exceeds limit"
    );
    if !success {
        #[derive(Deserialize)]
        struct Failure {
            error: String,
        }
        let failure = serde_json::from_slice::<Failure>(bytes).ok();
        let category = match failure.as_ref().map(|failure| failure.error.as_str()) {
            Some("invalid_scope") => "invalid_scope",
            Some("invalid_grant") => "invalid_grant",
            Some("invalid_client") => "invalid_client",
            _ => "unknown",
        };
        // Never expose the provider's description, body, or unrecognized code.
        bail!("OAuth renewal denied ({category})")
    }
    #[derive(Deserialize)]
    struct Response {
        scope: String,
        access_token: Zeroizing<String>,
    }
    let response: Response = serde_json::from_slice(bytes)
        .map_err(|_| anyhow::Error::msg("invalid OAuth response or scope evidence"))?;
    validate_scopes(&response.scope, scopes)?;
    ensure!(
        !response.access_token.trim().is_empty()
            && !response.access_token.chars().any(char::is_control),
        "OAuth access credential missing or malformed"
    );
    Ok(response.access_token)
}

/// Gmail offers no draft-only OAuth scope. Both supported grants permit send,
/// and modify also permits mailbox mutations; the HTTP allowlist denies them.
/// Accept only the exact subset selected before renewal, never a broader reply.
pub(crate) fn validate_scopes(scopes: &str, selected: OAuthScopes) -> Result<()> {
    let expected = selected
        .request()
        .split_whitespace()
        .collect::<std::collections::HashSet<_>>();
    let actual = scopes
        .split_whitespace()
        .collect::<std::collections::HashSet<_>>();
    ensure!(
        actual == expected,
        "OAuth scope evidence does not match the requested connector subset"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    fn stored_record(scopes: Value) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "refresh_token": "synthetic-refresh-credential",
            "scopes": scopes,
        }))
        .expect("synthetic record serialization")
    }

    #[test]
    fn workspace_never_substitutes_broad_drive_grants() -> Result<()> {
        assert_eq!(
            workspace_refresh_credentials(&stored_record(serde_json::json!([MODIFY_SCOPE])))
                .unwrap_err()
                .to_string(),
            "owner incremental consent for drive.file required; no broad-scope fallback"
        );
        let scope = OAuthScopes::DriveFile.request();
        assert!(workspace_refresh_credentials(&stored_record(serde_json::json!([scope]))).is_ok());
        for scopes in [
            serde_json::json!([]),
            serde_json::json!(["https://www.googleapis.com/auth/drive"]),
            serde_json::json!([
                "https://www.googleapis.com/auth/documents",
                "https://www.googleapis.com/auth/spreadsheets"
            ]),
            serde_json::json!([scope, "malformed scope"]),
            Value::Null,
        ] {
            assert!(workspace_refresh_credentials(&stored_record(scopes)).is_err());
        }
        assert!(
            validate_scopes(
                "https://www.googleapis.com/auth/drive",
                OAuthScopes::DriveFile
            )
            .is_err()
        );
        assert!(
            validate_scopes(&format!("{scope} {MODIFY_SCOPE}"), OAuthScopes::DriveFile).is_err()
        );
        Ok(())
    }

    #[test]
    fn stored_grant_selects_only_supported_literal_subset_and_prefers_pair() -> Result<()> {
        for scopes in [
            serde_json::json!([COMPOSE_SCOPE, READONLY_SCOPE]),
            serde_json::json!([MODIFY_SCOPE, READONLY_SCOPE, "openid", COMPOSE_SCOPE]),
        ] {
            let (refresh, selected) = refresh_credentials(&stored_record(scopes))?;
            assert_eq!(refresh.as_str(), "synthetic-refresh-credential");
            assert_eq!(selected, OAuthScopes::ComposeReadonly);
            assert_eq!(
                selected.request(),
                format!("{COMPOSE_SCOPE} {READONLY_SCOPE}")
            );
        }
        for scopes in [
            serde_json::json!([MODIFY_SCOPE]),
            serde_json::json!([
                "email",
                "https://www.googleapis.com/auth/calendar",
                MODIFY_SCOPE,
                "https://www.googleapis.com/auth/gmail.settings.basic",
                "https://www.googleapis.com/auth/gmail.settings.sharing",
                "https://www.googleapis.com/auth/pubsub",
                "https://www.googleapis.com/auth/userinfo.email",
                "openid"
            ]),
            serde_json::json!([COMPOSE_SCOPE, MODIFY_SCOPE]),
            serde_json::json!([READONLY_SCOPE, MODIFY_SCOPE]),
        ] {
            let (_, selected) = refresh_credentials(&stored_record(scopes))?;
            assert_eq!(selected, OAuthScopes::Modify);
            assert_eq!(selected.request(), MODIFY_SCOPE);
        }
        Ok(())
    }

    #[test]
    fn unsupported_or_malformed_stored_scopes_never_select_a_fallback() {
        for scopes in [
            Value::Null,
            serde_json::json!(MODIFY_SCOPE),
            serde_json::json!({"scope": MODIFY_SCOPE}),
            serde_json::json!([]),
            serde_json::json!([COMPOSE_SCOPE]),
            serde_json::json!([READONLY_SCOPE]),
            serde_json::json!(["https://mail.google.com/"]),
            serde_json::json!([MODIFY_SCOPE, null]),
            serde_json::json!([MODIFY_SCOPE, 42]),
            serde_json::json!([MODIFY_SCOPE, ""]),
            serde_json::json!([MODIFY_SCOPE, "scope with spaces"]),
            serde_json::json!([MODIFY_SCOPE, "synthetic-sensitive-payload\n"]),
            serde_json::json!([MODIFY_SCOPE, "synthetic-sensitive-payload\\"]),
            serde_json::json!([MODIFY_SCOPE, "synthetic-sensitive-payload\""]),
        ] {
            let error = refresh_credentials(&stored_record(scopes))
                .expect_err("unusable scope evidence must fail before renewal");
            assert!(!format!("{error:?}").contains("synthetic-sensitive-payload"));
        }
        for record in [
            br#"{"refresh_token":"synthetic-sensitive-payload"}"#.as_slice(),
            br#"{"refresh_token":"synthetic-sensitive-payload","scopes":[],"scopes":[]}"#
                .as_slice(),
            br#"{"refresh_token":"synthetic-sensitive-payload","scopes":["#.as_slice(),
        ] {
            let error = refresh_credentials(record).expect_err("malformed record must fail");
            assert!(!format!("{error:?}").contains("synthetic-sensitive-payload"));
        }
    }

    #[test]
    fn refresh_response_must_match_the_selected_subset_without_fallback() -> Result<()> {
        for selected in [
            OAuthScopes::ComposeReadonly,
            OAuthScopes::Modify,
            OAuthScopes::DriveFile,
        ] {
            let success = serde_json::to_vec(&serde_json::json!({
                "access_token": "synthetic-access-credential",
                "scope": selected.request(),
            }))?;
            assert_eq!(
                oauth_token_response(&success, true, selected)?.as_str(),
                "synthetic-access-credential"
            );
            let other = match selected {
                OAuthScopes::ComposeReadonly => OAuthScopes::Modify,
                OAuthScopes::Modify => OAuthScopes::ComposeReadonly,
                OAuthScopes::DriveFile => OAuthScopes::Modify,
            };
            for scope in [
                other.request().to_owned(),
                format!("{} openid", selected.request()),
                format!("{} https://mail.google.com/", selected.request()),
                format!(
                    "{} https://www.googleapis.com/auth/calendar",
                    selected.request()
                ),
                String::new(),
            ] {
                let bytes = serde_json::to_vec(&serde_json::json!({
                    "access_token": "synthetic-sensitive-payload",
                    "scope": scope,
                }))?;
                let error = oauth_token_response(&bytes, true, selected)
                    .expect_err("response may not substitute or expand the chosen set");
                assert!(!format!("{error:?}").contains("synthetic-sensitive-payload"));
            }
            // Even valid-looking scope/access fields cannot turn a denial into
            // success, and do not authorize a second request with another scope.
            assert!(oauth_token_response(&success, false, selected).is_err());
        }
        Ok(())
    }

    #[test]
    fn refresh_response_rejects_missing_or_malformed_scope_evidence() -> Result<()> {
        for scopes in [Value::Null, serde_json::json!([]), serde_json::json!(42)] {
            let response = serde_json::to_vec(&serde_json::json!({
                "access_token": "synthetic-sensitive-payload",
                "scope": scopes,
            }))?;
            let error = oauth_token_response(&response, true, OAuthScopes::Modify)
                .expect_err("invalid scope evidence must not release a credential");
            assert!(!format!("{error:?}").contains("synthetic-sensitive-payload"));
        }
        for bytes in [
            br#"{"access_token":"synthetic-sensitive-payload"}"#.as_slice(),
            br#"{"access_token":"synthetic-sensitive-payload","scope":"x","scope":"y"}"#.as_slice(),
            br#"{"access_token":"synthetic-sensitive-payload""#.as_slice(),
        ] {
            let error = oauth_token_response(bytes, true, OAuthScopes::Modify)
                .expect_err("missing or ambiguous evidence must fail");
            assert!(!format!("{error:?}").contains("synthetic-sensitive-payload"));
        }
        Ok(())
    }

    #[test]
    fn refresh_denials_report_only_allowlisted_error_categories() -> Result<()> {
        for (code, category) in [
            ("invalid_scope", "invalid_scope"),
            ("invalid_grant", "invalid_grant"),
            ("invalid_client", "invalid_client"),
            ("synthetic-sensitive-payload", "unknown"),
            ("invalid_scope\nsynthetic-sensitive-payload", "unknown"),
        ] {
            let response = serde_json::to_vec(&serde_json::json!({
                "error": code,
                "error_description": "synthetic-sensitive-payload",
                "access_token": "synthetic-sensitive-payload",
            }))?;
            let error = oauth_token_response(&response, false, OAuthScopes::Modify)
                .expect_err("denied renewal must fail");
            assert_eq!(
                error.to_string(),
                format!("OAuth renewal denied ({category})")
            );
            assert!(!format!("{error:?}").contains("synthetic-sensitive-payload"));
        }
        for bytes in [
            b"synthetic-sensitive-payload".as_slice(),
            br#"{"error":42,"error_description":"synthetic-sensitive-payload"}"#.as_slice(),
            br#"{"error":"invalid_scope","error":"invalid_client"}"#.as_slice(),
        ] {
            let error = oauth_token_response(bytes, false, OAuthScopes::Modify)
                .expect_err("unknown denial must fail");
            assert_eq!(error.to_string(), "OAuth renewal denied (unknown)");
        }
        Ok(())
    }

    #[test]
    fn both_success_and_failure_oauth_responses_are_bounded() {
        let response = vec![b'x'; MAX_OAUTH_RESPONSE + 1];
        for success in [false, true] {
            assert_eq!(
                oauth_token_response(&response, success, OAuthScopes::Modify)
                    .expect_err("oversized response must fail")
                    .to_string(),
                "OAuth response exceeds limit"
            );
        }
    }

    #[test]
    fn metadata_only_reads_exact_default_secret_key_and_trims_raw_bytes() -> Result<()> {
        for interactive in [false, true] {
            for metadata in [
                br#"{"client_id":"synthetic-client"}"#.as_slice(),
                br#"{"client_id":"synthetic-client","client_secret":"   "}"#.as_slice(),
            ] {
                let calls = Cell::new(0);
                let credentials =
                    client_credentials(metadata, interactive, |service, key, allowed| {
                        calls.set(calls.get() + 1);
                        assert_eq!(service, "gogcli");
                        assert_eq!(key, "client/default/client-secret");
                        assert_eq!(allowed, interactive);
                        Ok(Zeroizing::new(b" \tsynthetic-client-secret\r\n".to_vec()))
                    })?;
                assert_eq!(calls.get(), 1);
                assert_eq!(credentials.client_id, "synthetic-client");
                assert_eq!(
                    credentials.client_secret.as_str(),
                    "synthetic-client-secret"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn legacy_inline_secret_does_not_read_keychain() -> Result<()> {
        let credentials = client_credentials(
            br#"{"client_id":"synthetic-client","client_secret":"legacy-secret"}"#,
            false,
            |_, _, _| panic!("nonempty inline secret must bypass Keychain fallback"),
        )?;
        assert_eq!(credentials.client_secret.as_str(), "legacy-secret");
        Ok(())
    }

    #[test]
    fn malformed_metadata_never_falls_back_or_exposes_its_payload() {
        for metadata in [
            br#"{"client_id":"synthetic-sensitive-payload""#.as_slice(),
            br#"{"client_secret":"synthetic-sensitive-payload"}"#.as_slice(),
            br#"{"client_id":null,"client_secret":"synthetic-sensitive-payload"}"#.as_slice(),
            br#"{"client_id":42,"client_secret":"synthetic-sensitive-payload"}"#.as_slice(),
            br#"{"client_id":"   ","client_secret":"synthetic-sensitive-payload"}"#.as_slice(),
            br#"{"client_id":"synthetic-client","client_secret":null}"#.as_slice(),
            br#"{"client_id":"synthetic-client","client_secret":42}"#.as_slice(),
            br#"{"client_id":"synthetic-client","client_secret":"synthetic-sensitive-payload","client_secret":"second"}"#.as_slice(),
            br#"{"client_id":"synthetic-client","client_secret":"synthetic-sensitive-payload\u0000"}"#.as_slice(),
        ] {
            let error = client_credentials(metadata, false, |_, _, _| {
                panic!("malformed metadata must not fall back to Keychain")
            }).err().expect("malformed metadata must be rejected");
            assert!(!format!("{error:?}").contains("synthetic-sensitive-payload"));
        }
    }

    #[test]
    fn invalid_keychain_secret_bytes_fail_closed_without_payload_errors() {
        for bytes in [
            b"".as_slice(),
            b" \r\n\t",
            b"synthetic-sensitive-payload\xff",
            b"synthetic-sensitive-payload\0",
            b"synthetic-sensitive-payload\nsecond",
        ] {
            let error =
                client_credentials(br#"{"client_id":"synthetic-client"}"#, false, |_, _, _| {
                    Ok(Zeroizing::new(bytes.to_vec()))
                })
                .err()
                .expect("invalid secret must be rejected");
            assert!(!format!("{error:?}").contains("synthetic-sensitive-payload"));
        }
    }

    #[test]
    fn secret_access_error_propagates_without_retry_or_payload() {
        let calls = Cell::new(0);
        let error = client_credentials(br#"{"client_id":"synthetic-client"}"#, false, |_, _, _| {
            calls.set(calls.get() + 1);
            Err(anyhow::Error::msg(
                "access denied: synthetic-sensitive-payload",
            ))
        })
        .err()
        .expect("access denial must remain an error");
        assert_eq!(calls.get(), 1);
        assert!(error.to_string().contains("Keychain access unavailable"));
        assert!(!format!("{error:?}").contains("synthetic-sensitive-payload"));
    }

    #[test]
    fn every_keychain_read_restores_noninteractive_policy_even_on_error() -> Result<()> {
        for interactive in [false, true] {
            for denied in [false, true] {
                let events = RefCell::new(Vec::new());
                let result = with_keychain_interaction(
                    interactive,
                    |allowed| {
                        events.borrow_mut().push(format!("interaction:{allowed}"));
                        Ok(())
                    },
                    || {
                        events.borrow_mut().push("read".into());
                        if denied {
                            Err(anyhow::Error::msg("synthetic access denied"))
                        } else {
                            Ok(Zeroizing::new(b"synthetic-secret".to_vec()))
                        }
                    },
                );
                assert_eq!(
                    *events.borrow(),
                    vec![
                        format!("interaction:{interactive}"),
                        "read".into(),
                        "interaction:false".into()
                    ]
                );
                if denied {
                    let error = match result {
                        Err(error) => error,
                        Ok(_) => panic!("access denial must remain an error"),
                    };
                    assert_eq!(error.to_string(), "synthetic access denied");
                } else {
                    assert_eq!(result?.as_slice(), b"synthetic-secret");
                }
            }
        }
        Ok(())
    }

    #[test]
    fn keychain_policy_failures_do_not_return_secret_bytes() {
        let calls = Cell::new(0);
        let result = with_keychain_interaction(
            true,
            |_| {
                calls.set(calls.get() + 1);
                if calls.get() == 2 {
                    Err(anyhow::Error::msg("cannot restore interaction policy"))
                } else {
                    Ok(())
                }
            },
            || Ok(Zeroizing::new(b"synthetic-sensitive-payload".to_vec())),
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("reset failure must reject the secret"),
        };
        assert_eq!(calls.get(), 2);
        assert!(!format!("{error:?}").contains("synthetic-sensitive-payload"));
        assert!(
            with_keychain_interaction(
                true,
                |_| Err(anyhow::Error::msg("cannot configure interaction policy")),
                || panic!("failed policy setup must not read Keychain"),
            )
            .is_err()
        );
    }
}
