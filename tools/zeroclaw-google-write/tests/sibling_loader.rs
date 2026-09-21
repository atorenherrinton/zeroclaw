//! Process-boundary regression tests. Only synthetic shell fixtures execute;
//! no Google credentials, network requests, or calendar mutations are involved.
use serde_json::{Value, json};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

struct Install(PathBuf);
impl Install {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "google-writer-sibling-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        fs::create_dir(path.join("install with spaces")).unwrap();
        fs::create_dir(path.join("other-cwd")).unwrap();
        fs::copy(
            env!("CARGO_BIN_EXE_zeroclaw-google-write"),
            path.join("install with spaces/relocated-writer"),
        )
        .unwrap();
        Self(path)
    }

    fn sibling(&self) -> PathBuf {
        self.0.join("install with spaces/gog-calendar-patch")
    }

    fn invoke(&self) -> Value {
        self.invoke_tool(
            "calendar_update_event",
            json!({
                "calendar_id":"primary","event_id":"synthetic123","location":"New location"
            }),
        )
    }

    fn invoke_tool(&self, name: &str, arguments: Value) -> Value {
        let mut child = Command::new(self.0.join("install with spaces/relocated-writer"))
            .current_dir(self.0.join("other-cwd"))
            .env_clear()
            .env("HOME", &self.0)
            .env("PATH", self.0.join("other-cwd"))
            .env("GOG_ACCOUNT", "synthetic-owner@example.com")
            .env("WRITER_TEST_UNTRUSTED_ENV", "must-not-reach-child")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
            "name":name,"arguments":arguments
        }});
        writeln!(child.stdin.take().unwrap(), "{request}").unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        serde_json::from_slice(&output.stdout).unwrap()
    }
}
impl Drop for Install {
    fn drop(&mut self) {
        // This directory was uniquely created by this test and contains fixtures only.
        fs::remove_dir_all(&self.0).unwrap();
    }
}
fn executable(path: &Path, script: &str) {
    fs::write(path, script).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn relocated_writer_uses_only_its_sibling_and_preserves_transport_guards() {
    let install = Install::new();
    executable(
        &install.0.join("other-cwd/gog-calendar-patch"),
        "#!/bin/sh\nexit 99\n",
    );
    executable(
        &install.sibling(),
        r##"#!/bin/sh
set -eu
test -z "${WRITER_TEST_UNTRUSTED_ENV+x}"
test -z "${GOG_ACCOUNT+x}"
test "$PATH" = /opt/homebrew/bin:/usr/bin:/bin
dir=${0%/*}
printf '%s\n' --CALL-- "$@" >> "$dir/calls"
case " $* " in
  *' calendar.events.get '*)
    if test -f "$dir/patched"; then location='New location'; version=v2; else location='Old location'; version=v1; fi
    printf '%s\n' '{"id":"synthetic123","etag":"\"'"$version"'\"","status":"confirmed","summary":"Old title","location":"'"$location"'","start":{"dateTime":"2030-01-01T10:00:00Z"},"end":{"dateTime":"2030-01-01T11:00:00Z"}}' ;;
  *' calendar.events.patch '*) : > "$dir/patched"; printf '%s\n' '{"id":"synthetic123","etag":"\"v2\""}' ;;
  *) exit 98 ;;
esac
"##,
    );
    let response = install.invoke();
    assert_eq!(
        response["result"]["structuredContent"]["updated"], true,
        "{response}"
    );
    let calls = fs::read_to_string(install.0.join("install with spaces/calls")).unwrap();
    let calls: Vec<_> = calls.split("--CALL--\n").skip(1).collect();
    assert_eq!(calls.len(), 4);
    for call in &calls {
        assert!(call.contains("--account=synthetic-owner@example.com\n"));
        assert!(call.contains("--no-input\n"));
        assert!(call.contains("--gmail-no-send\n"));
        assert!(!call.contains("--results-only"));
    }
    for index in [0, 1, 3] {
        assert!(calls[index].contains("--readonly\n"));
    }
    assert!(calls[0].contains("--enable-commands-exact=api.call,api.calendar.events.get\n"));
    assert!(calls[2].contains("--enable-commands-exact=api.call,api.calendar.events.patch\n"));
    assert!(calls[2].contains("--single-attempt\n"));
    assert!(calls[2].contains("--if-match=\"v1\"\n"));
    assert!(calls[2].contains("--body={\"location\":\"New location\"}\n"));
    assert!(calls[2].contains("\"sendUpdates\":\"none\""));
}

#[test]
fn failing_sibling_read_stops_without_fallback_or_write() {
    let install = Install::new();
    executable(
        &install.sibling(),
        "#!/bin/sh\ndir=${0%/*}\nprintf '%s\\n' --CALL-- \"$@\" >> \"$dir/calls\"\nprintf '%s\\n' 'synthetic loader failure' >&2\nexit 71\n",
    );
    let response = install.invoke();
    assert_eq!(response["result"]["isError"], true);
    assert!(
        response["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Exact event read failed; no patch attempted")
    );
    let calls = fs::read_to_string(install.0.join("install with spaces/calls")).unwrap();
    assert_eq!(calls.matches("--CALL--").count(), 1);
    assert!(calls.contains("calendar.events.get"));
    assert!(!calls.contains("calendar.events.patch"));
}

#[test]
fn attendee_preread_keychain_failure_survives_mcp_without_writing_or_registering() {
    let install = Install::new();
    executable(
        &install.sibling(),
        r##"#!/bin/sh
set -eu
dir=${0%/*}
printf '%s\n' --CALL-- "$@" >> "$dir/calls"
printf '%s\n' 'token source: get token for private@example.invalid: read token: keyring connection timed out after 30s; private diagnostic details' >&2
exit 1
"##,
    );
    let args = json!({
        "action":"update","calendar_id":"primary","event_id":"_synthetic_exact_id",
        "expected_etag":"\"v1\"","attendees":["guest@example.invalid"],
        "attendees_owner_authorized":true,"send_updates":"all","scope":"single",
        "idempotency_key":"fixture-attendee-invitation","owner_authorized":true
    });
    let validation = install.invoke_tool("calendar_validate", args.clone());
    assert_eq!(validation["result"]["structuredContent"]["valid"], true);
    assert_eq!(
        validation["result"]["structuredContent"]["provider_read"],
        false
    );
    assert!(!install.0.join("install with spaces/calls").exists());
    let response = install.invoke_tool("calendar_mutate", args);
    assert_eq!(response["result"]["isError"], true);
    let message = response["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        message.starts_with("read failed; no mutation attempted: google_keychain_access_required:")
    );
    assert!(message.contains("native macOS session"));
    assert!(!message.contains("private@example.invalid"));
    assert!(!message.contains("private diagnostic details"));
    let reconciled = install.invoke_tool(
        "calendar_reconcile",
        json!({"idempotency_key":"fixture-attendee-invitation"}),
    );
    assert_eq!(reconciled["result"]["isError"], true);
    assert_eq!(
        reconciled["result"]["content"][0]["text"],
        "unknown action key"
    );
    let narrow = install.invoke();
    assert!(
        narrow["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with(
                "Exact event read failed; no patch attempted: google_keychain_access_required:"
            )
    );
    let calls = fs::read_to_string(install.0.join("install with spaces/calls")).unwrap();
    assert_eq!(calls.matches("--CALL--").count(), 2);
    assert!(calls.contains("\"eventId\":\"_synthetic_exact_id\""));
    assert!(calls.contains("calendar.events.get"));
    assert!(calls.contains("--readonly\n"));
    assert!(!calls.contains("--allow-write"));
    assert!(!calls.contains("calendar.events.patch"));
}

#[test]
fn saved_compatibility_create_returns_actionable_receipt_over_mcp_and_recovers_readonly() {
    use sha2::{Digest, Sha256};
    use zeroclaw_personal_ops::Ops;
    let install = Install::new();
    let args = json!({"summary":"Synthetic appointment","start":"2030-01-01T10:00:00-08:00","end":"2030-01-01T11:00:00-08:00"});
    let hash = |v: &Value| format!("{:x}", Sha256::digest(serde_json::to_vec(v).unwrap()));
    let identity = json!({"summary":args["summary"],"start":1893520800i64,"end":1893524400i64});
    let key = format!("legacy-create-{}", hash(&identity));
    let journal_key = hash(&json!({"account":"synthetic-owner@example.com","key":key}));
    let mut durable = args.clone();
    for (k,v) in json!({"action":"create","calendar_id":"primary","idempotency_key":key,"owner_authorized":true,"timezone":"America/Los_Angeles","send_updates":"none"}).as_object().unwrap() {
        durable[k] = v.clone();
    }
    let ops = Ops::open(&install.0.join(".zeroclaw")).unwrap();
    ops.db.execute_batch("CREATE TABLE calendar_actions(key TEXT PRIMARY KEY,request_hash TEXT NOT NULL,request TEXT NOT NULL,event_id TEXT NOT NULL,intended TEXT NOT NULL,before_image TEXT NOT NULL,state TEXT NOT NULL,evidence TEXT NOT NULL,created_ms INTEGER NOT NULL);").unwrap();
    ops.db
        .execute(
            "INSERT INTO calendar_actions VALUES(?1,?2,?3,'0cfixture',?4,'{}','uncertain','{}',0)",
            rusqlite::params![
                journal_key,
                hash(&durable),
                durable.to_string(),
                json!({"id":"0cfixture","summary":"Synthetic appointment"}).to_string()
            ],
        )
        .unwrap();
    drop(ops);
    executable(
        &install.sibling(),
        r##"#!/bin/sh
set -eu
dir=${0%/*}
printf '%s\n' --CALL-- "$@" >> "$dir/calls"
case " $* " in *" --readonly "*) ;; *) exit 99;; esac
printf '%s\n' 'read token: keyring connection timed out after 30s; private@example.invalid' >&2
exit 1
"##,
    );
    let response = install.invoke_tool("calendar_create_event", args.clone());
    assert_ne!(response["result"]["isError"], true, "{response}");
    let receipt = &response["result"]["structuredContent"];
    assert_eq!(receipt["state"], "uncertain");
    assert_eq!(receipt["created"], false);
    assert_eq!(receipt["idempotency_key"], key);
    assert_eq!(receipt["event_id"], "0cfixture");
    assert_eq!(
        receipt["evidence"]["read_error"]["code"],
        "google_keychain_access_required"
    );
    assert!(!response.to_string().contains("private@example.invalid"));
    let recovered = install.invoke_tool("calendar_reconcile", json!({"create_identity":args}));
    assert_eq!(
        recovered["result"]["structuredContent"]["idempotency_key"],
        key
    );
    executable(
        &install.sibling(),
        r##"#!/bin/sh
set -eu
dir=${0%/*}
printf '%s\n' --CALL-- "$@" >> "$dir/calls"
case " $* " in *" --readonly "*) ;; *) exit 99;; esac
printf '%s\n' '{"id":"0cfixture","summary":"Synthetic appointment"}'
"##,
    );
    let verified = install.invoke_tool(
        "calendar_reconcile",
        receipt["reconcile"]["arguments"].clone(),
    );
    assert_eq!(verified["result"]["structuredContent"]["state"], "verified");
    let calls = fs::read_to_string(install.0.join("install with spaces/calls")).unwrap();
    assert_eq!(calls.matches("--CALL--").count(), 3);
    assert!(!calls.contains("calendar.events.insert"));
    assert!(!calls.contains("calendar.events.delete"));
    assert!(!calls.contains("calendar.events.patch"));
}

#[test]
fn tentative_create_reaches_sibling_and_persists_status_without_invites_or_replay() {
    let install = Install::new();
    executable(
        &install.sibling(),
        r##"#!/usr/bin/python3
import json, pathlib, sys
root = pathlib.Path(__file__).parent
args = sys.argv[1:]
params = json.loads(next(a.removeprefix('--params=') for a in args if a.startswith('--params=')))
if 'calendar.events.insert' in args:
    assert '--single-attempt' in args and '--allow-write' in args
    assert params['sendUpdates'] == 'none'
    body = json.loads(next(a.removeprefix('--body=') for a in args if a.startswith('--body=')))
    assert body['status'] == 'tentative' and 'attendees' not in body
    assert not (root / 'event.json').exists(), 'insert must not replay'
    (root / 'event.json').write_text(json.dumps(body))
elif 'calendar.events.get' in args:
    assert '--readonly' in args and '--allow-write' not in args
    body = json.loads((root / 'event.json').read_text())
    assert params['eventId'] == body['id']
else:
    raise AssertionError('unexpected provider operation')
with (root / 'calls.jsonl').open('a') as log:
    log.write(json.dumps(args) + '\n')
print(json.dumps(body))
"##,
    );
    let arguments = json!({"action":"create","calendar_id":"primary",
        "idempotency_key":"tentative-process-fixture","owner_authorized":true,
        "summary":"Tentative fixture","status":"tentative","send_updates":"none",
        "start":"2030-01-01T10:00:00Z","end":"2030-01-01T11:00:00Z"});
    let first = install.invoke_tool("calendar_mutate", arguments.clone());
    let receipt = &first["result"]["structuredContent"];
    assert_eq!(receipt["state"], "verified", "{first}");
    assert_eq!(receipt["invitations_delivered"], false);
    assert_eq!(receipt["evidence"]["notifications_requested"], false);
    let duplicate = install.invoke_tool("calendar_mutate", arguments);
    assert_eq!(
        duplicate["result"]["structuredContent"]["duplicate_prevented"],
        true
    );
    assert_eq!(
        duplicate["result"]["structuredContent"]["event_id"],
        receipt["event_id"]
    );
    let calls = fs::read_to_string(install.0.join("install with spaces/calls.jsonl")).unwrap();
    assert_eq!(
        calls.lines().count(),
        2,
        "one insert and one verification GET"
    );
}
