use anyhow::{Context, Result, bail};
use std::{
    io,
    os::{fd::OwnedFd, unix::process::CommandExt as _},
    process::Stdio,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::{Child, Command},
};

const GRACE: Duration = Duration::from_secs(1);
const REAP: Duration = Duration::from_secs(2);
const STARTUP: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(25);
const HELLO: u8 = b'W';
const LAUNCH: u8 = b'L';

fn signal_group(group: i32, signal: i32) -> Result<bool> {
    if group <= 1 || group == unsafe { libc::getpgrp() } {
        bail!("Invalid dedicated browser process group");
    }
    if unsafe { libc::kill(-group, signal) } == 0 {
        return Ok(true);
    }
    signal_error(signal, io::Error::last_os_error())
}

fn signal_error(signal: i32, error: io::Error) -> Result<bool> {
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(false);
    }
    // A denied presence probe proves presence; real signal errors still fail.
    if signal == 0 && error.raw_os_error() == Some(libc::EPERM) {
        return Ok(true);
    }
    Err(error).context("Cannot signal dedicated browser process group")
}

async fn wait_group_gone(group: i32) -> Result<()> {
    let deadline = tokio::time::Instant::now() + REAP;
    while signal_group(group, 0)? {
        if tokio::time::Instant::now() >= deadline {
            bail!("Dedicated browser process group did not exit after forced cleanup");
        }
        tokio::time::sleep(POLL).await;
    }
    Ok(())
}

/// The supervisor is the sole owner of the direct driver and its group. Keep
/// the leader unreaped until the last destructive group signal: its reserved
/// PID prevents a late cleanup from targeting a reused, unrelated group ID.
struct OwnedDriver {
    child: Child,
    group: i32,
}

impl OwnedDriver {
    fn spawn(mut command: Command) -> Result<Self> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        command.as_std_mut().process_group(0);
        let child = command
            .spawn()
            .context("Cannot start the dedicated ChromeDriver")?;
        let group = child
            .id()
            .context("Dedicated ChromeDriver has no process ID")? as i32;
        Ok(Self { child, group })
    }

    fn exited(&self) -> Result<bool> {
        // WNOWAIT observes our own child without releasing its PID reservation.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::waitid(
                libc::P_PID,
                self.group as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            return Err(io::Error::last_os_error())
                .context("Cannot inspect dedicated ChromeDriver");
        }
        Ok(unsafe { info.si_pid() } != 0)
    }

    async fn stop(&mut self) -> Result<()> {
        let group = self.group;
        if group == 0 {
            return Ok(());
        }
        let mut signal_failure = signal_group(group, libc::SIGTERM).err();
        if signal_failure.is_none() {
            // Do not reap during grace. Even an exited leader reserves the group
            // identifier while descendants finish, so escalation remains owned.
            tokio::time::sleep(GRACE).await;
            signal_failure = signal_group(group, libc::SIGKILL).err();
        }
        if let Some(error) = &signal_failure {
            // macOS returns EPERM for an all-zombie group. This is not proof of
            // signal delivery: only an exited owned leader permits reaping and
            // independent absence verification. A surviving group stays failed.
            let denied = error
                .downcast_ref::<io::Error>()
                .is_some_and(|error| error.raw_os_error() == Some(libc::EPERM));
            if !denied {
                return Err(signal_failure.context("Missing group signal failure")?);
            }
            match self.exited() {
                Ok(true) => {}
                Ok(false) => return Err(signal_failure.context("Missing group signal failure")?),
                Err(observation_error) => {
                    return Err(signal_failure
                        .context("Missing group signal failure")?
                        .context(observation_error));
                }
            }
        }
        // No more destructive group signals after reaping starts, including
        // cancellation of this future. Tokio still owns direct-child reaping.
        self.group = 0;
        let cleanup = async {
            tokio::time::timeout(REAP, self.child.wait())
                .await
                .context("Timed out reaping dedicated ChromeDriver")?
                .context("Cannot reap dedicated ChromeDriver")?;
            wait_group_gone(group).await
        }
        .await;
        match (signal_failure, cleanup) {
            (Some(signal_error), Err(cleanup_error)) => Err(signal_error.context(cleanup_error)),
            (_, cleanup) => cleanup,
        }
    }
}

