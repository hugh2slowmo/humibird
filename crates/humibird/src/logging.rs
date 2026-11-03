//! Where the app writes what it did.
//!
//! A bundle started from Finder has no terminal, so stderr goes nowhere: a
//! report of "it broke" would arrive with nothing to read. Every record goes to
//! stderr as before *and* to `<profile>/logs/humibird.log`. The file keeps one
//! generation, so a crash loop cannot fill the disk.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use humibird_profile::Profile;

/// Past this size, the log is moved aside at startup and a fresh one begins.
const MAX_BYTES: u64 = 1 << 20;

pub fn init() {
    let requested = std::env::var("RUST_LOG")
        .ok()
        .and_then(|value| value.parse::<log::LevelFilter>().ok())
        .unwrap_or(log::LevelFilter::Warn);

    let file = match log_path().map(|path| open(&path)) {
        Some(Ok(file)) => Some(file),
        Some(Err(error)) => {
            eprintln!("humibird: could not open the log file: {error}");
            None
        }
        None => None,
    };
    let (most_verbose, stderr) = levels(requested, file.is_some());

    if log::set_logger(Box::leak(Box::new(Logger {
        file: Mutex::new(file),
        stderr,
    })))
    .is_err()
    {
        eprintln!("could not install the logger");
    }
    log::set_max_level(most_verbose);
}

/// `(most verbose record kept, most verbose record shown on stderr)`.
///
/// The file is the account of what happened, so it is at least at info even when
/// stderr is quiet: a report from a double-clicked app needs the breadcrumbs
/// ("sending …", "compacting before this turn") that a warning-only log drops.
fn levels(requested: log::LevelFilter, has_file: bool) -> (log::LevelFilter, log::LevelFilter) {
    if has_file {
        (requested.max(log::LevelFilter::Info), requested)
    } else {
        (requested, requested)
    }
}

/// The log of the profile this process belongs to. Absent when there is no home
/// directory to put a profile in, in which case stderr is all there is.
fn log_path() -> Option<PathBuf> {
    Profile::default_location().map(|profile| profile.root().join("logs").join("humibird.log"))
}

fn open(path: &Path) -> std::io::Result<File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(path).is_ok_and(|metadata| metadata.len() > MAX_BYTES) {
        let aside = path.with_extension("log.1");
        if let Err(error) = std::fs::rename(path, &aside) {
            // Starting a fresh file is worth more than the old one: keep going.
            eprintln!("humibird: could not move the old log aside: {error}");
        }
    }
    OpenOptions::new().create(true).append(true).open(path)
}

struct Logger {
    file: Mutex<Option<File>>,
    /// The most verbose record that also goes to stderr.
    stderr: log::LevelFilter,
}

impl log::Log for Logger {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let line = format!(
            "[{} {}] {}\n",
            record.level(),
            record.target(),
            record.args()
        );
        if record.level() <= self.stderr {
            self.write_stderr(&line);
        }
        self.write_file(&line);
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock()
            && let Some(writer) = file.as_mut()
            && let Err(error) = writer.flush()
        {
            eprintln!("humibird: could not flush the log file: {error}");
        }
    }
}

impl Logger {
    fn write_stderr(&self, line: &str) {
        let mut sink = std::io::stderr();
        if let Err(error) = sink.write_all(line.as_bytes()).and_then(|()| sink.flush()) {
            // `eprintln!` goes to the same broken place; there is nowhere left.
            eprintln!("humibird: could not write to stderr: {error}");
        }
    }

    fn write_file(&self, line: &str) {
        // A poisoned lock means a thread panicked while logging; the file is
        // still worth writing to.
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(writer) = file.as_mut() else {
            return;
        };
        if let Err(error) = writer
            .write_all(line.as_bytes())
            .and_then(|()| writer.flush())
        {
            // Stop trying rather than repeat this on every record.
            eprintln!("humibird: stopped writing the log file: {error}");
            *file = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_file_is_more_verbose_than_stderr() {
        assert_eq!(
            levels(log::LevelFilter::Warn, true),
            (log::LevelFilter::Info, log::LevelFilter::Warn),
            "a quiet stderr still leaves breadcrumbs in the file"
        );
        assert_eq!(
            levels(log::LevelFilter::Debug, true),
            (log::LevelFilter::Debug, log::LevelFilter::Debug),
            "a verbose request goes to both"
        );
        assert_eq!(
            levels(log::LevelFilter::Warn, false),
            (log::LevelFilter::Warn, log::LevelFilter::Warn),
            "without a file, nothing is kept that is not shown"
        );
    }

    #[test]
    fn the_log_directory_is_created() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("logs").join("humibird.log");

        let file = open(&path).expect("the log opens");

        assert!(path.is_file());
        assert_eq!(file.metadata().expect("metadata").len(), 0);
    }

    #[test]
    fn a_log_past_the_limit_is_moved_aside() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("humibird.log");
        std::fs::write(&path, vec![b'x'; (MAX_BYTES + 1) as usize]).expect("write the old log");

        open(&path).expect("the log opens");

        let aside = directory.path().join("humibird.log.1");
        assert!(aside.is_file(), "the old log is kept, once");
        assert_eq!(
            std::fs::metadata(&aside).expect("metadata").len(),
            MAX_BYTES + 1
        );
        assert_eq!(
            std::fs::metadata(&path).expect("metadata").len(),
            0,
            "and a fresh one starts"
        );
    }

    #[test]
    fn a_small_log_is_appended_to() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("humibird.log");
        std::fs::write(&path, "earlier\n").expect("write the old log");

        let mut file = open(&path).expect("the log opens");
        file.write_all(b"later\n").expect("append");

        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "earlier\nlater\n"
        );
        assert!(
            !directory.path().join("humibird.log.1").exists(),
            "a small log is not moved"
        );
    }
}
