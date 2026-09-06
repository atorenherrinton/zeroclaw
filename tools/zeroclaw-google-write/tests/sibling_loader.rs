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
            "name":"calendar_update_event","arguments":{
                "calendar_id":"primary","event_id":"synthetic123",
                "location":"New location"
            }
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
