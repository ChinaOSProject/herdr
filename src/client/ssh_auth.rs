//! Ephemeral, client-owned PTY for interactive SSH authentication.
//!
//! No shell interpolation, output history, logging, or server state. The caller
//! owns rendering and must retry input when `write` returns `WouldBlock`.

use std::io::{self, Read, Write};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

const CHUNK_BYTES: usize = 4096;
const OUTPUT_CHUNKS: usize = 32;
const INPUT_CHUNKS: usize = 16;
const POLL_BYTES: usize = 64 * 1024;
const MAX_WRITE_BYTES: usize = 4096;

pub(super) struct AuthPoll {
    pub(super) output: Vec<u8>,
    /// Latched status, reported only after all output has been drained.
    pub(super) exit: Option<bool>,
}

pub(super) struct SshAuthProcess {
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    output: Option<mpsc::Receiver<io::Result<Vec<u8>>>>,
    input: Option<mpsc::SyncSender<Vec<u8>>>,
    cancelled: Arc<AtomicBool>,
    output_done: bool,
    exit: Option<bool>,
}

fn pty_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows: rows.max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

impl SshAuthProcess {
    /// Copies the program, literal argv, cwd, and explicit environment changes.
    /// Command stdio/process hooks are intentionally not used: the PTY owns them.
    /// As std does not expose env_clear, callers should use inherited environment
    /// plus explicit env/env_remove overrides rather than env_clear.
    pub(super) fn spawn(command: &Command, cols: u16, rows: u16) -> io::Result<Self> {
        let mut builder = CommandBuilder::new(command.get_program());
        builder.args(command.get_args());
        if let Some(cwd) = command.get_current_dir() {
            builder.cwd(cwd);
        }
        for (key, value) in command.get_envs() {
            if let Some(value) = value {
                builder.env(key, value);
            } else {
                builder.env_remove(key);
            }
        }
        builder.env("SSH_ASKPASS_REQUIRE", "never");
        let pair = native_pty_system()
            .openpty(size(cols, rows))
            .map_err(pty_error)?;
        crate::platform::configure_authentication_pty(pair.master.as_ref())?;
        let mut reader = pair.master.try_clone_reader().map_err(pty_error)?;
        let mut writer = pair.master.take_writer().map_err(pty_error)?;
        let child = pair.slave.spawn_command(builder).map_err(pty_error)?;
        drop(pair.slave);
        let (output_tx, output_rx) = mpsc::sync_channel(OUTPUT_CHUNKS);
        let (input_tx, input_rx) = mpsc::sync_channel::<Vec<u8>>(INPUT_CHUNKS);
        let cancelled = Arc::new(AtomicBool::new(false));
        // Construct the owner before starting threads so failures kill/reap too.
        let process = Self {
            master: pair.master,
            child,
            output: Some(output_rx),
            input: Some(input_tx),
            cancelled: cancelled.clone(),
            output_done: false,
            exit: None,
        };
        let reader_cancelled = cancelled.clone();
        std::thread::Builder::new()
            .name("ssh-auth-read".into())
            .spawn(move || {
                let mut buffer = [0; CHUNK_BYTES];
                while !reader_cancelled.load(Ordering::Acquire) {
                    let message = match reader.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => Ok(buffer[..count].to_vec()),
                        Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                        Err(error) => Err(error),
                    };
                    let failed = message.is_err();
                    // Dropping the receiver cancels even a blocked, full-queue send.
                    if output_tx.send(message).is_err() || failed {
                        break;
                    }
                }
            })?;
        std::thread::Builder::new()
            .name("ssh-auth-write".into())
            .spawn(move || {
                while let Ok(bytes) = input_rx.recv() {
                    if cancelled.load(Ordering::Acquire) || writer.write_all(&bytes).is_err() {
                        break;
                    }
                }
            })?;
        Ok(process)
    }

    /// Enqueues all bytes or none. Oversized chunks and a full queue return
    /// WouldBlock; split pastes into chunks of at most 4096 bytes before calling.
    pub(super) fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        if bytes.len() > MAX_WRITE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "PTY input chunk too large",
            ));
        }
        let input = self
            .input
            .as_ref()
            .ok_or_else(|| io::Error::from(io::ErrorKind::BrokenPipe))?;
        input.try_send(bytes.to_vec()).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => io::Error::from(io::ErrorKind::WouldBlock),
            mpsc::TrySendError::Disconnected(_) => io::Error::from(io::ErrorKind::BrokenPipe),
        })
    }

    pub(super) fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.master.resize(size(cols, rows)).map_err(pty_error)
    }

    /// Nonblocking; consumes at most 64 KiB per call without dropping output.
    pub(super) fn poll(&mut self) -> io::Result<AuthPoll> {
        if self.exit.is_none() {
            self.exit = self.child.try_wait()?.map(|status| status.success());
        }
        let mut output = Vec::new();
        if let Some(receiver) = &self.output {
            for _ in 0..POLL_BYTES / CHUNK_BYTES {
                match receiver.try_recv() {
                    Ok(Ok(bytes)) => output.extend_from_slice(&bytes),
                    Ok(Err(error)) => return Err(error),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.output_done = true;
                        break;
                    }
                }
            }
        }
        Ok(AuthPoll {
            output,
            exit: if self.output_done { self.exit } else { None },
        })
    }

    fn shutdown(&mut self) -> io::Result<()> {
        self.cancelled.store(true, Ordering::Release);
        self.output.take();
        self.input.take();
        // Use only the owned child handle, never a stored PID or process group.
        if !matches!(self.child.try_wait(), Ok(Some(_))) {
            let _ = self.child.kill();
        }
        self.child.wait()?;
        Ok(())
    }
}

