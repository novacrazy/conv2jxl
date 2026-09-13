//! Log files written as the run goes, and the summary printed once the TUI is
//! gone.
//!
//! The TUI shows errors while it is up and takes them with it on exit, so this
//! is the only record that survives the run: `--log` gets one line per file
//! outcome plus the summary, `--error-log` gets the error lines alone. Lines
//! are written whole and unbuffered, so a crash loses nothing already recorded.

use std::{
    fmt::{self, Write as _},
    fs::{File, OpenOptions},
    io::Write as _,
    path::Path,
    sync::{Mutex, atomic::Ordering},
};

use crate::{
    cli::Conv2JxlArgs,
    formatting::{Bytes, TimeBreakdown, Timestamp},
};

use super::{ConversionOutcome, ConversionState, FileEntry};

/// Errors listed on the terminal after the run. Past this the list is a wall,
/// and the error log has all of them.
const MAX_LISTED_ERRORS: usize = 25;

#[derive(Default)]
pub struct Logs {
    all: Option<Mutex<File>>,
    errors: Option<Mutex<File>>,
}

impl Logs {
    /// Open (append) whichever log files were asked for. An unwritable path is
    /// reported here, before the TUI starts, rather than failing silently on
    /// every line.
    pub fn open(args: &Conv2JxlArgs) -> std::io::Result<Logs> {
        let open = |path: &Path| {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| std::io::Error::new(e.kind(), format!("{}: {e}", path.display())))
        };

        Ok(Logs {
            all: args.log.as_deref().map(open).transpose()?.map(Mutex::new),
            errors: args.error_log.as_deref().map(open).transpose()?.map(Mutex::new),
        })
    }

    /// Record a file's terminal outcome. Call after `set_state`, since the
    /// line is built from what is stored there.
    pub fn outcome(&self, src: &FileEntry) {
        if self.all.is_none() && self.errors.is_none() {
            return;
        }

        let Some(outcome) = src.state.get() else {
            return;
        };

        let line = format!("{} {}\n", Timestamp::now(), OutcomeLine(src, outcome));

        write_line(&self.all, &line);

        if matches!(outcome, ConversionOutcome::Error(_)) {
            write_line(&self.errors, &line);
        }
    }

    /// Free-form text to the main log only, one stamped line per input line.
    pub fn note(&self, text: impl fmt::Display) {
        if self.all.is_none() {
            return;
        }

        let mut buf = String::new();
        let stamp = Timestamp::now();

        for line in text.to_string().lines() {
            let _ = writeln!(buf, "{stamp} {line}");
        }

        write_line(&self.all, &buf);
    }
}

/// A failed log write must not stop the run, so it is dropped on the floor.
fn write_line(file: &Option<Mutex<File>>, line: &str) {
    if let Some(file) = file
        && let Ok(mut file) = file.lock()
    {
        let _ = file.write_all(line.as_bytes());
    }
}

struct OutcomeLine<'a>(&'a FileEntry, &'a ConversionOutcome);

impl fmt::Display for OutcomeLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let path = self.0.path.display().to_string();
        let path = path.trim_start_matches(r#"\\?\"#);

        let ratio = |input: u64, output: u64| output as f64 / input.max(1) as f64 * 100.0;

        match self.1 {
            ConversionOutcome::Success(input, output) => write!(
                f,
                "OK          {:>7.2}%  {} -> {}  '{path}'",
                ratio(*input, *output),
                Bytes(*input),
                Bytes(*output)
            ),
            ConversionOutcome::Warning(input, output, note) => write!(
                f,
                "WARN        {:>7.2}%  {} -> {}  '{path}' | {note}",
                ratio(*input, *output),
                Bytes(*input),
                Bytes(*output)
            ),
            ConversionOutcome::Inefficient(input, output) => write!(
                f,
                "INEFFICIENT {:>7.2}%  {} -> {}  '{path}'",
                ratio(*input, *output),
                Bytes(*input),
                Bytes(*output)
            ),
            ConversionOutcome::Skipped(reason) => write!(f, "SKIP        '{path}' | {reason}"),
            ConversionOutcome::Error(error) => write!(f, "ERROR       '{path}' | {error}"),
        }
    }
}

/// Totals over every file type, as of the moment it was taken.
pub struct Summary {
    pub total: usize,
    pub converted: usize,
    pub errored: usize,
    pub inefficient: usize,
    pub skipped: usize,
    pub input: u64,
    pub output: u64,
    pub elapsed_ms: f64,
}

