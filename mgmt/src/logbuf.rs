// logbuf.rs — the last log lines in memory, for the Logs page: an admin without SSH sees what the server did (client
// boots, publishes, DHCP/TFTP, errors) in the browser. A copy of what goes to stdout/stderr (main.rs tees the log
// writer into this one), the newest KEEP lines, each with a sequence number so the page asks only for what is new.
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::sync::LazyLock;

use axum::{Json, Router, extract::Query, routing::get};
use serde::Deserialize;

use crate::SharedState;

/// Lines kept (~150 bytes each → a few hundred KB).
const KEEP: usize = 2000;

struct Ring {
    next: u64,
    lines: VecDeque<(u64, String)>,
}

static RING: LazyLock<Mutex<Ring>> = LazyLock::new(|| {
    Mutex::new(Ring {
        next: 0,
        lines: VecDeque::new(),
    })
});

pub fn routes() -> Router<SharedState> {
    Router::new().route("/api/logs", get(logs))
}

/// Remove terminal color codes (ESC [ … letter): the log is colored when the server runs on a terminal.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\u{1b}' && it.peek() == Some(&'[') {
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Add the text of one log event (one line, or several).
pub fn push(text: &str) {
    let mut r = RING.lock();
    for line in text.lines() {
        let line = strip_ansi(line);
        if line.trim().is_empty() {
            continue;
        }
        r.next += 1;
        let seq = r.next;
        r.lines.push_back((seq, line));
        if r.lines.len() > KEEP {
            r.lines.pop_front();
        }
    }
}

/// The level word of a line as tracing writes it (`<time>  WARN message`): looked for near the start only.
fn level_of(line: &str) -> u8 {
    // On a char boundary: messages carry non-ASCII ("—", hostnames).
    let head = line
        .char_indices()
        .nth(48)
        .map_or(line, |(i, _)| &line[..i]);
    if head.contains(" ERROR ") {
        3
    } else if head.contains(" WARN ") {
        2
    } else {
        1
    }
}

/// Lines after `after`, at least `min` (1 all, 2 warn+, 3 error), containing `q` (any case). Also the newest seq.
pub fn query(after: u64, min: u8, q: &str) -> (Vec<(u64, String)>, u64) {
    let q = q.to_lowercase();
    let r = RING.lock();
    let lines = r
        .lines
        .iter()
        .filter(|(s, l)| {
            *s > after && level_of(l) >= min && (q.is_empty() || l.to_lowercase().contains(&q))
        })
        .cloned()
        .collect();
    (lines, r.next)
}

/// `tracing_subscriber` writer: the event's formatted text is collected, then added as lines when the writer is
/// dropped (one writer per event).
pub struct Make;

pub struct Writer(Vec<u8>);

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Make {
    type Writer = Writer;
    fn make_writer(&'a self) -> Writer {
        Writer(Vec::new())
    }
}

impl std::io::Write for Writer {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            push(&String::from_utf8_lossy(&self.0));
        }
    }
}

#[derive(Deserialize)]
struct LogQuery {
    #[serde(default)]
    after: u64,
    /// "warn" | "error" | anything else = all.
    #[serde(default)]
    level: String,
    #[serde(default)]
    q: String,
}

/// GET /api/logs?after=&level=&q= — {lines: [{seq, text}], last}.
async fn logs(Query(p): Query<LogQuery>) -> Json<serde_json::Value> {
    let min = match p.level.as_str() {
        "error" => 3,
        "warn" => 2,
        _ => 1,
    };
    let (lines, last) = query(p.after, min, p.q.trim());
    Json(serde_json::json!({
        "lines": lines.iter().map(|(s, t)| serde_json::json!({"seq": s, "text": t})).collect::<Vec<_>>(),
        "last": last,
    }))
}

#[cfg(test)]
#[path = "../tests/unit/logbuf.rs"]
mod tests;
