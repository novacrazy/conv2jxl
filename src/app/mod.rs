use std::{
    borrow::Cow,
    cmp::Reverse,
    collections::BTreeSet,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, OnceLock, RwLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Instant,
};

use ratatui::style::Color;

use crate::cli::{Conv2JxlArgs, FileType, PerFileType};

pub mod conv2png;

pub enum ConversionOutcome {
    Success(u64, u64),                    // input size, output size
    Warning(u64, u64, Cow<'static, str>), // input size, output size, warning message
    Skipped,
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
    pub total: usize,
    /// Files successfully processed
    pub processed: AtomicUsize,
    /// Files that encountered errors during processing
    pub errored: AtomicUsize,
    /// Files that were converted, but deemed inefficient (e.g., larger output size),
    /// and then reverted to the original format
    pub inefficient: AtomicUsize,
    /// Total bytes of input files before processing
    pub total_bytes: AtomicU64,
    /// Total bytes of input files processed so far
    pub input_bytes: AtomicU64,
    /// Total bytes of output files generated so far
    pub output_bytes: AtomicU64,
    /// Total elapsed time in milliseconds
    pub elapsed: AtomicU64,
}

impl ConversionProgress {
    pub fn add(&self, input: u64, output: u64, elapsed: u64) {
        self.input_bytes.fetch_add(input, Ordering::Relaxed);
        self.output_bytes.fetch_add(output, Ordering::Relaxed);
        self.elapsed.fetch_add(elapsed, Ordering::Relaxed);
        self.processed.fetch_add(1, Ordering::Release);
    }

    pub fn errored(&self, size: u64) {
        self.errored.fetch_add(1, Ordering::Relaxed);
        self.total_bytes.fetch_sub(size, Ordering::Relaxed);
    }

    pub fn inefficient(&self, size: u64) {
        self.inefficient.fetch_add(1, Ordering::Relaxed);
        self.total_bytes.fetch_sub(size, Ordering::Relaxed);
    }
}

pub struct ThreadState {
    pub file_idx: AtomicUsize,
    pub start_time: AtomicU64, // in milliseconds since program start
}

pub struct ConversionState {
    pub excluded: usize,
    pub files: Vec<FileEntry>,
    pub idx: AtomicUsize,
    /// Pre-allocated slots for active threads to update
    pub active: Vec<ThreadState>,
    /// indices of files that encountered errors or inefficiencies during processing,
    /// kept in a btree for easy iteration in order (important for UI display)
    pub non_success: RwLock<BTreeSet<(Reverse<u64>, usize)>>, // (last_active, index)
    pub progress: PerFileType<Box<ConversionProgress>>,
    pub paused: Arc<(Mutex<bool>, Condvar)>,
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
        if offset < 0 {
            self.ui_state.list_offset = self.ui_state.list_offset.saturating_sub((-offset) as usize);
        } else {
            self.ui_state.list_offset = self
                .ui_state
                .list_offset
                .saturating_add(offset as usize)
                .min(self.shared.conv.files.len().saturating_sub(1));
        }
    }

    pub fn toggle_pause(&self) {
        let (lock, cvar) = &*self.shared.conv.paused;
        let mut p = lock.lock().unwrap();

        *p = !*p;

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
