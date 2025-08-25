use std::collections::HashMap;
use std::error::Error;
use std::ffi::OsStr;
use std::os::windows::fs::FileTimesExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use arc_swap::ArcSwap;

mod enums;
use enums::{RecentField, SortMethod};

mod formatting;
use formatting::{format_bytes, format_time};

/// Convert files and directories to JPEG XL format.
#[derive(argh::FromArgs, Debug)]
pub struct Conv2JxlArgs {
    /// process subdirectories recursively
    #[argh(switch, short = 'r')]
    pub recurse: bool,

    /// maximum recursion depth. Default is unlimited.
    /// Only applies if --recurse is set.
    /// A depth of 0 means only the given directories, 1 means their direct subdirectories, etc.
    /// Note that --ignore-recent only applies to files in the direct subdirectories of the given paths, not further down.
    #[argh(option, default = "u64::MAX")]
    pub max_depth: u64,

    /// minimum recursion depth. Default is 0.
    /// Only applies if --recurse is set.
    /// A depth of 0 means only the given directories, 1 means their direct subdirectories, etc.
    #[argh(option, default = "0")]
    pub min_depth: u64,

    /// overwrite existing files.
    #[argh(switch, short = 'o')]
    pub overwrite: bool,

    /// delete original files after conversion.
    #[argh(switch, short = 'd')]
    pub delete: bool,

    /// conversion quality, from 0 to 100, where 100 is lossless.
    #[argh(option, short = 'q', default = "100")]
    pub quality: u8,

    /// effort level, from 0 to 9, where 0 is fastest and 9 is best quality.
    /// 10 exists, but uses too much memory for most systems.
    #[argh(option, short = 'e', default = "9")]
    pub effort: u8,

    /// ignore the N most recent files based on creation time in each directory.
    /// Only applies if --recurse is set.
    /// This is useful to avoid processing files that archive tools might have created recently.
    #[argh(option, default = "0")]
    pub ignore_recent: u32,

    /// field to use for sorting files. Default is "mtime" (modification time).
    /// Valid values are "mtime", "ctime", "atime" (modification time, creation time, access time).
    /// This is used to determine which files to ignore with `--ignore-recent`.
    #[argh(option, default = "RecentField::default()")]
    pub recent_field: RecentField,

    /// perform a trial run with no changes made, just print what would be done.
    #[argh(switch)]
    pub dry_run: bool,

    /// only convert files larger than this size in bytes. Default is 0 (no minimum).
    #[argh(option, short = 'm', default = "0")]
    pub min_size: u64,

    /// only convert files smaller than this size in bytes. Default is unlimited.
    #[argh(option, short = 'M', default = "u64::MAX")]
    pub max_size: u64,

    /// limit the number of files to convert. Default is no limit.
    #[argh(option, short = 'l')]
    pub limit: Option<usize>,

    /// use lossless recompression of JPEG files. Default is true.
    /// Set to false to allow lossy recompression of JPEG files.
    #[argh(option, short = 'j', default = "true")]
    pub lossless_jpeg: bool,

    /// enable JPEG reconstruction from JPEG XL files. Default is false,
    /// which saves some space by not storing data required to reconstruct the original JPEG.
    /// Only applies if --lossless-jpeg is set.
    #[argh(switch)]
    pub enable_jpeg_reconstruction: bool,

    /// filter input images as comma-separated list of file extensions.
    /// Defaults to "png", which means only PNG files will be processed.
    /// Use "*" to process all supported files.
    #[argh(option, long = "ext", default = "String::from(\"png\")")]
    pub extensions: String,

    /// filter input images by regex pattern on full path.
    /// Only files matching the pattern will be processed.
    #[argh(option)]
    pub filter: Option<String>,

    /// exclude input images by regex pattern on full path.
    /// Files matching the pattern will be skipped.
    #[argh(option)]
    pub exclude: Option<String>,

    /// path to error log, for which errors will be appended
    #[argh(option)]
    pub error_log: Option<PathBuf>,

