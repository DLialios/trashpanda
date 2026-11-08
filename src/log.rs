//! The session log: a detailed, timestamped record of what the program did,
//! written for whoever (or whatever) debugs it rather than for its users.
//! People see short messages; the log keeps the whole story, including the
//! full chain of every error. Depot keys are never written to it.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// The most lines the session log holds, a few megabytes. A download on a bad
/// connection logs every failed request, so without a limit it could grow
/// by hundreds of megabytes.
const LIMIT: usize = 20_000;

static LOG: Mutex<Log> = Mutex::new(Log { lines: Vec::new(), head: 0, dropped: 0 });
static START: OnceLock<Instant> = OnceLock::new();

/// Lines kept to a limit: past it, the oldest lines after the first tenth are
/// dropped, so a long log keeps how the session began and how it is going.
#[derive(Default)]
pub struct Log {
    lines: Vec<String>,
    /// How many lines at the start are always kept.
    head: usize,
    /// How many lines were dropped after those.
    dropped: usize,
}

impl Log {
    pub fn push(&mut self, line: String, limit: usize) {
        if self.lines.len() >= limit {
            // Drop a quarter at once, so that moving the lines after them is rare.
            (self.head, self.dropped) = (limit / 10, self.dropped + limit / 4);
            self.lines.drain(self.head..self.head + limit / 4);
        }
        self.lines.push(line);
    }

    /// The lines, with a note where any were dropped.
    pub fn lines(&self) -> Vec<String> {
        let mut lines = self.lines.clone();
        if self.dropped > 0 {
            let note = format!("{:10}  ({} lines dropped here to keep the log small)", "", self.dropped);
            lines.insert(self.head, note);
        }
        lines
    }
}

fn log() -> MutexGuard<'static, Log> {
    // A panic elsewhere must not stop the log, least of all the panic's own entry.
    LOG.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Add a line, stamped with the seconds since the log began.
pub fn write(line: impl AsRef<str>) {
    let seconds = START.get_or_init(Instant::now).elapsed().as_secs_f64();
    // Later lines of a long entry are indented, so that every entry starts with its time.
    let line = format!("{seconds:10.3}  {}", line.as_ref().replace('\n', "\n            "));
    log().push(line, LIMIT);
}

pub fn lines() -> Vec<String> {
    log().lines()
}

/// Save the whole log to `path`.
pub fn save(path: &Path) -> io::Result<()> {
    std::fs::write(path, lines().join("\n") + "\n")
}

/// Begin the log with what program this is and how it was started, and log
/// panics from then on.
pub fn start(mode: &str) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |time| time.as_secs());
    write(format!("trashpanda {} for {}-{}, {mode}, started {}", env!("CARGO_PKG_VERSION"),
                  std::env::consts::OS, std::env::consts::ARCH, crate::download::datetime(now)));
    write(format!("args: {:?}", std::env::args_os().skip(1).collect::<Vec<_>>()));
    write(format!("program: {:?}", std::env::current_exe().unwrap_or_default()));
    write(format!("current directory: {:?}", std::env::current_dir().unwrap_or_default()));
    write(format!("cpus: {}", std::thread::available_parallelism().map_or(0, |n| n.get())));
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        write(format!("PANIC in thread {:?}: {info}", std::thread::current().name().unwrap_or("unnamed")));
        default(info);
    }));
}

/// Saves the log to a file when dropped, however the program ends.
pub struct SaveOnDrop(pub PathBuf);

impl Drop for SaveOnDrop {
    fn drop(&mut self) {
        if let Err(error) = save(&self.0) {
            eprintln!("Could not save the log to {}: {error}", self.0.display());
        }
    }
}

/// What went wrong, in a few words for people: the outermost description of
/// `error`, and its cause if that is a common one. The log keeps the rest.
pub fn brief(error: &anyhow::Error) -> String {
    let cause = error.chain().skip(1).find_map(|cause| {
        if let Some(error) = cause.downcast_ref::<ureq::Error>() {
            return Some(match error {
                ureq::Error::StatusCode(code) => format!("the server answered with error {code}"),
                ureq::Error::Timeout(_) => "the connection timed out".into(),
                ureq::Error::HostNotFound | ureq::Error::ConnectionFailed | ureq::Error::Io(_) => {
                    "could not connect; check the internet connection".into()
                }
                _ => "a network error".into(),
            });
        }
        Some(match cause.downcast_ref::<io::Error>()?.kind() {
            io::ErrorKind::NotFound => "it does not exist".into(),
            io::ErrorKind::PermissionDenied => "permission denied".into(),
            io::ErrorKind::StorageFull => "the drive is full".into(),
            io::ErrorKind::ReadOnlyFilesystem => "the drive is read-only".into(),
            kind => kind.to_string(),
        })
    });
    let text = match cause {
        Some(cause) => format!("{error}: {cause}"),
        None => error.to_string(),
    };
    let mut chars = text.chars();
    chars.next().map(|first| first.to_uppercase().chain(chars).collect()).unwrap_or_default()
}

/// `text` with every run of exactly 64 hex digits, a depot key, replaced by `<key>`.
pub fn redact_keys(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(|c: char| c.is_ascii_hexdigit()) {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(rest.len());
        out.push_str(if end == 64 { "<key>" } else { &rest[..end] });
        rest = &rest[end..];
    }
    out + rest
}

/// At most the first `max` bytes of `text`, cut at a character boundary.
pub fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
