use anyhow::{Context, Result, bail};
use std::{io, time::Duration};
use tokio::process::Child;

const GRACE: Duration = Duration::from_secs(1);
const REAP: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(25);

fn signal_group(group: i32, signal: i32) -> Result<bool> {
    // The caller owns a group created with process_group(0). Never signal the
    // helper's own group, PID 1, or the current group through kill(0, ...).
    if group <= 1 || group == unsafe { libc::getpgrp() } {
        bail!("Invalid dedicated browser process group");
    }
    if unsafe { libc::kill(-group, signal) } == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    Err(error).context("Cannot signal dedicated browser process group")
}

async fn wait_group(group: i32, duration: Duration) -> Result<bool> {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        if !signal_group(group, 0)? {
            return Ok(true);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
        tokio::time::sleep(POLL).await;
    }
}

async fn terminate_group(group: i32) -> Result<()> {
    if !signal_group(group, libc::SIGTERM)? || wait_group(group, GRACE).await? {
        return Ok(());
    }
    signal_group(group, libc::SIGKILL)?;
    if !wait_group(group, REAP).await? {
        bail!("Dedicated browser process group did not exit after forced cleanup");
    }
    Ok(())
}

pub async fn stop_driver(driver: &mut Child, group: i32) -> Result<()> {
    // Reap our direct child while checking the whole group. A zombie leader
    // otherwise keeps the group present even after all processes have exited.
    let (cleanup, reaped) = tokio::join!(
        terminate_group(group),
        tokio::time::timeout(GRACE + REAP, driver.wait())
    );
    cleanup?;
    reaped
        .context("Timed out reaping dedicated ChromeDriver")?
        .context("Cannot reap dedicated ChromeDriver")?;
    Ok(())
}

pub fn force_stop(group: i32) {
    // Drop cannot await graceful cleanup. This fallback covers cancellation and
    // startup errors; the Tokio Child still owns reaping its direct process.
    if let Err(error) = signal_group(group, libc::SIGKILL) {
        eprintln!("Dedicated browser emergency cleanup failed: {error}");
    }
}

pub async fn watch_driver(group: i32, expected_parent: i32) -> Result<()> {
    if expected_parent <= 1 {
        bail!("Invalid browser owner process");
    }
    // The spawning helper supplies its PID. Capturing getppid() here could
    // capture PID 1 if the helper died before this process was scheduled.
    signal_group(group, 0)?;
    loop {
        if unsafe { libc::getppid() } != expected_parent {
            return terminate_group(group).await;
        }
        if !signal_group(group, 0)? {
            return Ok(());
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::process::{CommandExt, ExitStatusExt},
        process::Stdio,
    };
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    async fn close_escalates_and_reaps_a_term_resistant_driver() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "trap '' TERM; printf 'ready\\n'; exec /bin/sleep 60"])
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let mut driver = command.spawn().unwrap();
        let group = driver.id().unwrap() as i32;
        let mut line = String::new();
        BufReader::new(driver.stdout.take().unwrap())
            .read_line(&mut line)
            .await
            .unwrap();
        assert_eq!(line.trim(), "ready");
        stop_driver(&mut driver, group).await.unwrap();
        assert_eq!(
            driver.try_wait().unwrap().unwrap().signal(),
            Some(libc::SIGKILL)
        );
        assert!(!signal_group(group, 0).unwrap());
    }

    #[tokio::test]
    async fn close_allows_graceful_termination_and_reaps_driver() {
        let mut command = tokio::process::Command::new("/bin/sh");
        command
            .args(["-c", "printf 'ready\\n'; exec /bin/sleep 60"])
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let mut driver = command.spawn().unwrap();
        let group = driver.id().unwrap() as i32;
        let mut line = String::new();
        BufReader::new(driver.stdout.take().unwrap())
            .read_line(&mut line)
            .await
            .unwrap();
        assert_eq!(line.trim(), "ready");
        stop_driver(&mut driver, group).await.unwrap();
        assert_eq!(
            driver.try_wait().unwrap().unwrap().signal(),
            Some(libc::SIGTERM)
        );
        assert!(!signal_group(group, 0).unwrap());
    }
}