    /// interval (in files processed) to print a summary of progress.
    /// Default is no summary.
    #[argh(option)]
    pub summary_interval: Option<usize>,

    /// number of threads each conversion process should use.
    /// Use -1 to use all available threads, 0 (default) for single-threaded.
    #[argh(option, short = 't', default = "0")]
    pub threads: i32,

    /// number of parallel conversion processes to run.
    /// Use -1 (default) to use all available threads. Minimum is 1 if set.
    #[argh(option, short = 'p', default = "-1")]
    pub parallel: i32,

    /// use progressive encoding for JPEG XL files.
    #[argh(switch)]
    pub progressive: bool,

    /// sort files before conversion.
    /// Valid values are "none", "asc", "desc", "rand", "name", "mtime", "ctime", "atime".
    /// "asc" and "desc" sort by file size, "rand" sorts randomly. Default is "none".
    #[argh(option, short = 's', default = "SortMethod::None")]
    pub sort: SortMethod,

    /// paths to files or directories to convert.
    #[argh(positional, greedy)]
    pub paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct FileEntry {
    path: PathBuf,
    metadata: std::fs::Metadata,
}

#[derive(Debug, Default)]
struct Progress {
    processed: AtomicUsize,
    errored: AtomicUsize,
    inefficient: AtomicUsize,
    input_bytes: AtomicU64,
    output_bytes: AtomicU64,
    elapsed: AtomicU64,
}

impl Progress {
    pub fn add(&self, input: u64, output: u64, elapsed: u64) -> usize {
        self.input_bytes.fetch_add(input, Ordering::Relaxed);
        self.output_bytes.fetch_add(output, Ordering::Relaxed);
        self.elapsed.fetch_add(elapsed, Ordering::Relaxed);

        self.processed.fetch_add(1, Ordering::Release) + 1
    }

    pub fn errored(&self) {
        self.errored.fetch_add(1, Ordering::Relaxed);
    }

    pub fn inefficient(&self) {
        self.inefficient.fetch_add(1, Ordering::Relaxed);
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args: Conv2JxlArgs = argh::from_env();

    if args.parallel == -1 {
        args.parallel = std::thread::available_parallelism().map(|n| n.get() as i32).unwrap_or(1);
    } else {
        args.parallel = args.parallel.max(1);
    }

    args.threads = args.threads.clamp(-1, i32::MAX);
    args.quality = args.quality.clamp(0, 100);
    args.effort = args.effort.clamp(0, 10);

    let filter = args.filter.as_ref().map(|s| regex::Regex::new(s)).transpose()?;
    let exclude = args.exclude.as_ref().map(|s| regex::Regex::new(s)).transpose()?;

    let mut extensions = args.extensions.split(',').map(|s| s.trim().as_ref()).collect::<Vec<&OsStr>>();

    extensions.sort_unstable();
    extensions.dedup();

    let all_extensions = extensions.contains(&"*".as_ref());

    // check if extension matches any of the provided extensions
    let matches_ext = |path: &Path| all_extensions || extensions.iter().any(|ext| path.extension().is_some_and(|s| s.eq_ignore_ascii_case(ext)));

    // setup key extraction for ignoring recent files based on the recent_field argument
    let ignore_key = match args.recent_field {
        RecentField::MTime => std::fs::Metadata::modified,
        RecentField::CTime => std::fs::Metadata::created,
        RecentField::ATime => std::fs::Metadata::accessed,
    };

    let error_log = if let Some(ref path) = args.error_log {
        Some(Arc::new(Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(path)?)))
    } else {
        None
    };

    let mut files: Vec<FileEntry> = Vec::new();
    let mut current_files: Vec<FileEntry> = Vec::new();
    let mut pending_dirs = Vec::new();

    for path in &args.paths {
        let metadata = std::fs::metadata(path)?;

        if metadata.is_file() && matches_ext(path) {
            files.push(FileEntry { path: path.clone(), metadata });
        } else if metadata.is_dir() && args.recurse {
            pending_dirs.push((0u64, path.to_path_buf()));
        }
    }

