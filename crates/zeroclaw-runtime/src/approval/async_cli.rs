//! Cancellation-safe input for async CLI approval. The synchronous public API
//! retains its historical stdin fallback; the tool loop requires an owned
//! nonblocking terminal. Piped-only input and non-Unix platforms fail closed
//! as unavailable until they have an explicit owned async input backend.

use super::{ApprovalRequest, ApprovalResponse};
use std::io;

#[cfg(unix)]
const MAX_APPROVAL_LINE_BYTES: usize = 1024;

pub(super) async fn prompt(request: &ApprovalRequest) -> io::Result<ApprovalResponse> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Serialize only async approval prompts. The lock and descriptor are
        // owned by this future, so cancellation releases both immediately.
        static PROMPT: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _prompt = PROMPT.lock().await;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/tty")?;
        let terminal = tokio::io::unix::AsyncFd::new(file)?;
        // Write to the same owned terminal: stderr can be a full pipe, and a
        // synchronous eprint would block cancellation before the read starts.
        write_prompt(
            &terminal,
            super::format_cli_approval_prompt(request).as_bytes(),
        )
        .await?;
        read_line(terminal)
            .await
            .map(|line| super::parse_cli_approval_response(&line))
    }
    #[cfg(not(unix))]
    {
        let _ = request;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "cancellation-safe CLI approval requires an owned nonblocking terminal",
        ))
    }
}

#[cfg(unix)]
async fn write_prompt(
    terminal: &tokio::io::unix::AsyncFd<std::fs::File>,
    mut bytes: &[u8],
) -> io::Result<()> {
    use std::io::Write;
    while !bytes.is_empty() {
        let mut ready = terminal.writable().await?;
        match ready.try_io(|fd| {
            let mut file = fd.get_ref();
            file.write(bytes)
        }) {
            Ok(Ok(0)) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(Ok(written)) => bytes = &bytes[written..],
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => {} // WouldBlock cleared readiness; await the next event.
        }
    }
    Ok(())
}

#[cfg(unix)]
async fn read_line(terminal: tokio::io::unix::AsyncFd<std::fs::File>) -> io::Result<String> {
    use std::io::Read;
    let mut line = Vec::new();
    let mut overlong = false;
    let mut bytes_since_yield = 0;
    loop {
        // A single-byte read does not consume a later response from the same
        // terminal. O_NONBLOCK makes even a stale readiness event safe.
        let mut byte = [0];
        let mut ready = terminal.readable().await?;
        match ready.try_io(|fd| {
            let mut file = fd.get_ref();
            file.read(&mut byte)
        }) {
            Ok(Ok(0)) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(Ok(_)) => {
                if line.len() == MAX_APPROVAL_LINE_BYTES {
                    overlong = true;
                }
                if !overlong {
                    line.push(byte[0]);
                }
                if byte[0] == b'\n' {
                    if overlong {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "CLI approval response exceeds the line limit",
                        ));
                    }
                    return String::from_utf8(line)
                        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
                }
                // Keep line framing even on oversized input: a trailing "yes"
                // from this rejected line must not approve the next prompt.
                // Draining remains cancellable, bounded in memory, and fair to
                // cancellation even when input is continuously ready.
                bytes_since_yield += 1;
                if bytes_since_yield == 256 {
                    tokio::task::yield_now().await;
                    bytes_since_yield = 0;
                }
            }
            Ok(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => {} // WouldBlock cleared readiness; no polling loop.
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::time::Duration;
    use tokio::io::unix::AsyncFd;

    fn stream() -> (File, std::os::unix::net::UnixStream) {
        let (reader, writer) = std::os::unix::net::UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        let owned: OwnedFd = reader.into();
        (File::from(owned), writer)
    }

    #[tokio::test]
    async fn async_reader_drop_closes_owned_fd_and_leaves_next_response_for_resume() {
        let (reader, mut writer) = stream();
        let resumed = reader.try_clone().unwrap();
        let fd = reader.as_raw_fd();
        let mut wait = Box::pin(read_line(AsyncFd::new(reader).unwrap()));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut wait)
                .await
                .is_err()
        );
        drop(wait);
        // SAFETY: fcntl only queries our just-closed descriptor; no intervening
        // descriptor allocation occurs before this assertion.
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        writer.write_all(b"yes\n").unwrap();
        let line = tokio::time::timeout(
            Duration::from_secs(1),
            read_line(AsyncFd::new(resumed).unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            super::super::parse_cli_approval_response(&line),
            ApprovalResponse::Yes
        );
    }

    #[tokio::test]
    async fn async_reader_bounds_input_and_treats_eof_as_unavailable() {
        let (reader, mut writer) = stream();
        let resumed = reader.try_clone().unwrap();
        writer
            .write_all(&vec![b'a'; MAX_APPROVAL_LINE_BYTES + 1])
            .unwrap();
        writer.write_all(b"rejected-tail\nyes\n").unwrap();
        let error = read_line(AsyncFd::new(reader).unwrap()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            read_line(AsyncFd::new(resumed).unwrap()).await.unwrap(),
            "yes\n"
        );
        let (reader, writer) = stream();
        drop(writer);
        let error = read_line(AsyncFd::new(reader).unwrap()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[tokio::test]
    async fn async_terminal_readiness_cancels_without_changing_tty_mode() {
        let mut master_fd = -1;
        let mut slave_fd = -1;
        // SAFETY: openpty initializes two new owned descriptors. No process
        // terminal or stdin is inspected or changed by this private fixture.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master_fd,
                    &mut slave_fd,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );
        // SAFETY: each successful openpty output is owned exactly once here.
        let mut master = unsafe { File::from_raw_fd(master_fd) };
        let slave = unsafe { File::from_raw_fd(slave_fd) };
        // SAFETY: only this fixture's independently opened slave gets nonblocking I/O.
        let flags = unsafe { libc::fcntl(slave_fd, libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(
            unsafe { libc::fcntl(slave_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) },
            0
        );
        let resumed = slave.try_clone().unwrap();
        let mut before = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(resumed.as_raw_fd(), before.as_mut_ptr()) },
            0
        );
        let before = unsafe { before.assume_init() };
        let terminal = AsyncFd::new(slave).unwrap();
        write_prompt(&terminal, b"approval fixture> ")
            .await
            .unwrap();
        let mut wait = Box::pin(read_line(terminal));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut wait)
                .await
                .is_err()
        );
        drop(wait);
        master.write_all(b"always\n").unwrap();
        let mut after = std::mem::MaybeUninit::<libc::termios>::uninit();
        assert_eq!(
            unsafe { libc::tcgetattr(resumed.as_raw_fd(), after.as_mut_ptr()) },
            0
        );
        let after = unsafe { after.assume_init() };
        assert_eq!(before.c_lflag, after.c_lflag);
        assert_eq!(before.c_iflag, after.c_iflag);
        assert_eq!(before.c_oflag, after.c_oflag);
        assert_eq!(before.c_cflag, after.c_cflag);
        let line = tokio::time::timeout(
            Duration::from_secs(1),
            read_line(AsyncFd::new(resumed).unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            super::super::parse_cli_approval_response(&line),
            ApprovalResponse::Always
        );
    }
}
