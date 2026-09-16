//! Bounded local command ownership for the calendar adapter. All argv and
//! executables are assembled locally; no shell or caller-selected command.

use crate::common::{SafeResult, check};
use std::{
    io,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::{Child, Command},
};

struct UnreapedGroup(Option<i32>);

const CLEANUP: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);

fn signal_group(group: i32, signal: i32) -> io::Result<bool> {
    if unsafe { libc::kill(-group, signal) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ESRCH) => Ok(false),
        // EPERM on a presence probe proves presence, not absence.
        Some(libc::EPERM) if signal == 0 => Ok(true),
        _ => Err(error),
    }
}

impl UnreapedGroup {
    fn exited(&self) -> SafeResult<bool> {
        let group = self.0.ok_or("appointment_group_missing")?;
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // Observe only our direct child without releasing its PID reservation.
        if unsafe {
            libc::waitid(
                libc::P_PID,
                group as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            return Err("appointment_exit_observation_failed");
        }
        Ok(unsafe { info.si_pid() } != 0)
    }

    async fn finish(&mut self, child: &mut Child) -> SafeResult<ExitStatus> {
        let group = self.0.ok_or("appointment_group_missing")?;
        let deadline = tokio::time::Instant::now() + CLEANUP;
        if let Err(error) = signal_group(group, libc::SIGKILL) {
            // macOS may reject SIGKILL for an all-zombie group. Reaping is
            // allowed only with a proven exited leader; the later absence
            // check must still succeed before cleanup can report success.
            check(
                error.raw_os_error() == Some(libc::EPERM),
                "appointment_group_signal_failed",
            )?;
            // EPERM can precede the exit notification becoming observable.
            // Keep the PID reserved while allowing that bounded transition.
            tokio::time::timeout_at(deadline, async {
                while !self.exited()? {
                    tokio::time::sleep(POLL).await;
                }
                Ok(())
            })
            .await
            .unwrap_or(Err("appointment_group_signal_failed"))?;
        }
        // The last destructive signal precedes wait. Disarm BEFORE awaiting
        // reap so cancellation cannot target a reused PID/process-group ID.
        self.0 = None;
        tokio::time::timeout_at(deadline, async {
            let status = child.wait().await.map_err(|_| "appointment_wait_failed")?;
            while signal_group(group, 0).map_err(|_| "appointment_group_probe_failed")? {
                tokio::time::sleep(POLL).await;
            }
            Ok(status)
        })
        .await
        .unwrap_or(Err("appointment_cleanup_incomplete"))
    }
}

impl Drop for UnreapedGroup {
    fn drop(&mut self) {
        // Cancellation still sends the final group signal while the owned
        // leader remains unreaped. Child's kill-on-drop/reaper owns its wait.
        if let Some(group) = self.0 {
            let _ = signal_group(group, libc::SIGKILL);
        }
    }
}

pub async fn run(
    mut command: Command,
    input: Option<Vec<u8>>,
    max_bytes: usize,
    budget: Duration,
) -> SafeResult<Vec<u8>> {
    check(
        max_bytes > 0 && max_bytes <= 2 * 1024 * 1024,
        "appointment_output_limit_invalid",
    )?;
    check(
        input.as_ref().is_none_or(|bytes| bytes.len() <= 32 * 1024),
        "appointment_input_limit_invalid",
    )?;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command
        .spawn()
        .map_err(|_| "appointment_command_start_failed")?;
    // Declared after child: cancellation drops the group guard before Child.
    let mut group = UnreapedGroup(Some(child.id().ok_or("appointment_child_missing")? as i32));
    let deadline = tokio::time::Instant::now() + budget;
    let result = tokio::time::timeout_at(deadline, async {
        if let Some(bytes) = input {
            let mut stdin = child.stdin.take().ok_or("appointment_stdin_missing")?;
            stdin
                .write_all(&bytes)
                .await
                .map_err(|_| "appointment_input_failed")?;
            stdin
                .shutdown()
                .await
                .map_err(|_| "appointment_input_failed")?;
            drop(stdin);
        }
        let mut stdout = child.stdout.take().ok_or("appointment_stdout_missing")?;
        let mut bytes = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            let count = stdout
                .read(&mut chunk)
                .await
                .map_err(|_| "appointment_output_failed")?;
            if count == 0 {
                break;
            }
            check(
                bytes.len() + count <= max_bytes,
                "appointment_output_exceeded",
            )?;
            bytes.extend_from_slice(&chunk[..count]);
        }
        // Both pipe EOF and leader exit are required. Descendants may close
        // stdout and keep running, so success still goes through group cleanup.
        while !group.exited()? {
            tokio::time::sleep(POLL).await;
        }
        Ok(bytes)
    })
    .await
    .unwrap_or(Err("appointment_command_timeout"));
    let status = group.finish(&mut child).await?;
    let bytes = result?;
    check(status.success(), "appointment_command_failed")?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bounds_output_and_reaps_timeout_without_shell_arguments_from_callers() {
        let mut cat = Command::new("/bin/cat");
        cat.arg("-");
        assert_eq!(
            run(cat, Some(b"synthetic".to_vec()), 10, Duration::from_secs(2))
                .await
                .unwrap(),
            b"synthetic"
        );
        let cat = Command::new("/bin/cat");
        assert_eq!(
            run(cat, Some(vec![b'x'; 1024]), 16, Duration::from_secs(2))
                .await
                .unwrap_err(),
            "appointment_output_exceeded"
        );
        let mut sleep = Command::new("/bin/sleep");
        sleep.arg("20");
        let started = tokio::time::Instant::now();
        assert_eq!(
            run(sleep, None, 1024, Duration::from_millis(30))
                .await
                .unwrap_err(),
            "appointment_command_timeout"
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    fn descendant_command(pid_path: &std::path::Path, keep_leader: bool) -> Command {
        let mut command = Command::new("/bin/sh");
        // Fixed synthetic fixture only; production never accepts shell text.
        command.args([
            "-c",
            "/bin/sleep 30 >/dev/null 2>&1 & descendant=$!; printf '%s %s' \"$$\" \"$descendant\" > \"$1\"; if [ \"$2\" = keep ]; then exec /bin/sleep 30; fi; printf 'owned'; exit 0",
            "appointment_fixture",
        ]);
        command
            .arg(pid_path)
            .arg(if keep_leader { "keep" } else { "exit" });
        command
    }

    async fn fixture_pids(path: &std::path::Path) -> (i32, i32) {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(text) = tokio::fs::read_to_string(path).await {
                    let pids: Vec<i32> = text
                        .split_whitespace()
                        .filter_map(|part| part.parse().ok())
                        .collect();
                    if pids.len() == 2 {
                        return (pids[0], pids[1]);
                    }
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .unwrap()
    }

    async fn assert_group_reaped(leader: i32, descendant: i32) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let gone = [leader, descendant].into_iter().all(|pid| {
                    (unsafe { libc::kill(pid, 0) }) == -1
                        && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                });
                if gone && !signal_group(leader, 0).unwrap() {
                    break;
                }
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .expect("owned leader and descendant must exit and be reaped");
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(leader, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[tokio::test]
    async fn success_kills_descendant_that_closed_stdout_before_reaping_leader() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pids");
        assert_eq!(
            run(
                descendant_command(&path, false),
                None,
                1024,
                Duration::from_secs(3)
            )
            .await
            .unwrap(),
            b"owned"
        );
        let (leader, descendant) = fixture_pids(&path).await;
        assert_group_reaped(leader, descendant).await;
    }

    #[tokio::test]
    async fn dropping_pending_command_kills_group_and_reaps_owned_child() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pids");
        let command = descendant_command(&path, true);
        let task = zeroclaw_spawn::spawn!(async move {
            run(command, None, 1024, Duration::from_secs(30)).await
        });
        let (leader, descendant) = fixture_pids(&path).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert_group_reaped(leader, descendant).await;
    }

    #[tokio::test]
    async fn timeout_kills_descendant_before_reaping_owned_leader() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pids");
        assert_eq!(
            run(
                descendant_command(&path, true),
                None,
                1024,
                Duration::from_millis(100)
            )
            .await
            .unwrap_err(),
            "appointment_command_timeout"
        );
        let (leader, descendant) = fixture_pids(&path).await;
        assert_group_reaped(leader, descendant).await;
    }
}