    while let Some((depth, path)) = pending_dirs.pop() {
        if depth > args.max_depth {
            continue;
        }

        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let metadata = entry.metadata()?;
            let path = entry.path();

            if (filter.is_some() || exclude.is_some())
                && let Some(path) = path.to_str()
            {
                if matches!(filter, Some(ref filter) if !filter.is_match(path)) {
                    continue;
                }

                if matches!(exclude, Some(ref exclude) if exclude.is_match(path)) {
                    continue;
                }
            }

            if depth >= args.min_depth && metadata.is_file() && matches_ext(&path) {
                current_files.push(FileEntry { path, metadata });
            } else if args.recurse && metadata.is_dir() {
                pending_dirs.push((depth + 1, path));
            }
        }

        if args.ignore_recent > 0 {
            // ignore all files if they are all "recent"
            if current_files.len() <= args.ignore_recent as usize {
                current_files.clear();
                continue;
            }

            let truncate = current_files.len() - args.ignore_recent as usize;
            current_files.select_nth_unstable_by_key(truncate, |f| ignore_key(&f.metadata).ok());
            current_files.truncate(truncate);
        }

        files.append(&mut current_files);
    }

    if args.min_size > 0 || args.max_size < u64::MAX {
        files.retain(|f| {
            let size = f.metadata.len();
            args.min_size <= size && size <= args.max_size
        });
    }

    match args.sort {
        SortMethod::None => {}
        SortMethod::Asc => files.sort_by_key(|f| f.metadata.len()),
        SortMethod::Desc => files.sort_by_key(|f| std::cmp::Reverse(f.metadata.len())),
        SortMethod::Name => files.sort_by(|a, b| a.path.cmp(&b.path)),
        // most-recent first for these time-based sorts
        SortMethod::MTime => files.sort_by_key(|f| std::cmp::Reverse(f.metadata.modified().ok())),
        SortMethod::CTime => files.sort_by_key(|f| std::cmp::Reverse(f.metadata.created().ok())),
        SortMethod::ATime => files.sort_by_key(|f| std::cmp::Reverse(f.metadata.accessed().ok())),

        SortMethod::Rand => {
            use rand::seq::SliceRandom;
            files.shuffle(&mut rand::rng());
        }
    }

    if let Some(limit) = args.limit {
        files.truncate(limit);
    }

    let total_size = files.iter().map(|f| f.metadata.len()).sum::<u64>();

    println!("Found {} files ({}) to convert.", files.len(), format_bytes(total_size));

    let idx = Arc::new(AtomicUsize::new(0));
    let n = files.len();

    let progress = Progress::default();

    let ext_progress = ArcSwap::from_pointee(HashMap::<String, Arc<Progress>, foldhash::fast::FixedState>::default());

    let real_start = Instant::now();

    let error_style = anstyle::Style::new().fg_color(Some(anstyle::AnsiColor::Red.into()));
    let success_style = anstyle::Style::new().fg_color(Some(anstyle::AnsiColor::Green.into()));
    let warning_style = anstyle::Style::new().fg_color(Some(anstyle::AnsiColor::Yellow.into()));
    let info_style = anstyle::Style::new().fg_color(Some(anstyle::AnsiColor::Cyan.into()));

