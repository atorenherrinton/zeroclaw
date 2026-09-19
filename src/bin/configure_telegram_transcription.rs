//! Local, one-shot voice-key migration. No speech API calls are made.
//! Secrets are read into memory and piped to the native CLI, never put in
//! argv, environment variables, plaintext files, or diagnostic output.
//!
//! The operator must provide absolute `ZEROCLAW_CONFIG_DIR`,
//! `OPENCLAW_CONFIG_DIR`, `ZEROCLAW_BIN`, and `ZEROCLAW_BACKUP_DIR` paths.

#[cfg(unix)]
#[path = "support/configure_telegram_transcription.rs"]
mod unix;

#[cfg(unix)]
fn main() {
    unix::main();
}

#[cfg(not(unix))]
fn main() {
    // i18n-exempt: stable machine-readable unsupported-platform status.
    eprintln!("transcription_migration_requires_unix");
    std::process::exit(2);
}
