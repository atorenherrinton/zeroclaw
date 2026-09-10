use anyhow::{Result, bail};
use zeroize::Zeroizing;

pub const SERVICE: &str = "com.zeroclaw.local.service-cli";
pub const SLOTS: [&str; 3] = ["vercel", "resend", "resend-send"];

pub fn validate_slot(slot: &str) -> Result<()> {
    if !SLOTS.contains(&slot) {
        bail!("Use credential slot vercel, resend, or resend-send");
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

#[cfg(target_os = "macos")]
pub fn load(slot: &str) -> Result<Zeroizing<Vec<u8>>> {
    use security_framework::os::macos::keychain::SecKeychain;
    validate_slot(slot)?;
    // This helper never prompts from an unattended MCP call or silently unlocks
    // a keychain. Only the three exact app-owned items are queried.
    let _interaction_guard =
        SecKeychain::disable_user_interaction().map_err(|e| safe_error(e.code()))?;
    let keychain = SecKeychain::default().map_err(|e| safe_error(e.code()))?;
    let (password, _) = keychain
        .find_generic_password(SERVICE, slot)
        .map_err(|e| safe_error(e.code()))?;
    let token = Zeroizing::new(password.to_vec());
    validate_token(&token)?;
    Ok(token)
}

#[cfg(target_os = "macos")]
pub fn store(slot: &str, token: &[u8]) -> Result<()> {
    use security_framework::os::macos::keychain::SecKeychain;
    validate_slot(slot)?;
    validate_token(token)?;
    let keychain = SecKeychain::default().map_err(|e| safe_error(e.code()))?;
    // macOS's normal creator-app ACL applies; do not grant all apps access.
    keychain
        .set_generic_password(SERVICE, slot, token)
        .map_err(|e| safe_error(e.code()))
}

#[cfg(not(target_os = "macos"))]
pub fn load(_: &str) -> Result<Zeroizing<Vec<u8>>> {
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
        for slot in ["login", "Safari", "../vercel", "", "VERCEL"] {
            assert!(validate_slot(slot).is_err());
        }
        for token in [
            b"short".as_slice(),
            b"long-enough-token\n",
            b"long enough token",
        ] {
            assert!(validate_token(token).is_err());
        }
    }
}
