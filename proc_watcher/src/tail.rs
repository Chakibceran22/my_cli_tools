//! Follows growing log files on background threads, like `tail -f`, and
//! renders each line (colours, JSON log records) off the UI thread.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::Duration;

use crate::logs::Source;
use crate::render::{self, Segment};

/// How much of the existing file is shown when following starts.
const BACKLOG: u64 = 64 * 1024;
/// Wait between reads once the end of the file is reached.
const POLL: Duration = Duration::from_millis(200);
/// A "line" longer than this is flushed even without a newline.
const MAX_LINE: usize = 64 * 1024;

/// Sends every new line of `path` to `tx` until the receiver is dropped.
pub fn follow(path: PathBuf, source: Source, tx: Sender<(Source, Vec<Segment>)>) {
    thread::spawn(move || {
        let _ = run(&path, source, &tx);
    });
}

fn run(path: &Path, source: Source, tx: &Sender<(Source, Vec<Segment>)>) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut pos = file.metadata()?.len().saturating_sub(BACKLOG);
    file.seek(SeekFrom::Start(pos))?;
    // Starting mid-file almost always lands inside a line; drop that fragment.
    let mut skip_fragment = pos > 0;
    let mut pending = Vec::new();
    let mut chunk = vec![0; 64 * 1024];

    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            // Truncated, e.g. by `> app.log` on a restart: start from the top.
            if file.metadata()?.len() < pos {
                pos = 0;
                file.seek(SeekFrom::Start(0))?;
                pending.clear();
            }
            thread::sleep(POLL);
            continue;
        }
        pos += read as u64;
        pending.extend_from_slice(&chunk[..read]);

        let mut start = 0;
        while let Some(newline) = pending[start..].iter().position(|&b| b == b'\n') {
            let line = &pending[start..start + newline];
            start += newline + 1;
            if std::mem::take(&mut skip_fragment) {
                continue;
            }
            send(tx, source, line)?;
        }
        pending.drain(..start);
        if pending.len() > MAX_LINE {
            send(tx, source, &pending)?;
            pending.clear();
        }
    }
}

fn send(tx: &Sender<(Source, Vec<Segment>)>, source: Source, line: &[u8]) -> io::Result<()> {
    tx.send((source, render::line(line)))
        .map_err(|_| io::Error::from(io::ErrorKind::BrokenPipe))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::mpsc;

    #[test]
    fn follows_appended_lines() {
        let path = std::env::temp_dir().join(format!("procwatch-tail-{}.log", std::process::id()));
        std::fs::write(&path, "old line\n").unwrap();
        let (tx, rx) = mpsc::channel();
        follow(path.clone(), Source::Stdout, tx);

        let recv = || -> String {
            let (_, segments) = rx.recv_timeout(Duration::from_secs(2)).unwrap();
            segments.into_iter().map(|s| s.text).collect()
        };
        assert_eq!(recv(), "old line");
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"\x1b[32mnew\x1b[0m line\nhalf").unwrap();
        assert_eq!(recv(), "new line");
        file.write_all(b" done\n").unwrap();
        assert_eq!(recv(), "half done");
        std::fs::remove_file(path).unwrap();
    }
}
