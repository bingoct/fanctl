//! Runs external commands with a hard deadline.
//!
//! `Command::output()` has no timeout and `ipmitool` has none either, so a BMC
//! that stops answering would freeze the control loop forever. Everything that
//! spawns a process goes through [`run_checked`]; nothing else in the crate
//! touches [`std::process::Command`] directly.

use std::io::{self, Read, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Output, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::error::CommandError;

/// First pause between two `try_wait` calls.
const MIN_BACKOFF: Duration = Duration::from_millis(2);
/// Longest pause between two `try_wait` calls: bounded syscall count, bounded
/// reaction latency after the child exits.
const MAX_BACKOFF: Duration = Duration::from_millis(50);

/// Runs `cmd` to completion within `timeout`.
///
/// On expiry the child is killed **and reaped**, and both output readers are
/// joined before returning: a wedged BMC must not leave one process plus two
/// threads behind every cycle.
///
/// A non-zero exit status is turned into [`CommandError::Failed`], so callers
/// only ever see `Ok` for a successful run and cannot forget the status check.
pub fn run_checked(cmd: Command, timeout: Duration) -> Result<Output, CommandError> {
    run_inner(cmd, None, timeout, true)
}

/// Runs `cmd` with `stdin` fed to it, failing on a non-zero exit status.
pub fn run_checked_with_stdin(
    cmd: Command,
    stdin: &[u8],
    timeout: Duration,
) -> Result<Output, CommandError> {
    run_inner(cmd, Some(stdin), timeout, true)
}

/// Runs `cmd` with `stdin` fed to it and returns the output even when the child
/// exits non-zero.
///
/// For `ipmitool exec`: it exits 1 if *any* command in the list failed, while
/// still printing the output of the ones that worked. The batch reader needs
/// those bytes, and the separator protocol already tells it which command
/// failed, so the status is informational here.
pub fn run_capturing_with_stdin(
    cmd: Command,
    stdin: &[u8],
    timeout: Duration,
) -> Result<Output, CommandError> {
    run_inner(cmd, Some(stdin), timeout, false)
}

fn run_inner(
    mut cmd: Command,
    stdin: Option<&[u8]>,
    timeout: Duration,
    check_status: bool,
) -> Result<Output, CommandError> {
    let command = format!("{cmd:?}");
    let program = cmd.get_program().to_string_lossy().into_owned();

    cmd.stdin(if stdin.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|source| CommandError::Spawn { program, source })?;

    // stdin is written on its own thread for the same reason the readers are:
    // a child that reads slowly (or not at all) must not block us past the
    // deadline. Dropping the handle inside the thread closes the pipe, which is
    // what lets the child see EOF.
    let writer = match stdin.map(|bytes| (child.stdin.take(), bytes)) {
        None => None,
        Some((pipe, bytes)) => match spawn_writer(pipe, bytes) {
            Ok(handle) => Some(handle),
            Err(source) => {
                reap(&mut child);
                return Err(CommandError::Wait { command, source });
            }
        },
    };

    // Pipes are drained on helper threads: a child that writes more than the
    // pipe buffer holds would deadlock if we waited for it first.
    let stdout = match spawn_reader(child.stdout.take()) {
        Ok(handle) => handle,
        Err(source) => {
            reap(&mut child);
            join_writer(writer);
            return Err(CommandError::Wait { command, source });
        }
    };
    let stderr = match spawn_reader(child.stderr.take()) {
        Ok(handle) => handle,
        Err(source) => {
            reap(&mut child);
            let _ = stdout.join();
            join_writer(writer);
            return Err(CommandError::Wait { command, source });
        }
    };

    let status = match wait_for(&mut child, timeout, &command) {
        Ok(status) => status,
        Err(err) => {
            reap(&mut child);
            // The child is gone, so all three helpers now see EOF and finish.
            let _ = stdout.join();
            let _ = stderr.join();
            join_writer(writer);
            return Err(err);
        }
    };

    let stdout = join(stdout, &command)?;
    let stderr = join(stderr, &command)?;
    join_writer(writer);

    if status.success() || !check_status {
        return Ok(Output {
            status,
            stdout,
            stderr,
        });
    }

    Err(CommandError::Failed {
        command,
        status,
        details: failure_details(&stdout, &stderr),
    })
}

/// Waits for `child`, polling with a short backoff so the deadline is honoured
/// without busy waiting.
fn wait_for(
    child: &mut Child,
    timeout: Duration,
    command: &str,
) -> Result<ExitStatus, CommandError> {
    let deadline = Instant::now() + timeout;
    let mut backoff = MIN_BACKOFF;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(source) => {
                return Err(CommandError::Wait {
                    command: command.to_string(),
                    source,
                })
            }
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(CommandError::Timeout {
                command: command.to_string(),
                timeout,
            });
        }

        thread::sleep(backoff.min(deadline - now));
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Kills and reaps the child, ignoring failures: the caller is already on an
/// error path and a zombie is better than a panic.
fn reap(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Reads one pipe to the end on its own thread.
fn spawn_reader<R>(pipe: Option<R>) -> io::Result<JoinHandle<io::Result<Vec<u8>>>>
where
    R: Read + Send + 'static,
{
    thread::Builder::new()
        .name("fanctl-pipe".to_string())
        .spawn(move || {
            let mut buffer = Vec::new();
            if let Some(mut pipe) = pipe {
                pipe.read_to_end(&mut buffer)?;
            }
            Ok(buffer)
        })
}

/// Feeds the child's stdin on its own thread and closes it afterwards.
///
/// Write errors are dropped on purpose: a child that exits early closes the
/// pipe, and its stdout/stderr are still worth reading. The join in the caller
/// is what guarantees the pipe is closed and the thread is gone.
fn spawn_writer(pipe: Option<ChildStdin>, bytes: &[u8]) -> io::Result<JoinHandle<()>> {
    let bytes = bytes.to_vec();
    thread::Builder::new()
        .name("fanctl-stdin".to_string())
        .spawn(move || {
            if let Some(mut pipe) = pipe {
                let _ = pipe.write_all(&bytes);
                let _ = pipe.flush();
            }
        })
}

/// Waits for the stdin writer, if one was started.
fn join_writer(handle: Option<JoinHandle<()>>) {
    if let Some(handle) = handle {
        let _ = handle.join();
    }
}

/// Collects the bytes produced by a reader thread.
fn join(handle: JoinHandle<io::Result<Vec<u8>>>, command: &str) -> Result<Vec<u8>, CommandError> {
    let wait_error = |source| CommandError::Wait {
        command: command.to_string(),
        source,
    };

    match handle.join() {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(source)) => Err(wait_error(source)),
        Err(_) => Err(wait_error(io::Error::other(
            "output reader thread panicked",
        ))),
    }
}

/// Best available one-line reason for a failed command.
fn failure_details(stdout: &[u8], stderr: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr);
    let trimmed = stderr.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }

    let stdout = String::from_utf8_lossy(stdout);
    match stdout.lines().map(str::trim).find(|line| !line.is_empty()) {
        Some(line) => line.to_string(),
        None => "(no output)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::time::{Duration, Instant};

    use super::run_checked;

    #[cfg(unix)]
    use super::run_checked_with_stdin;
    use crate::error::CommandError;

    #[cfg(windows)]
    fn sleep_command(seconds: u32) -> Command {
        let mut command = Command::new("cmd");
        command.args(["/C", &format!("ping -n {seconds} 127.0.0.1 > NUL")]);
        command
    }

    #[cfg(unix)]
    fn sleep_command(seconds: u32) -> Command {
        let mut command = Command::new("sleep");
        command.arg(seconds.to_string());
        command
    }

    #[cfg(windows)]
    fn echo_command(text: &str) -> Command {
        let mut command = Command::new("cmd");
        command.args(["/C", &format!("echo {text}")]);
        command
    }

    #[cfg(unix)]
    fn echo_command(text: &str) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", &format!("echo {text}")]);
        command
    }

    fn exit_command(code: u32) -> Command {
        #[cfg(windows)]
        {
            let mut command = Command::new("cmd");
            command.args(["/C", &format!("exit {code}")]);
            command
        }
        #[cfg(unix)]
        {
            let mut command = Command::new("sh");
            command.args(["-c", &format!("exit {code}")]);
            command
        }
    }

    #[test]
    fn captures_stdout() {
        let output = run_checked(echo_command("hello"), Duration::from_secs(10)).unwrap();
        assert!(String::from_utf8_lossy(&output.stdout).contains("hello"));
    }

    #[test]
    fn non_zero_exit_is_an_error() {
        match run_checked(exit_command(3), Duration::from_secs(10)) {
            Err(CommandError::Failed { status, .. }) => assert_eq!(status.code(), Some(3)),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[test]
    fn missing_binary_is_reported_as_such() {
        let command = Command::new("fanctl-no-such-binary-anywhere");
        match run_checked(command, Duration::from_secs(10)) {
            Err(err) => assert!(err.is_missing_program(), "unexpected error: {err}"),
            Ok(_) => panic!("a missing binary must not succeed"),
        }
    }

    #[test]
    fn timeout_kills_the_child() {
        let started = Instant::now();
        let err = run_checked(sleep_command(30), Duration::from_millis(200)).unwrap_err();
        let elapsed = started.elapsed();

        assert!(matches!(err, CommandError::Timeout { .. }), "got {err}");
        assert!(
            elapsed < Duration::from_secs(10),
            "the child was not killed: waited {elapsed:?}"
        );
    }

    /// The batch reader depends on the child seeing EOF, which only happens
    /// when the writer thread drops the pipe.
    #[cfg(unix)]
    #[test]
    fn feeds_stdin_and_closes_it() {
        let output = run_checked_with_stdin(
            Command::new("cat"),
            b"raw 0x04 0x2d 0x01\n",
            Duration::from_secs(10),
        )
        .unwrap();

        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            "raw 0x04 0x2d 0x01\n"
        );
    }
}
