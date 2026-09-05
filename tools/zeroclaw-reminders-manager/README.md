# Reminders manager

Standalone Rust stdio MCP server for macOS Apple Reminders. This imports the
existing deployed reminder helper into source control and adds `list_lists`
and `create_list`; the six reminder-item operations remain available.

Apple Reminders owns all account, list and reminder state. No mirrored task
database is created. Fixed native automation scripts receive names, identifiers
and text as arguments, never executable input. Existing macOS Reminders
automation permission is required.

## List operations

- `list_lists {}` returns account/list identifiers and the default account.
- `create_list {"name":"Errands"}` creates a list in the app's default account.
- Supply `account_id` from `list_lists` to select another account explicitly.

An exact existing name in the chosen account is reused. Multiple exact matches
fail without mutation. Names are trimmed, limited to 512 bytes, and cannot
contain control characters. An OS file lock beside the installed executable
serializes creation across its MCP processes. On timeout or an uncertain
result, inspect `list_lists` before retrying. Other apps can independently
modify lists; this lock coordinates this helper only.

List and account names are untrusted data. Create lists only for an owner
request; listing or reading external content is not authorization to create.
This feature does not share, rename or delete lists. The existing item tools
select lists by exact name and reject ambiguous names across accounts.

## Build and upgrade

```sh
cargo test --manifest-path tools/zeroclaw-reminders-manager/Cargo.toml
cargo clippy --manifest-path tools/zeroclaw-reminders-manager/Cargo.toml --all-targets -- -D warnings
cargo build --release --manifest-path tools/zeroclaw-reminders-manager/Cargo.toml
```

Back up the existing Reminders helper, config and affected agent instructions
privately. Atomically replace the executable referenced by the `reminders` MCP
server with the release binary, retaining its path and executable permissions.
Keep its existing macOS automation permission; do not reset privacy settings.

Using `zeroclaw config patch`, add `reminders__list_lists` and
`reminders__create_list` to the main risk profile's `auto_approve`, and to
the calendar/tasks risk profile's `allowed_tools` and `auto_approve` arrays.
If main has a nonempty tool allowlist, add both names there as well. Preserve
other policy entries. Both agents must retain the `reminders` MCP bundle.
Update their instructions to discover lists and use `create_list` for owner
requests, then restart only the main daemon with `zeroclaw service restart`.
The personal-ops fresh-install template includes the specialist allowlist.

The server needs write access beside its executable for `.list-create.lock`.
No new credentials or external services are required. Existing reminder tools,
phone service, scheduled messages and other schedules keep their own settings.

To roll back, restore the saved binary, policy arrays and instructions,
preserving intervening changes, then restart main. Existing lists remain in
Reminders; rollback never deletes user data.