impl Drop for SshAuthProcess {
    fn drop(&mut self) {
        let _ = self.shutdown();
        // Do not join potentially blocked OS I/O threads. Killing the terminal
        // child closes its slave, waking the I/O and releasing cloned handles.
        // Descendants retaining the slave can delay those threads' completion.
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn command(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    fn finish(process: &mut SshAuthProcess) -> (Vec<u8>, bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut output = Vec::new();
        loop {
            let poll = process.poll().unwrap();
            assert!(poll.output.len() <= POLL_BYTES);
            output.extend(poll.output);
            if let Some(success) = poll.exit {
                return (output, success);
            }
            assert!(Instant::now() < deadline, "PTY did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn buffers_output_before_reporting_exit() {
        let mut process = SshAuthProcess::spawn(
            &command("i=0; while [ $i -lt 20000 ]; do printf abcdefgh; i=$((i+1)); done"),
            80,
            24,
        )
        .unwrap();
        let (output, success) = finish(&mut process);
        assert!(success);
        assert_eq!(output, b"abcdefgh".repeat(20000));
    }

    #[test]
    fn forces_terminal_auth_and_reports_failure() {
        let mut command = command("printf '%s' \"$SSH_ASKPASS_REQUIRE\"; exit 7");
        command.env("SSH_ASKPASS_REQUIRE", "force");
        let mut process = SshAuthProcess::spawn(&command, 80, 24).unwrap();
        let (output, success) = finish(&mut process);
        assert_eq!(output, b"never");
        assert!(!success);
    }

    #[test]
    fn typeahead_is_never_echoed_before_child_prompt() {
        const SECRET: &[u8] = b"private-typeahead-7f36b9";
        // Deliberately read before printing any prompt: this makes input before
        // the prompt deterministic, without a timing-sensitive sleep. The child
        // never changes terminal flags, so only the parent's setup prevents echo.
        let mut process = SshAuthProcess::spawn(
            &command("IFS= read -r secret; printf 'Password: accepted'; [ \"$secret\" = private-typeahead-7f36b9 ]"),
            80,
            24,
        ).unwrap();
        let mut input = SECRET.to_vec();
        input.push(b'\n');
        process.write(&input).unwrap();
        let (output, success) = finish(&mut process);
        assert!(success, "child must receive the exact typeahead");
        assert!(!output.windows(SECRET.len()).any(|bytes| bytes == SECRET));
        assert_eq!(output, b"Password: accepted");
    }

    #[test]
    fn writes_and_resizes() {
        let mut process = SshAuthProcess::spawn(
            &command("read line; printf 'received:%s' \"$line\""),
            80,
            24,
        )
        .unwrap();
        process.resize(100, 30).unwrap();
        process.write(b"hello\n").unwrap();
        let (output, success) = finish(&mut process);
        assert!(success);
        assert!(output.windows(14).any(|s| s == b"received:hello"));
    }

    #[test]
    fn drop_reaps_owned_child_even_with_full_output_queue() {
        let mut process =
            SshAuthProcess::spawn(&command("while :; do printf abcdefgh; done"), 80, 24).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // Exercise the same cleanup as Drop, retaining the owned child handle
        // so its cached, reaped status can be checked without platform syscalls.
        process.shutdown().unwrap();
        assert!(process.child.try_wait().unwrap().is_some());
        assert!(process.output.is_none());
        assert!(process.input.is_none());
        drop(process);
    }
}
