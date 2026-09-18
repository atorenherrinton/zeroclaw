//! One-time migration verification. No model, logger, configuration writes, or page output.
//! Takes an existing private install directory and private browser overlay JSON.

#[cfg(unix)]
#[path = "support/verify_native_modmail_browser.rs"]
mod unix;

#[cfg(unix)]
fn main() {
    unix::main();
}

#[cfg(not(unix))]
fn main() {
    // i18n-exempt: stable machine-readable unsupported-platform status.
    eprintln!("native_verification_requires_unix");
    std::process::exit(2);
}
