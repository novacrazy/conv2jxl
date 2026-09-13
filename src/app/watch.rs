//! Filesystem watcher for `--watch` mode. After the initial scan/convert,
//! this module runs a background thread that:
//!
//! 1. Holds a [`notify::RecommendedWatcher`] on each input directory.
//! 2. Buffers incoming events in a per-path "last event" map.
//! 3. After `--watch-debounce-ms` of quiet on a path, evaluates it against the
//!    same filters the scan uses (extension, size, regex, depth) and, if it
//!    matches, hands it to [`ConversionState::add_file`].
//!
//! Self-events from our own writes (truncating sources, etc.) are suppressed
//! by remembering every path we've already seen.
//!
//! Files this process writes are recorded in [`ConversionState::produced`] and
//! ignored when their events come back, so a run that converts into the same
//! directory it is watching does not feed on its own output.
//!
//! Limitations (acceptable for v1):
//! - Cancellation latency is bounded by the 100 ms event-poll timeout.

use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    path::{Path, PathBuf},
    str::FromStr as _,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::{
    app::{FileEntry, SharedState},
    cli::{Conv2JxlArgs, FileType},
};

/// Owns the watcher thread. Joining waits for the promoter to exit cleanly
/// (it polls `shared.conv.shutdown` every event-poll iteration).
pub struct Handle {
    thread: Option<JoinHandle<()>>,
    degraded: Arc<AtomicBool>,
}

impl Handle {
    /// Waits for the promoter to exit and reports whether the backend stopped
    /// watching part of the tree along the way. The caller prints that after
    /// tearing the TUI down, since there is nowhere to show it while it is up.
    pub fn join(mut self) -> bool {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }

        self.degraded.load(Ordering::Relaxed)
    }
}

/// A watcher that has been created and started receiving events but whose
/// promoter thread hasn't been spawned yet. Built by [`prepare`] before the
/// initial scan and consumed by [`start`] once the [`SharedState`] exists.
///
/// Holding this _during_ the scan means events fired while phase 1 is walking
/// the tree are buffered in the channel rather than lost, which closes the
/// race where a file created in an already-walked directory is missed.
pub struct PendingWatcher {
    watcher: RecommendedWatcher,
    rx: mpsc::Receiver<notify::Result<Event>>,
    roots: Vec<PathBuf>,
}

/// Create the watcher backend and start receiving events into a buffer. Call
/// this _before_ the scan so events that fire during phase 1 are not lost.
pub fn prepare(args: &Conv2JxlArgs) -> notify::Result<PendingWatcher> {
    let (ev_tx, ev_rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(ev_tx)?;

    let mode = if args.recurse {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };

    // Canonicalize each input dir so we can later compute depth from it by
    // simple prefix-stripping. Non-dirs and unreadable paths are silently
    // skipped, since the same paths would be useless to watch anyway.
    let mut roots: Vec<PathBuf> = Vec::new();
    let mut dirs = 0usize;

    for path in &args.paths {
        let Ok(canon) = path.canonicalize() else {
            continue;
        };
        if !canon.is_dir() {
            continue;
        }

        dirs += 1;

        match watcher.watch(&canon, mode) {
            Ok(()) => roots.push(canon),
            // Running out of watch descriptors is not a per-path problem: every
            // later root, and every directory created from here on, would fail
            // the same way, so half a watch is worse than a clear error.
            Err(e) if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) => {
                return Err(watch_limit_error(&canon));
            }
            // Best-effort watch. Permission failures etc. just mean we'll miss
            // events from that root.
            Err(_) => {}
        }
    }

    // Every directory the user asked to watch failed. --watch would sit there
    // doing nothing, so report it up front instead of leaving the user to
    // notice.
    if dirs > 0 && roots.is_empty() {
        return Err(notify::Error::generic("none of the given directories could be watched"));
    }

    Ok(PendingWatcher { watcher, rx: ev_rx, roots })
}

/// The OS ran out of file-watch descriptors.
///
/// Only inotify gets here in practice: a recursive watch on Linux costs one
/// watch per directory, and the cap is per _user_ and shared with every other
/// watcher that user is running. Windows watches a whole tree with a single
/// handle, so its recursion has no such per-directory price.
fn watch_limit_error(path: &Path) -> notify::Error {
    notify::Error::generic(&format!(
        concat!(
            "hit the OS file-watch limit while watching '{}'. On Linux a recursive watch needs one ",
            "inotify watch per directory, shared across every watcher you are running: raise the cap ",
            "(sudo sysctl -w fs.inotify.max_user_watches=524288), watch fewer directories, or drop ",
            "--recurse."
        ),
        path.display()
    ))
}

/// Spawn the promoter thread. Drains any events buffered since [`prepare`]
/// and then enters the normal event-poll loop. The promoter seeds its
/// known-paths set from the files the initial scan already found, so any
/// buffered event for a path the scan also picked up is silently deduped.
pub fn start(pending: PendingWatcher, shared: Arc<SharedState>) -> Handle {
    let PendingWatcher { watcher, rx, roots } = pending;

    let degraded = Arc::new(AtomicBool::new(false));

    let thread = std::thread::spawn({
        let degraded = degraded.clone();

        move || {
            run_promoter(shared, rx, watcher, roots, &degraded);
        }
    });

    Handle {
        thread: Some(thread),
        degraded,
    }
}