impl Drop for OwnedDriver {
    fn drop(&mut self) {
        if self.group > 1
            && let Err(error) = signal_group(self.group, libc::SIGKILL)
        {
            eprintln!("Dedicated browser emergency cleanup failed: {error}");
        }
    }
}

/// Parent-side ownership is the private socket, not a driver PID cache. Drop
/// closes it so an already-running supervisor finishes cleanup even if startup
/// was cancelled before the driver handshake. Never kill that owner early.
pub struct DriverSupervisor {
    child: Child,
    lifeline: Option<UnixStream>,
}

impl DriverSupervisor {
    pub async fn start(port: u16) -> Result<Self> {
        let (parent, child) = std::os::unix::net::UnixStream::pair()?;
        parent.set_nonblocking(true)?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([
                "--supervise-driver",
                &std::process::id().to_string(),
                &port.to_string(),
            ])
            .stdin(Stdio::from(OwnedFd::from(child)))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.as_std_mut().process_group(0);
        let child = command.spawn().context("Cannot start browser supervisor")?;
        // Command must not keep a duplicate of the child's socket endpoint.
        drop(command);
        let mut owner = Self {
            child,
            lifeline: Some(UnixStream::from_std(parent)?),
        };
        let startup = async {
            let socket = owner
                .lifeline
                .as_mut()
                .context("Missing browser lifeline")?;
            if socket.read_u8().await? != HELLO {
                bail!("Invalid browser supervisor readiness");
            }
            socket.write_u8(LAUNCH).await?;
            let group = socket.read_i32().await?;
            if group <= 1 || group == unsafe { libc::getpgrp() } {
                bail!("Invalid dedicated browser process group");
            }
            Ok(())
        };
        tokio::time::timeout(STARTUP, startup)
            .await
            .context("Browser supervisor startup timed out")??;
        Ok(owner)
    }

    pub fn exited(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_some())
    }

    pub async fn close(&mut self) -> Result<()> {
        self.lifeline.take();
        let status = tokio::time::timeout(GRACE + REAP + REAP + POLL, self.child.wait())
            .await
            .context("Browser supervisor cleanup did not finish; outcome is unknown")?
            .context("Cannot reap browser supervisor")?;
        if !status.success() {
            bail!("Browser supervisor reported incomplete cleanup");
        }
        Ok(())
    }
}

async fn owner_gone(expected_parent: i32) {
    while unsafe { libc::getppid() } == expected_parent {
        tokio::time::sleep(POLL).await;
    }
}

