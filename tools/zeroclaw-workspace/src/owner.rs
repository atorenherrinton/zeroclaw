//! OS-native owner presence for the CLI issuer. Never called from MCP.
use anyhow::Result;
#[cfg(target_os = "macos")]
pub fn confirm(reason: &str) -> Result<()> {
    use anyhow::ensure;
    use objc2::{
        class, msg_send,
        rc::Retained,
        runtime::{AnyObject, Bool},
    };
    use objc2_foundation::NSString;
    #[link(name = "LocalAuthentication", kind = "framework")]
    unsafe extern "C" {}
    // Fresh context, zero biometric reuse window, and no reusable generic grant.
    // The OS owns authentication UI; no password or biometric material enters us.
    let context: Retained<AnyObject> = unsafe { msg_send![class!(LAContext), new] };
    unsafe {
        let _: () = msg_send![&context, setTouchIDAuthenticationAllowableReuseDuration: 0.0f64];
    }
    let reason = NSString::from_str(reason);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reply = block2::RcBlock::new(move |success: Bool, _error: *mut AnyObject| {
        let _ = sender.send(success.as_bool());
    });
    unsafe {
        let _: () =
            msg_send![&context, evaluatePolicy: 2isize, localizedReason: &*reason, reply: &*reply];
    }
    let result = receiver.recv_timeout(std::time::Duration::from_secs(120));
    unsafe {
        let _: () = msg_send![&context, invalidate];
    }
    ensure!(
        result == Ok(true),
        "native owner authentication cancelled, unavailable or timed out"
    );
    Ok(())
}
#[cfg(not(target_os = "macos"))]
pub fn confirm(_: &str) -> Result<()> {
    anyhow::bail!("native owner document authorization requires macOS")
}

/// Keychain owns the journal authentication key. MCP reads disable native UI.
/// Creating the key is only reachable after native owner authentication.
#[cfg(target_os = "macos")]
pub fn journal_key(create: bool) -> Result<zeroize::Zeroizing<Vec<u8>>> {
    use anyhow::ensure;
    use security_framework::passwords::{get_generic_password, set_generic_password};
    const SERVICE: &str = "zeroclaw-workspace-journal-v1";
    const ACCOUNT: &str = "journal-seal";
    let status = unsafe {
        security_framework_sys::keychain::SecKeychainSetUserInteractionAllowed(u8::from(create))
    };
    ensure!(
        status == 0,
        "cannot set journal Keychain interaction policy"
    );
    let result = (|| match get_generic_password(SERVICE, ACCOUNT) {
        Ok(bytes) => {
            ensure!(bytes.len() == 32, "invalid journal authentication key");
            Ok(zeroize::Zeroizing::new(bytes))
        }
        Err(error) if create && error.code() == -25300 => {
            let mut key = zeroize::Zeroizing::new(vec![0u8; 32]);
            getrandom::getrandom(&mut key)
                .map_err(|_| anyhow::Error::msg("OS randomness unavailable"))?;
            set_generic_password(SERVICE, ACCOUNT, &key)
                .map_err(|_| anyhow::Error::msg("journal Keychain creation denied"))?;
            Ok(key)
        }
        Err(_) => anyhow::bail!(
            "journal Keychain access denied; owner authorization/native approval required"
        ),
    })();
    let status =
        unsafe { security_framework_sys::keychain::SecKeychainSetUserInteractionAllowed(0) };
    ensure!(
        status == 0,
        "cannot reset journal Keychain interaction policy"
    );
    result
}
#[cfg(not(target_os = "macos"))]
pub fn journal_key(_: bool) -> Result<zeroize::Zeroizing<Vec<u8>>> {
    anyhow::bail!("native journal authentication requires macOS")
}