fn run_promoter(
    shared: Arc<SharedState>,
    rx: mpsc::Receiver<notify::Result<Event>>,
    _watcher: RecommendedWatcher, // owned so the watcher backend lives as long as we do
    roots: Vec<PathBuf>,
    degraded: &AtomicBool,
) {
    let debounce = Duration::from_millis(shared.args.watch_debounce_ms);
    // Cap at the debounce window so a quiet stream still gets evaluated
    // promptly, and don't go below 50 ms so we don't spin.
    let poll_timeout = debounce.min(Duration::from_millis(100)).max(Duration::from_millis(50));

    let filter = shared.args.filter.as_deref().and_then(|s| regex::Regex::new(s).ok());
    let exclude = shared.args.exclude.as_deref().and_then(|s| regex::Regex::new(s).ok());

    // Seed the known-paths set from whatever the initial scan found so that
    // any subsequent event on those paths (e.g. truncate-after-convert) is
    // ignored as a self-event.
    let mut known: HashSet<PathBuf, foldhash::fast::FixedState> =
        HashSet::with_hasher(foldhash::fast::FixedState::default());
    for (_, entry) in shared.conv.files.iter() {
        known.insert(entry.path.clone());
    }

    let mut pending: HashMap<PathBuf, Instant, foldhash::fast::FixedState> =
        HashMap::with_hasher(foldhash::fast::FixedState::default());

    loop {
        if shared.conv.shutdown.load(Ordering::Relaxed) {
            break;
        }

        match rx.recv_timeout(poll_timeout) {
            Ok(Ok(event)) => record_event(event, &mut pending, &known, &shared),
            // A backend error is not fatal. Hitting the watch limit is worse:
            // the backend has stopped watching for new directories entirely,
            // so record it for the post-run report.
            Ok(Err(e)) => {
                if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) {
                    degraded.store(true, Ordering::Relaxed);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Promote any path whose last event was long enough ago.
        let now = Instant::now();
        pending.retain(|path, last_event| {
            if now.duration_since(*last_event) < debounce {
                return true;
            }

            if let Some(entry) = build_entry(path, &shared.args, &roots, &filter, &exclude) {
                known.insert(entry.path.clone());
                shared.conv.add_file(entry);
            }

            false // drop from pending whether we accepted it or not
        });
    }
}

fn record_event(
    event: Event,
    pending: &mut HashMap<PathBuf, Instant, foldhash::fast::FixedState>,
    known: &HashSet<PathBuf, foldhash::fast::FixedState>,
    shared: &SharedState,
) {
    // We treat any Create or Modify (including rename-to) as "this path may
    // have a new/finished file". Remove and folder events are filtered out by
    // the post-debounce existence + is_file check.
    if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
        return;
    }

    let now = Instant::now();
    for path in event.paths {
        // Skip what we already have, and what we wrote ourselves: a conversion
        // output is not new work, and queueing it would mean re-encoding every
        // file we produce (and, with `--ext jxl`, doing it in place).
        if known.contains(&path) || shared.conv.is_produced(&path) {
            continue;
        }
        pending.insert(path, now);
    }
}

/// Run the same filter pipeline the scan uses on a path that became quiet.
/// Returns `Some(FileEntry)` iff it's eligible for conversion right now.
fn build_entry(
    path: &Path,
    args: &Conv2JxlArgs,
    roots: &[PathBuf],
    filter: &Option<regex::Regex>,
    exclude: &Option<regex::Regex>,
) -> Option<FileEntry> {
    let metadata = std::fs::metadata(path).ok()?;

    if !metadata.is_file() {
        return None; // directory / vanished / unsupported
    }

    let len = metadata.len();
    if !(args.min_size..=args.max_size).contains(&len) {
        return None;
    }

    let ext = path
        .extension()
        .and_then(OsStr::to_str)
        .and_then(|s| FileType::from_str(s).ok())?;

    if !args.extensions.contains(&ext) || args.skip_by_name(path, ext) {
        return None;
    }

    if let Some(s) = path.to_str() {
        if let Some(filter) = filter
            && !filter.is_match(s)
        {
            return None;
        }
        if let Some(exclude) = exclude
            && exclude.is_match(s)
        {
            return None;
        }
    }

    if !depth_in_range(path, roots, args.min_depth, args.max_depth) {
        return None;
    }

    Some(FileEntry::new(path.to_path_buf(), ext, metadata))
}

/// Depth of `path` relative to the watched root that contains it. Matches the
/// scan's convention: a file directly inside a watched root has depth 0.
fn depth_in_range(path: &Path, roots: &[PathBuf], min: u64, max: u64) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };

    // pick the longest root that prefixes this path, since the longest match
    // is the most-specific containing watch root
    let Some(suffix) = roots
        .iter()
        .filter_map(|root| parent.strip_prefix(root).ok())
        .max_by_key(|s| s.as_os_str().len())
    else {
        return false; // event for a path outside any watched root
    };

    let depth = suffix.components().count() as u64;
    (min..=max).contains(&depth)
}