/// Internal mode, reached only on a private inherited socket. The supervisor
/// exists before driver creation and owns the driver before publishing its ID.
pub async fn supervise_driver(expected_parent: i32, port: u16) -> Result<()> {
    use std::os::fd::FromRawFd as _;

    if expected_parent <= 1 || unsafe { libc::getppid() } != expected_parent {
        bail!("Browser owner exited before supervisor startup");
    }
    if port == 0 {
        bail!("Invalid dedicated browser port");
    }
    let mut socket_type = 0_i32;
    let mut socket_type_len = std::mem::size_of::<i32>() as libc::socklen_t;
    if unsafe {
        libc::getsockopt(
            libc::STDIN_FILENO,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&mut socket_type as *mut i32).cast(),
            &mut socket_type_len,
        )
    } != 0
        || socket_type != libc::SOCK_STREAM
    {
        bail!("Browser supervisor requires its private socket");
    }
    let mut address: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let mut address_len = std::mem::size_of_val(&address) as libc::socklen_t;
    if unsafe {
        libc::getpeername(
            libc::STDIN_FILENO,
            (&mut address as *mut libc::sockaddr_storage).cast(),
            &mut address_len,
        )
    } != 0
        || address.ss_family as i32 != libc::AF_UNIX
    {
        bail!("Browser supervisor requires its connected private Unix socket");
    }
    // This mode exclusively owns stdin. No Tokio blocking-stdin thread may
    // survive owner death and delay runtime shutdown after driver cleanup.
    let socket = unsafe { std::os::unix::net::UnixStream::from_raw_fd(libc::STDIN_FILENO) };
    socket.set_nonblocking(true)?;
    let mut socket = UnixStream::from_std(socket)?;
    let launch = async {
        socket.write_u8(HELLO).await?;
        if socket.read_u8().await? != LAUNCH {
            bail!("Invalid browser supervisor launch request");
        }
        Ok(())
    };
    tokio::select! {
        biased;
        () = owner_gone(expected_parent) => return Ok(()),
        result = tokio::time::timeout(STARTUP, launch) => result.context("Browser launch request timed out")??,
    }
    if unsafe { libc::getppid() } != expected_parent {
        return Ok(());
    }
    let path = std::env::current_exe()?
        .parent()
        .context("Executable has no parent")?
        .join("chromedriver");
    let mut command = Command::new(path);
    command.args([
        format!("--port={port}"),
        "--allowed-ips=127.0.0.1".into(),
        "--log-level=OFF".into(),
    ]);
    let mut driver = OwnedDriver::spawn(command)?;
    let watch = async {
        socket.write_i32(driver.group).await?;
        let mut byte = [0_u8; 1];
        loop {
            tokio::select! {
                biased;
                () = owner_gone(expected_parent) => return Ok(()),
                result = socket.read(&mut byte) => {
                    if result? == 0 { return Ok(()); }
                    bail!("Invalid browser supervisor control message");
                }
                () = tokio::time::sleep(POLL) => {
                    if driver.exited()? { return Ok(()); }
                }
            }
        }
    };
    let result = watch.await;
    let cleanup = driver.stop().await;
    cleanup?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt as _;

    #[test]
    fn permission_denied_probe_means_present_but_real_signals_still_fail() {
        assert!(signal_error(0, io::Error::from_raw_os_error(libc::EPERM)).unwrap());
        for signal in [libc::SIGTERM, libc::SIGKILL] {
            assert!(signal_error(signal, io::Error::from_raw_os_error(libc::EPERM)).is_err());
        }
        for signal in [0, libc::SIGTERM, libc::SIGKILL] {
            assert!(!signal_error(signal, io::Error::from_raw_os_error(libc::ESRCH)).unwrap());
        }
        assert!(signal_error(0, io::Error::from_raw_os_error(libc::EINVAL)).is_err());
    }

    #[tokio::test]
    async fn close_escalates_and_reaps_a_term_resistant_driver() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "trap '' TERM; exec /bin/sleep 60"]);
        let mut driver = OwnedDriver::spawn(command).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        driver.stop().await.unwrap();
        assert_eq!(
            driver.child.try_wait().unwrap().unwrap().signal(),
            Some(libc::SIGKILL)
        );
    }

    #[tokio::test]
    async fn close_allows_graceful_termination_and_reaps_driver() {
        let mut command = Command::new("/bin/sleep");
        command.arg("60");
        let mut driver = OwnedDriver::spawn(command).unwrap();
        driver.stop().await.unwrap();
        assert_eq!(
            driver.child.try_wait().unwrap().unwrap().signal(),
            Some(libc::SIGTERM)
        );
    }

    #[tokio::test]
    async fn exited_driver_remains_unreaped_until_cleanup_reserves_group_identity() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exit 0"]);
        let mut driver = OwnedDriver::spawn(command).unwrap();
        let group = driver.group;
        tokio::time::timeout(REAP, async {
            while !driver.exited().unwrap() {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .unwrap();
        assert!(signal_group(group, 0).unwrap());
        driver.stop().await.unwrap();
        assert_eq!(driver.group, 0);
        assert!(!signal_group(group, 0).unwrap());
    }
}
