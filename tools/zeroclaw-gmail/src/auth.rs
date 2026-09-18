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

const GOG_KEYCHAIN_SERVICE: &str = "gogcli";
const DEFAULT_CLIENT_SECRET_KEY: &str = "client/default/client-secret";

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
    let creds = client_credentials(&creds, interactive, keychain_record)?;
    let response = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("grant_type", "refresh_token"),
            ("scope", "https://www.googleapis.com/auth/gmail.compose https://www.googleapis.com/auth/gmail.readonly"),
            ("refresh_token", refresh.as_str()),
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

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
