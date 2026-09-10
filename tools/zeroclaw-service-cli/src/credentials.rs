use anyhow::{Result, bail};
use zeroize::Zeroizing;

pub const SERVICE: &str = "com.zeroclaw.local.service-cli";
pub const SLOTS: [&str; 3] = ["vercel", "resend", "resend-send"];

pub fn validate_slot(slot: &str) -> Result<()> {
    if slot.is_empty()
        || slot.len() > 100
        || !slot
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        bail!("Credential names use 1–100 lowercase letters, digits, hyphens, or underscores");
    }
    Ok(())
}

pub fn validate_token(token: &[u8]) -> Result<()> {
    if !(16..=4096).contains(&token.len()) || !token.iter().all(|b| b.is_ascii_graphic()) {
        bail!("Token must be 16–4096 printable ASCII bytes without whitespace");
    }
    Ok(())
}

fn safe_error(code: i32) -> anyhow::Error {
    anyhow::Error::msg(match code {
        -25300 => "credential_not_configured: run the local setup command",
        -25308 | -25293 => {
            "credential_locked_or_access_denied: complete local Keychain access setup"
        }
        _ => "keychain_operation_failed: inspect local Keychain access; no secret was returned",
    })
}

pub fn validate_secret(secret: &[u8]) -> Result<()> {
    if secret.is_empty()
        || secret.len() > 16384
        || secret.contains(&0)
        || std::str::from_utf8(secret).is_err()
    {
        bail!("Credentials must be 1–16384 UTF-8 bytes without NUL");
    }
    Ok(())
}

pub fn load(slot: &str) -> Result<Zeroizing<Vec<u8>>> {
    load_from(slot, &crate::profiles::load()?, false)
}

#[cfg(target_os = "macos")]
pub fn load_from(
    slot: &str,
    config: &crate::profiles::Config,
    interactive: bool,
) -> Result<Zeroizing<Vec<u8>>> {
    use security_framework::os::macos::keychain::SecKeychain;
    validate_slot(slot)?;
    let _interaction_guard = if interactive {
        None
    } else {
        Some(SecKeychain::disable_user_interaction().map_err(|e| safe_error(e.code()))?)
    };
    let keychain = SecKeychain::default().map_err(|e| safe_error(e.code()))?;
    let (service, account) = config
        .credentials
        .get(slot)
        .map_or((SERVICE, slot), |source| {
            (source.service.as_str(), source.account.as_str())
        });
    let (password, _) = keychain
        .find_generic_password(service, account)
        .map_err(|e| safe_error(e.code()))?;
    let secret = Zeroizing::new(password.to_vec());
    validate_secret(&secret)?;
    Ok(secret)
}

#[cfg(target_os = "macos")]
pub fn store(slot: &str, token: &[u8]) -> Result<()> {
    use security_framework::os::macos::keychain::SecKeychain;
    validate_slot(slot)?;
    validate_secret(token)?;
    if crate::profiles::load()?.credentials.contains_key(slot) {
        bail!(
            "This name is bound to an existing Keychain item; use authorize or choose a new credential name"
        );
    }
    let keychain = SecKeychain::default().map_err(|e| safe_error(e.code()))?;
    // macOS's normal creator-app ACL applies; do not grant all apps access.
    keychain
        .set_generic_password(SERVICE, slot, token)
        .map_err(|e| safe_error(e.code()))
}

#[cfg(not(target_os = "macos"))]
pub fn load_from(_: &str, _: &crate::profiles::Config, _: bool) -> Result<Zeroizing<Vec<u8>>> {
    bail!("Keychain credentials require macOS")
}

#[cfg(not(target_os = "macos"))]
pub fn store(_: &str, _: &[u8]) -> Result<()> {
    bail!("Keychain credentials require macOS")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_arbitrary_keychain_items_and_malformed_tokens() {
        for slot in ["Safari", "../vercel", "", "VERCEL", "has space"] {
            assert!(validate_slot(slot).is_err());
        }
        assert!(validate_slot("aws-production-session").is_ok());
        assert!(validate_secret("short password ✓".as_bytes()).is_ok());
        assert!(validate_secret(b"bad\0value").is_err());
        for token in [
            b"short".as_slice(),
            b"long-enough-token\n",
            b"long enough token",
        ] {
            assert!(validate_token(token).is_err());
        }
    }
}
