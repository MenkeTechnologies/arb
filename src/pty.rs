//! `spawn -pty CMD` support: run a spawn source on a pseudo-terminal so it acts
//! as if interactive, and hand back a writer to its stdin for the Expect `send`
//! action. Ported from the sibling `zwire-host` PTY session; cross-platform via
//! `portable-pty`.

use std::io::{self, BufRead, BufReader, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::stream::StreamState;

/// Run `cmd` via `sh -c` on a pseudo-terminal, feeding its output into `state`
/// line by line, and return a writer to its stdin (for `send`). The reader
/// thread owns the PTY master, keeping the session open until the child exits;
/// the PTY merges stdout and stderr, so there is no separate error pane.
pub fn spawn_pty_producer(
    cmd: &str,
    state: Arc<Mutex<StreamState>>,
) -> io::Result<Box<dyn Write + Send>> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    let wrap = |e: String| io::Error::other(e);
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| wrap(format!("openpty: {e}")))?;
    let mut builder = CommandBuilder::new("sh");
    builder.arg("-c");
    builder.arg(cmd);
    builder.env("TERM", "xterm-256color");
    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| wrap(format!("pty spawn: {e}")))?;
    drop(pair.slave);
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| wrap(format!("pty reader: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| wrap(format!("pty writer: {e}")))?;
    let master = pair.master;
    thread::spawn(move || {
        let _keep = master; // hold the PTY master open while the child runs
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            state
                .lock()
                .unwrap()
                .push(line.trim_end_matches('\r').to_string());
        }
        let _ = child.wait();
    });
    Ok(writer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn pty_round_trip_send_and_read() {
        // `cat` on a PTY echoes its input; write a line via the returned writer
        // and it should come back on the stream (proves both directions work).
        let state = Arc::new(Mutex::new(StreamState::new()));
        let mut writer = spawn_pty_producer("cat", state.clone()).expect("openpty");
        writer.write_all(b"hello-pty\n").unwrap();
        writer.flush().unwrap();
        // Poll the stream for the echoed line (the PTY reader thread is async).
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if state
                .lock()
                .unwrap()
                .lines
                .iter()
                .any(|l| l.contains("hello-pty"))
            {
                break;
            }
            assert!(Instant::now() < deadline, "PTY echo not seen within 5s");
            thread::yield_now();
        }
    }
}
