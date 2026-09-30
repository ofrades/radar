//! Repro probe for the GUI SIGABRT storm (2026-09-30 16:19-17:08): attach to
//! every live daemon session READ-ONLY (no input, no resize), decode each
//! snapshot the way TerminalView::load_snapshot does, run the render path, then
//! feed the live output stream in GUI-sized chunks and re-render. A panic or
//! abort here is the crash.

use radar::ghostty::render::RenderState;
use radar::ghostty::Terminal;
use radar::session::daemon::{Client, Command, Response};
use radar::session::registry::{Lifecycle, Output};
use std::io;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn data_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("RADAR_HOME") {
        return PathBuf::from(home);
    }
    let xdg = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".local/share")
        });
    xdg.join("radar")
}

fn main() {
    let home = data_dir();
    let sessions = match Client::request(&home, Command::List) {
        Ok(Response::Sessions(list)) => list,
        Ok(other) => {
            eprintln!("unexpected list response: {other:?}");
            return;
        }
        Err(error) => {
            eprintln!("cannot list sessions (daemon down?): {error}");
            return;
        }
    };
    println!("{} sessions", sessions.len());
    for status in sessions {
        if !matches!(status.lifecycle, Lifecycle::Running) {
            println!("  skip {} ({:?})", status.id, status.lifecycle);
            continue;
        }
        println!(
            "attaching {} ({}): {}",
            status.id,
            status.cwd.display(),
            status.title.as_deref().unwrap_or("")
        );
        if let Err(error) = probe_session(&home, &status.id) {
            eprintln!("  PROBE FAILED: {error}");
        }
    }
    println!("done, no crash");
}

fn probe_session(home: &std::path::Path, id: &str) -> anyhow::Result<()> {
    let mut client = Client::connect(home, Command::Attach { id: id.to_owned() })?;
    let snapshot = match client.receive()? {
        Response::Snapshot(snapshot) => *snapshot,
        other => anyhow::bail!("unexpected attach response: {other:?}"),
    };
    println!(
        "  snapshot: {} bytes, dims {}x{}, sequence {}",
        snapshot.terminal_snapshot.len(),
        snapshot.dims.cols,
        snapshot.dims.rows,
        snapshot.sequence
    );

    // The GUI's load_snapshot path.
    let mut term = Terminal::from_snapshot(&snapshot.terminal_snapshot);
    let mut render = RenderState::new();
    let frame = render.frame(&term);
    println!(
        "  frame: {}x{}, {} lines, cursor {:?}",
        frame.cols,
        frame.rows,
        frame.lines.len(),
        frame.cursor.map(|c| (c.x, c.y))
    );

    // The GUI's feed path: raw bytes after the watermark, then a re-render
    // per chunk, like the draw callback does on queue_draw.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut chunks = 0usize;
    let mut bytes_total = 0usize;
    client.set_read_timeout(Some(Duration::from_millis(500)))?;
    while Instant::now() < deadline {
        match client.receive() {
            Ok(Response::Output(event)) if matches!(event.event, Output::Bytes(_)) => {
                if let Output::Bytes(bytes) = event.event {
                    term.write(&bytes);
                    bytes_total += bytes.len();
                    chunks += 1;
                    // Re-render at most every 4th chunk (the GUI coalesces
                    // draws per frame), and force the same resize call the
                    // widget does when the grid already matches (a no-op).
                    if chunks % 4 == 0 {
                        let frame = render.frame(&term);
                        let _ = frame;
                    }
                }
            }
            Ok(Response::Output(_)) => continue,
            Ok(other) => anyhow::bail!("unexpected stream response: {other:?}"),
            Err(error) => {
                // Read timeout just means no output in the window.
                let is_timeout = error
                    .downcast_ref::<io::Error>()
                    .map(|e| matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut))
                    .unwrap_or(false);
                if is_timeout {
                    continue;
                }
                return Err(error);
            }
        }
    }
    let frame = render.frame(&term);
    println!(
        "  stream: {chunks} chunks, {bytes_total} bytes, final frame {}x{}",
        frame.cols, frame.rows
    );
    drop(client);
    Ok(())
}
