//! Helper for running external commands with a hard deadline.

use std::io;
use std::process::{Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

/// Runs `cmd` to completion and returns its output, giving up after `timeout`.
///
/// The child is driven from a short lived helper thread: a BMC that stops
/// answering is a real failure mode, and [`Command::output`] has no timeout of
/// its own, so blocking the control loop would freeze fan regulation entirely.
///
/// A timed out child is **not** killed - the helper thread keeps waiting for it
/// and exits once the BMC answers again. Callers must therefore treat
/// [`io::ErrorKind::TimedOut`] as "result unknown, retry on the next cycle".
pub fn output_with_timeout(mut cmd: Command, timeout: Duration) -> io::Result<Output> {
    let description = format!("{cmd:?}");
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("fanctl-exec".to_string())
        .spawn(move || {
            // The receiver is gone once the deadline expired; that is expected.
            let _ = sender.send(cmd.output());
        })?;

    match receiver.recv_timeout(timeout) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("`{description}` did not finish within {timeout:?}"),
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::other(format!(
            "`{description}` finished without reporting a result"
        ))),
    }
}