    let print_progress_report = || {
        let i = progress.processed.load(Ordering::Acquire);
        let errored = progress.errored.load(Ordering::Relaxed);
        let inefficient = progress.inefficient.load(Ordering::Relaxed);
        let input = progress.input_bytes.load(Ordering::Relaxed);
        let output = progress.output_bytes.load(Ordering::Relaxed);
        let elapsed = progress.elapsed.load(Ordering::Relaxed);

        let avg_time = elapsed as f64 / i as f64;

        let in_size = format_bytes(input);
        let out_size = format_bytes(output);

        let in_percent = input as f64 / total_size as f64 * 100.0;
        let compression_ratio = output as f64 / input as f64 * 100.0;

        // lock stdout so the output doesn't get jumbled with other threads
        let mut stdout = std::io::stdout().lock();

        use std::io::Write;

        let _ = writeln!(
            stdout,
            "{info_style}--- Summary after processing {i}/{n} files ({errored} errors, {inefficient} inefficient) ---"
        );
        let _ = writeln!(stdout, "   total input size {in_size} ({in_percent:.02}% of {})", format_bytes(total_size));
        let _ = writeln!(stdout, "   total output size {out_size} (compression ratio {compression_ratio:.02}%)");
        let _ = writeln!(
            stdout,
            "   total elapsed time: {}, avg {} per file",
            format_time(real_start.elapsed().as_millis() as f64),
            format_time(avg_time)
        );

        let map = ext_progress.load();

        for (ext, progress) in map.iter() {
            let i = progress.processed.load(Ordering::Acquire);

            let input = progress.input_bytes.load(Ordering::Relaxed);
            let output = progress.output_bytes.load(Ordering::Relaxed);
            let elapsed = progress.elapsed.load(Ordering::Relaxed);

            if i == 0 {
                continue;
            }

            let avg_time = elapsed as f64 / i as f64;

            let in_size = format_bytes(input);
            let out_size = format_bytes(output);
            let compression_ratio = output as f64 / input as f64 * 100.0;

            let _ = writeln!(
                stdout,
                "   - '.{ext}': {i} files, {in_size} to {out_size} ({compression_ratio:.02}%) elapsed {} avg",
                format_time(avg_time)
            );
        }

        print!("{info_style:#}");
    };

    macro_rules! log_error {
        ($($arg:tt)*) => {
            use std::io::Write;

            let mut stderr = std::io::stderr().lock();

            let _ = write!(stderr, "{error_style}");
            let _ = write!(stderr, $($arg)*);
            let _ = writeln!(stderr, "{error_style:#}");

            progress.errored();

            if let Some(ref log) = error_log {
                if let Ok(mut file) = log.lock() {
                    let _ = writeln!(file, $($arg)*);
                }
            }
        };
    }

    ctrlc::set_handler({
        let idx = idx.clone();

        move || {
            println!("\n{warning_style}Received interrupt signal, stopping new conversions...{warning_style:#}");

            idx.store(n, Ordering::Relaxed);
        }
    })?;

