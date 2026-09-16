use std::{
    io::{BufRead, BufReader},
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn helper() -> std::ffi::OsString {
    std::env::var_os("ZEROCLAW_PUBLIC_BROWSER_TEST_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_zeroclaw-public-browser").into())
}

struct Group {
    child: Child,
    group: i32,
}

impl Group {
    fn new() -> Self {
        // Both this leader and its child ignore TERM, like an unresponsive
        // driver/Chrome pair. Each fixture owns an entirely separate group.
        let mut command = Command::new("/bin/sh");
        command
            .args([
                "-c",
                "trap '' TERM; /bin/sleep 60 & printf '%s\\n' \"$!\"; wait",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let mut child = command.spawn().unwrap();
        let group = child.id() as i32;
        let mut ready = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut ready)
            .unwrap();
        assert!(ready.trim().parse::<u32>().is_ok());
        Self { child, group }
    }

    fn assert_gone(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            // Reap the fixture leader so it cannot keep an empty group alive.
            self.child.try_wait().unwrap();
            if unsafe { libc::kill(-self.group, 0) } == -1
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
            {
                self.group = 0;
                return;
            }
            assert!(
                Instant::now() < deadline,
                "owned group survived watchdog cleanup"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        if self.group > 1 {
            unsafe {
                libc::kill(-self.group, libc::SIGKILL);
            }
        }
        let _ = self.child.wait();
    }
}

struct OwnedProcess(Child);

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct DetachedWatchdog(i32);

impl Drop for DetachedWatchdog {
    fn drop(&mut self) {
        if self.0 > 1 {
            unsafe {
                libc::kill(self.0, libc::SIGKILL);
            }
        }
    }
}

#[test]
fn watchdog_cleans_only_owned_group_after_actual_parent_death() {
    let mut owned = Group::new();
    let mut unrelated = Group::new();
    let mut owner = OwnedProcess(Command::new("/bin/sh")
        .args(["-c", "\"$1\" --watch-driver-group \"$2\" \"$$\" & watcher=$!; printf '%s\\n' \"$watcher\"; wait \"$watcher\"", "owner"])
        .arg(helper())
        .arg(owned.group.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn().unwrap());
    let mut line = String::new();
    BufReader::new(owner.0.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let mut watcher = DetachedWatchdog(line.trim().parse().unwrap());
    owner.0.kill().unwrap();
    owner.0.wait().unwrap();
    owned.assert_gone();
    assert!(unrelated.child.try_wait().unwrap().is_none());
    let deadline = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(watcher.0, 0) } == 0 {
        assert!(Instant::now() < deadline, "orphaned watchdog did not exit");
        thread::sleep(Duration::from_millis(20));
    }
    watcher.0 = 0;
}

#[test]
fn watchdog_detects_owner_already_gone_at_startup() {
    let mut owned = Group::new();
    let mut unrelated = Group::new();
    let mut prior_owner = Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let expected_parent = prior_owner.id();
    prior_owner.wait().unwrap();
    let mut watcher = OwnedProcess(
        Command::new(helper())
            .arg("--watch-driver-group")
            .arg(owned.group.to_string())
            .arg(expected_parent.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    owned.assert_gone();
    assert!(watcher.0.wait().unwrap().success());
    assert!(unrelated.child.try_wait().unwrap().is_none());
}

#[test]
fn invalid_group_is_rejected_without_touching_other_processes() {
    let mut unrelated = Group::new();
    let output = Command::new(helper())
        .args(["--watch-driver-group", "0", &std::process::id().to_string()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(unrelated.child.try_wait().unwrap().is_none());
}
