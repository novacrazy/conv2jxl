use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::BTreeSet,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

use ratatui::style::Color;

use crate::cli::{Conv2JxlArgs, FileType, PerFileType};

pub mod conv2png;
pub mod noise;
pub mod report;

pub enum ConversionOutcome {
    Success(u64, u64),                    // input size, output size
    Warning(u64, u64, Cow<'static, str>), // input size, output size, warning message
    Skipped(Cow<'static, str>), // reason the file was skipped
    Error(Cow<'static, str>), // error message
    Inefficient(u64, u64),    // input size, output size
}

pub struct FileEntry {
    pub state: OnceLock<ConversionOutcome>,
    pub last_active: AtomicU64,
    pub path: PathBuf,
    pub ext: FileType,
    pub metadata: std::fs::Metadata,
}

impl FileEntry {
    pub fn new(path: PathBuf, ext: FileType, metadata: std::fs::Metadata) -> Self {
        Self {
            state: OnceLock::new(),
            last_active: AtomicU64::new(0),
            path,
            ext,
            metadata,
        }
    }

    pub fn set_state(&self, start: Instant, outcome: ConversionOutcome) -> u64 {
        let last_active = start.elapsed().as_millis() as u64;
        self.last_active.store(last_active, Ordering::Relaxed);
        let _ = self.state.set(outcome);
        last_active
    }
}

#[derive(Default)]
pub struct ConversionProgress {
    pub total: AtomicUsize,
    /// Files successfully processed
    pub processed: AtomicUsize,
    /// Files that encountered errors during processing
    pub errored: AtomicUsize,
    /// Files that were converted, but deemed inefficient (e.g., larger output size),
    /// and then reverted to the original format
    pub inefficient: AtomicUsize,
    /// Files that were skipped (output already exists, or filtered out by dimensions)
    pub skipped: AtomicUsize,
    /// Total bytes of input files before processing
    pub total_bytes: AtomicU64,
    /// Total bytes of input files processed so far
    pub input_bytes: AtomicU64,
    /// Total bytes of output files generated so far
    pub output_bytes: AtomicU64,
    /// Total elapsed time in milliseconds
    pub elapsed: AtomicU64,
    /// Exponentially-weighted moving average of recent per-file throughput,
    /// in bytes per millisecond of per-worker time. Used for the ETA so it
    /// tracks current conditions instead of being smothered by a lifetime
    /// average. `None` until the first sample arrives.
    pub speed_ewma: Mutex<Option<f64>>,
}

/// Weight given to each new sample when updating [`ConversionProgress::speed_ewma`].
/// At α = 0.1 the most recent ~10 files dominate the EWMA while still smoothing
/// over single-file outliers.
const EWMA_ALPHA: f64 = 0.1;

impl ConversionProgress {
    pub fn add(&self, input: u64, output: u64, elapsed: u64) {
        self.input_bytes.fetch_add(input, Ordering::Relaxed);
        self.output_bytes.fetch_add(output, Ordering::Relaxed);
        self.elapsed.fetch_add(elapsed, Ordering::Relaxed);
        self.processed.fetch_add(1, Ordering::Release);

        // Update the EWMA with this file's observed throughput. Skip zero
        // elapsed (dry-run with no delay, etc.) so an infinite sample can't
        // poison the average.
        if elapsed > 0 && input > 0 {
            let sample = input as f64 / elapsed as f64;
            let mut ewma = self.speed_ewma.lock().unwrap();
            *ewma = Some(match *ewma {
                None => sample,
                Some(prev) => EWMA_ALPHA * sample + (1.0 - EWMA_ALPHA) * prev,
            });
        }
    }

    pub fn errored(&self, size: u64) {
        self.errored.fetch_add(1, Ordering::Relaxed);
        self.total_bytes.fetch_sub(size, Ordering::Relaxed);
    }

    pub fn inefficient(&self, size: u64) {
        self.inefficient.fetch_add(1, Ordering::Relaxed);
        self.total_bytes.fetch_sub(size, Ordering::Relaxed);
    }

    pub fn skipped(&self, size: u64) {
        self.skipped.fetch_add(1, Ordering::Relaxed);
        self.total_bytes.fetch_sub(size, Ordering::Relaxed);
    }
}

pub struct ThreadState {
    pub file_idx: AtomicUsize,
    pub start_time: AtomicU64, // in milliseconds since program start
    /// Quality the current attempt is encoding at, so the UI can show when a
    /// file is being taken lossy rather than leaving that invisible until it
    /// finishes. [`QUALITY_UNSET`] until an attempt actually starts.
    pub quality: AtomicU8,
}

/// `quality` sentinel for a worker that has not begun an attempt yet.
pub const QUALITY_UNSET: u8 = u8::MAX;

pub struct ConversionState {
    pub excluded: usize,
    /// Append-only, lock-free, stable-indexed list of files to convert. In
    /// `--watch` mode the promoter thread pushes new entries here while workers
    /// read. `boxcar::Vec` keeps existing references valid across growth.
    pub files: boxcar::Vec<FileEntry>,
    pub idx: AtomicUsize,
    /// Pre-allocated slots for active threads to update
    pub active: Vec<ThreadState>,
    /// indices of files that encountered errors or inefficiencies during processing,
    /// kept in a btree for easy iteration in order (important for UI display)
    pub non_success: RwLock<BTreeSet<(Reverse<u64>, usize)>>, // (last_active, index)
    pub progress: PerFileType<Box<ConversionProgress>>,
    pub paused: Arc<(Mutex<bool>, Condvar)>,
    /// Set by [`ConversionState::stop`] to tell workers (and the watch threads)
    /// to exit. [`Self::wake`] is notified at the same time so a blocked
    /// worker sees it.
    pub shutdown: AtomicBool,
    /// Notified whenever new files are appended (or shutdown is set). Workers
    /// in watch mode block on this when they've outrun the current list.
    pub wake: (Mutex<()>, Condvar),
    /// Paths this process writes: every output, and the temp file behind it.
    ///
    /// Two jobs. Inserting a final output path is how a worker claims it, so
    /// two sources that map to the same output (`foo.png` and `foo.jpg` under
    /// `-X`) cannot both encode into it. And the `--watch` promoter treats an
    /// event on any of these as its own echo rather than as a new file to
    /// convert, which is what stops a run with `--ext png,jxl` from picking up
    /// every `.jxl` it just produced.
    pub produced: Mutex<std::collections::HashSet<PathBuf, foldhash::fast::FixedState>>,
    /// `--log` and `--error-log`, written as each file finishes.
    pub logs: report::Logs,
}

impl ConversionState {
    /// Claim `path` as this run's output. `false` if another source already
    /// claimed it, in which case the caller must not write there.
    pub fn claim_output(&self, path: &std::path::Path) -> bool {
        self.produced.lock().unwrap().insert(path.to_path_buf())
    }

    /// Record a path as our own before writing to it, so a watch event for it
    /// cannot race ahead of the record. For paths that cannot collide.
    pub fn mark_produced(&self, path: &std::path::Path) {
        self.claim_output(path);
    }

    /// Did we write this path ourselves?
    pub fn is_produced(&self, path: &std::path::Path) -> bool {
        self.produced.lock().unwrap().contains(path)
    }
}

pub struct SharedState {
    pub args: Conv2JxlArgs,
    pub conv: ConversionState,
    pub start: Instant,
}

pub enum App2 {
    Started(Conv2JxlArgs), // initial state, before scanning
    Scanning {
        args: Conv2JxlArgs,
        observer: Arc<scan::ScanObserver>,
    },
    Converting {
        shared: Arc<SharedState>,
        ui_state: ConvertingUIState,
    },
}

pub struct App {
    pub shared: Arc<SharedState>,
    pub ui_state: ConvertingUIState,
}

impl App {
    pub fn add_offset(&mut self, offset: i32) {
        // Clamp against the current tab, so the stored offset can never run
        // past the rows the tab has. The other direction (rows shrinking under
        // a large offset, on the Files tab) is handled at render time.
        let max = self.tab_len().saturating_sub(1);

        self.ui_state.list_offset = if offset < 0 {
            self.ui_state.list_offset.min(max).saturating_sub((-offset) as usize)
        } else {
            self.ui_state.list_offset.saturating_add(offset as usize).min(max)
        };
    }

    /// Rows the current tab can list. Each tab shows a different subset of
    /// the files, so the scroll offset has to be bounded per tab: bounding
    /// every tab by the pending-file count, as this used to, left the finished
    /// tabs unable to scroll once the queue drained.
    pub fn tab_len(&self) -> usize {
        let conv = &self.shared.conv;

        let sum = |count: fn(&ConversionProgress) -> &AtomicUsize| {
            conv.progress
                .iter()
                .map(|(_, p)| count(p).load(Ordering::Relaxed))
                .sum::<usize>()
        };

        match self.ui_state.file_tab {
            FileTab::Files => {
                let num_files = conv.files.count();
                num_files.saturating_sub(conv.idx.load(Ordering::Relaxed).min(num_files))
            }
            FileTab::Converted => sum(|p| &p.processed) + sum(|p| &p.skipped),
            FileTab::Errors => sum(|p| &p.errored),
            FileTab::Inefficient => sum(|p| &p.inefficient),
            // warnings are successes with a note, so no counter tracks them
            FileTab::Warnings => conv
                .non_success
                .read()
                .unwrap()
                .iter()
                .filter(|&&(_, i)| matches!(conv.files[i].state.get(), Some(ConversionOutcome::Warning(..))))
                .count(),
            FileTab::Breakdown => 0,
        }
    }

    pub fn toggle_pause(&mut self) {
        let (lock, cvar) = &*self.shared.conv.paused;
        let mut p = lock.lock().unwrap();

        *p = !*p;
        self.ui_state.paused = *p;

        if !*p {
            cvar.notify_all();
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTab {
    Files,
    Converted,
    Errors,
    Warnings,
    Inefficient,
    Breakdown,
}

pub struct ScanningUIState {
    pub list_offset: usize,
    pub time: u64,
    pub start: Instant,
}

pub struct ConvertingUIState {
    /// Using PageUp/PageDown to scroll the file list will set this offset.
    pub list_offset: usize,

    /// Last frame's processing indexes for each thread, used to check old files for errors.
    pub last_processing: Vec<usize>,

    /// Current time (at render) in milliseconds since program start.
    pub time: u64,

    pub file_tab: FileTab,

    pub details: bool,

    pub paused: bool,
}

impl FileTab {
    pub const ALL: [FileTab; 6] = [
        FileTab::Files,
        FileTab::Converted,
        FileTab::Errors,
        FileTab::Warnings,
        FileTab::Inefficient,
        FileTab::Breakdown,
    ];

    pub fn idx(self) -> usize {
        Self::ALL.iter().position(|&t| t == self).unwrap_or(0)
    }

    pub fn next(self) -> Self {
        Self::ALL[(self.idx() + 1) % Self::ALL.len()]
    }

    pub fn prev(self) -> Self {
        let current = self.idx();

        if current == 0 {
            Self::ALL[Self::ALL.len() - 1]
        } else {
            Self::ALL[current - 1]
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            FileTab::Files => "Files",
            FileTab::Converted => "Converted",
            FileTab::Errors => "Errors",
            FileTab::Warnings => "Warnings",
            FileTab::Inefficient => "Inefficient",
            FileTab::Breakdown => "Breakdown",
        }
    }

    pub fn accent_color(self) -> Color {
        match self {
            FileTab::Files => Color::White,
            FileTab::Converted => Color::Green,
            FileTab::Errors => Color::Red,
            FileTab::Warnings => Color::LightRed,
            FileTab::Inefficient => Color::Yellow,
            FileTab::Breakdown => Color::Blue,
        }
    }

    pub fn text_color(self) -> Color {
        match self {
            FileTab::Files => Color::Black,
            FileTab::Converted => Color::Black,
            FileTab::Errors => Color::White,
            FileTab::Warnings => Color::White,
            FileTab::Inefficient => Color::Black,
            FileTab::Breakdown => Color::White,
        }
    }
}

pub mod convert;
pub mod render;
pub mod scan;
pub mod watch;