    std::thread::scope(|scope| {
        for _ in 0..args.parallel {
            scope.spawn(|| {
                loop {
                    let i = idx.fetch_add(1, Ordering::Relaxed);

                    if i >= files.len() {
                        return;
                    }

                    let src = &files[i];

                    let start_instant = Instant::now();

                    let output_path = src.path.with_extension("jxl");

                    let dst_leaf = output_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("file.jxl");

                    let src_ext = src.path.extension().and_then(|s| s.to_str()).unwrap_or("unknown");

                    if output_path.exists() && !args.overwrite {
                        println!("{info_style}Skipping {dst_leaf}: already exists.{info_style:#}");

                        continue;
                    }

                    let mut quality = args.quality;

                    if args.lossless_jpeg && (src_ext.eq_ignore_ascii_case("jpg") || src_ext.eq_ignore_ascii_case("jpeg")) {
                        quality = 100; // force lossless for JPEG files
                    }

                    let mut cmd = std::process::Command::new("cjxl");

                    cmd.arg(&src.path).arg(&output_path);
                    cmd.arg("-q").arg(quality.to_string());
                    cmd.arg("-e").arg(args.effort.to_string());
                    cmd.arg("--num_threads").arg(args.threads.to_string());
                    cmd.arg("--lossless_jpeg").arg(if args.lossless_jpeg { "1" } else { "0" });
                    cmd.arg("--quiet");

                    if !args.enable_jpeg_reconstruction {
                        cmd.arg("--allow_expert_options")
                           .arg("--allow_jpeg_reconstruction").arg("0"); // disable embedded JPEG reconstruction
                    }

                    if args.dry_run {
                        println!("{info_style}[{i}/{n}] Dry run: would convert '{}' to '{dst_leaf}' with quality {} and effort {}.{info_style:#}",
                            src.path.display(), quality, args.effort);

                        continue;
                    }

                    match cmd.output() {
                        Ok(output) if output.status.success() => {
                            let Ok(file) = std::fs::OpenOptions::new().write(true).open(&output_path) else {
                                log_error!("Failed to open converted file '{}' for verification.", output_path.display());
                                continue;
                            };

                            let Ok(meta) = file.metadata() else {
                                log_error!("Failed to get metadata for converted file '{}'", output_path.display());

                                if let Err(e) = std::fs::remove_file(&output_path) {
                                    log_error!("Failed to delete corrupted file '{}': {}", output_path.display(), e);
                                }

                                continue;
                            };

                            if meta.len() == 0 {
                                eprintln!("{warning_style}[{i}/{n}] Warning: Conversion of '{}' produced an empty file.{warning_style:#}", src.path.display());

                                if let Err(e) = std::fs::remove_file(&output_path) {
                                    log_error!("Failed to delete empty file '{}': {}", output_path.display(), e);
                                }

                                continue;
                            }

                            let src_size = format_bytes(src.metadata.len());
                            let dst_size = format_bytes(meta.len());

                            if meta.len() > src.metadata.len() {
                                println!("{warning_style}[{i}/{n}] Warning: Converted file '{dst_leaf}' ({dst_size}) is larger than the original '{src_ext}' ({src_size}).{warning_style:#}");

                                progress.inefficient();

                                if let Err(e) = std::fs::remove_file(&output_path) {
                                    log_error!("Failed to delete larger output file '{}': {}", output_path.display(), e);
                                }

                                continue;
                            }

                            #[allow(clippy::collapsible_if)]
                            if let (Ok(ctime), Ok(mtime), Ok(atime)) = (
                                src.metadata.created(), src.metadata.modified(), src.metadata.accessed()
                            ) {
                                if let Err(e) = file.set_times(std::fs::FileTimes::new().set_created(ctime).set_modified(mtime).set_accessed(atime)) {
                                    eprintln!("{warning_style}Failed to set file times for '{}': {}{warning_style:#}", output_path.display(), e);
                                }
                            }

                            let percent = meta.len() as f64 / src.metadata.len() as f64 * 100.0;

                            if args.delete && src.path != output_path {
                                if let Err(e) = std::fs::remove_file(&src.path) {
                                    log_error!("Failed to delete source file {}: {}", src.path.display(), e);
                                } else {
                                    println!("{success_style}[{i}/{n}] Successfully converted '{dst_leaf}' at {percent:.02}% ({src_size} to {dst_size}) and deleted the '{src_ext}' source file.{success_style}");
                                }
                            } else {
                                println!("{success_style}[{i}/{n}] Successfully converted '{dst_leaf}' at {percent:.02}% ({src_size} to {dst_size}).{success_style:#}");
                            }

                            let elapsed = start_instant.elapsed().as_millis() as u64;

                            // update per-extension progress
                            if let Some(progress) = ext_progress.load().get(src_ext) {
                                progress.add(src.metadata.len(), meta.len(), elapsed);
                            } else {
                                ext_progress.rcu(|map| {
                                    let mut map = HashMap::clone(map);
                                    map.entry(src_ext.to_string()).or_default().add(src.metadata.len(), meta.len(), elapsed);
                                    Arc::new(map)
                                });
                            }

                            // update overall progress
                            let processed = progress.add(src.metadata.len(), meta.len(), elapsed);

                            #[allow(clippy::collapsible_if)]
                            if let Some(interval) = args.summary_interval {
                                if processed % interval == 0 || processed == n - 1 {
                                    print_progress_report();
                                }
                            }
                        }
                        Ok(output) => {
                            log_error!("Failed to convert '{}': exit code {}: {}",
                                src.path.display(), output.status, String::from_utf8_lossy(&output.stderr).trim());
                        }
                        Err(e) => {
                            log_error!("Failed to convert '{}': {e}", src.path.display());
                        }
                    }
                }
            });
        }
    });

    print_progress_report();

    Ok(())
}
