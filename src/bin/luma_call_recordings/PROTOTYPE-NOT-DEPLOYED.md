# Superseded prototype — not installed

The user explicitly requires a ZeroClaw-only installation with no OpenClaw runtime dependency. The OpenClaw-backed ingress/recording design in this folder was stopped before compilation, installation, configuration initialization, service creation, or route cutover.

`common.rs` contains the abandoned OpenClaw credential/upstream assumptions and must not be deployed. Its local directories and bot username are accepted only through explicit environment inputs so this source does not encode an owner's workstation or identity. `protocol.rs` contains potentially reusable generic Rust Twilio protocol helpers. A hermetic test compiles both modules and exercises the protocol self-check, but the prototype remains uninstalled and unverified against live services.

The replacement must use ZeroClaw-owned authentication, configuration, telephony, call memory, and Telegram delivery. New permanent custom code must be Rust. Existing OpenClaw services have not yet been disabled; that remaining cutover must account for the currently working phone and scheduled workflows.