impl Summary {
    pub fn done(&self) -> usize {
        self.converted + self.errored + self.inefficient + self.skipped
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let saved = self.input.saturating_sub(self.output);
        let saved_pct = if self.input > 0 { saved as f64 / self.input as f64 * 100.0 } else { 0.0 };

        writeln!(
            f,
            "Processed {}/{} files in {}: {} converted, {} errors, {} inefficient, {} skipped",
            self.done(),
            self.total,
            TimeBreakdown(self.elapsed_ms),
            self.converted,
            self.errored,
            self.inefficient,
            self.skipped
        )?;

        write!(
            f,
            "In: {} | Out: {} | Saved: {} ({saved_pct:.2}%)",
            Bytes(self.input),
            Bytes(self.output),
            Bytes(saved)
        )
    }
}

impl ConversionState {
    pub fn summary(&self, elapsed_ms: f64) -> Summary {
        let mut summary = Summary {
            total: self.files.count(),
            converted: 0,
            errored: 0,
            inefficient: 0,
            skipped: 0,
            input: 0,
            output: 0,
            elapsed_ms,
        };

        for (_, progress) in self.progress.iter() {
            summary.converted += progress.processed.load(Ordering::Relaxed);
            summary.errored += progress.errored.load(Ordering::Relaxed);
            summary.inefficient += progress.inefficient.load(Ordering::Relaxed);
            summary.skipped += progress.skipped.load(Ordering::Relaxed);
            summary.input += progress.input_bytes.load(Ordering::Relaxed);
            summary.output += progress.output_bytes.load(Ordering::Relaxed);
        }

        summary
    }

    /// Every file that ended in an error, in file order.
    pub fn errors(&self) -> impl Iterator<Item = (&FileEntry, &str)> {
        self.files.iter().filter_map(|(_, entry)| match entry.state.get() {
            Some(ConversionOutcome::Error(e)) => Some((entry, &**e)),
            _ => None,
        })
    }

    /// Print the end-of-run report to stdout and append it to the log. Call
    /// after the terminal has been restored, or it lands on the TUI's screen.
    pub fn report(&self, elapsed_ms: f64, stopped_early: bool) {
        let summary = self.summary(elapsed_ms);

        let mut head = String::new();

        if stopped_early {
            head.push_str("Stopped early.\n");
        }

        let _ = write!(head, "{summary}");

        let errors: Vec<String> = self
            .errors()
            .map(|(entry, error)| {
                let path = entry.path.display().to_string();
                format!("  '{}' | {error}", path.trim_start_matches(r#"\\?\"#))
            })
            .collect();

        // The log gets every error. The terminal gets a readable number.
        let mut log_text = head.clone();
        let mut screen_text = head;

        if !errors.is_empty() {
            log_text.push_str("\nErrors:");
            screen_text.push_str("\nErrors:");

            for (i, line) in errors.iter().enumerate() {
                log_text.push('\n');
                log_text.push_str(line);

                if i < MAX_LISTED_ERRORS {
                    screen_text.push('\n');
                    screen_text.push_str(line);
                }
            }

            if errors.len() > MAX_LISTED_ERRORS {
                let _ = write!(screen_text, "\n  ... and {} more", errors.len() - MAX_LISTED_ERRORS);

                if self.logs.errors.is_none() {
                    screen_text.push_str(" (use --error-log to keep all of them)");
                }
            }
        }

        self.logs.note(&log_text);

        println!("{screen_text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::FileType;

    fn entry() -> FileEntry {
        let path = std::path::PathBuf::from("Cargo.toml");
        let metadata = std::fs::metadata(&path).unwrap();
        FileEntry::new(path, FileType::PNG, metadata)
    }

    #[test]
    fn outcome_lines() {
        let e = entry();

        let line = |o| OutcomeLine(&e, &o).to_string();

        assert_eq!(
            line(ConversionOutcome::Success(1_000_000, 250_000)),
            "OK            25.00%  1.00 MB -> 250.00 KB  'Cargo.toml'"
        );
        assert_eq!(
            line(ConversionOutcome::Warning(1000, 1500, "noisy".into())),
            "WARN         150.00%  1.00 KB -> 1.50 KB  'Cargo.toml' | noisy"
        );
        assert_eq!(
            line(ConversionOutcome::Skipped("exists".into())),
            "SKIP        'Cargo.toml' | exists"
        );
        assert_eq!(
            line(ConversionOutcome::Error("boom".into())),
            "ERROR       'Cargo.toml' | boom"
        );
    }

    #[test]
    fn summary_display() {
        let s = Summary {
            total: 10,
            converted: 6,
            errored: 1,
            inefficient: 2,
            skipped: 1,
            input: 4_000_000,
            output: 3_000_000,
            elapsed_ms: 61_500.0,
        };

        assert_eq!(s.done(), 10);
        assert_eq!(
            s.to_string(),
            "Processed 10/10 files in 1min 1.50s: 6 converted, 1 errors, 2 inefficient, 1 skipped\n\
             In: 4.00 MB | Out: 3.00 MB | Saved: 1.00 MB (25.00%)"
        );
    }
}
