use std::{ffi::OsStr, str::FromStr as _};

use crate::cli::{Conv2JxlArgs, SortMethod, SortOrder};

use super::*;

#[derive(Debug, Default)]
pub struct FileScanObserver {
    pub found: AtomicU64,
    pub bytes: AtomicU64,
}

#[derive(Debug, Default)]
pub struct ScanObserver {
    pub dir_read: AtomicU64,
    pub dir_found: AtomicU64,
    pub files: PerFileType<FileScanObserver>,
}

impl Conv2JxlArgs {
    pub fn normalize(&mut self) {
        self.threads = self.threads.clamp(-1, i32::MAX);
        self.quality = self.quality.clamp(0, 100);
        self.effort = self.effort.clamp(0, 10);
        self.randomize = self.randomize.clamp(0.0, 1.0);
        self.min_ratio = self.min_ratio.max(0.0);
        self.min_size = self.min_size.max(1); // always exclude empty files

        // ensure min_size <= max_size
        self.max_size = self.max_size.max(self.min_size);

        // ensure min_depth <= max_depth
        self.max_depth = self.max_depth.max(self.min_depth);

        if self.parallel == -1 {
            self.parallel = std::thread::available_parallelism()
                .map(|n| n.get() as i32)
                .unwrap_or(1);
        } else {
            self.parallel = self.parallel.max(1);
        }
    }

    pub fn scan(&self, observer: &ScanObserver) -> Result<ConversionState, Box<dyn std::error::Error>> {
        let filter = self.filter.as_ref().map(|s| regex::Regex::new(s)).transpose()?;
        let exclude = self.exclude.as_ref().map(|s| regex::Regex::new(s)).transpose()?;

        let mut visited = std::collections::HashSet::<PathBuf, _>::with_capacity_and_hasher(
            1024,
            foldhash::fast::FixedState::default(),
        );

        let mut files: Vec<FileEntry> = Vec::new();
        let mut current_files: Vec<FileEntry> = Vec::new();
        let mut pending_dirs = Vec::new();

        for path in &self.paths {
            let path = path.canonicalize()?;

            let mut metadata = std::fs::metadata(&path)?;

            if metadata.is_symlink() {
                if !self.follow_links {
                    continue;
                }

                metadata = std::fs::symlink_metadata(&path)?;
            }

            if metadata.is_file() && (self.min_size..=self.max_size).contains(&metadata.len()) {
                let Some(ext) = path
                    .extension()
                    .and_then(OsStr::to_str)
                    .and_then(|s| FileType::from_str(s).ok())
                else {
                    continue;
                };

                if !self.extensions.contains(&ext) {
                    continue;
                }

                let f = observer.files.get(ext);

                f.found.fetch_add(1, Ordering::Relaxed);
                f.bytes.fetch_add(metadata.len(), Ordering::Relaxed);

                files.push(FileEntry::new(path.clone(), ext, metadata));
            } else if metadata.is_dir() && visited.insert(path.clone()) {
                pending_dirs.push((0u64, path));

                observer.dir_found.fetch_add(1, Ordering::Relaxed);
            }
        }

        let mut excluded = 0;

        while let Some((depth, path)) = pending_dirs.pop() {
            observer.dir_read.fetch_add(1, Ordering::Relaxed);

            if depth > self.max_depth {
                continue;
            }

            current_files.clear();

            for entry in std::fs::read_dir(&path)? {
                let entry = entry?;
                let mut ft = entry.file_type()?;

                // avoid computing metadata unless necessary
                let mut ext = None;
                let mut metadata = None;

                let path = entry.path();

                // store and filter by extension only for files,
                // before potentially expensive metadata calls
                if ft.is_file() {
                    ext = match path
                        .extension()
                        .and_then(OsStr::to_str)
                        .and_then(|s| FileType::from_str(s).ok())
                    {
                        Some(ext) if self.extensions.contains(&ext) => Some(ext),
                        _ => continue,
                    };
                }

                if (filter.is_some() || exclude.is_some())
                    && let Some(path) = path.to_str()
                    && (matches!(filter, Some(ref filter) if !filter.is_match(path))
                        || matches!(exclude, Some(ref exclude) if exclude.is_match(path)))
                {
                    excluded += 1;
                    continue;
                }

                if ft.is_symlink() {
                    if !self.follow_links {
                        continue;
                    }

                    let new_metadata = std::fs::symlink_metadata(&path)?;
                    ft = new_metadata.file_type();
                    metadata = Some(new_metadata);
                }

                if ft.is_dir() {
                    if self.recurse && visited.insert(path.clone()) {
                        pending_dirs.push((depth + 1, path));
                    }

                    continue;
                }

                if !ft.is_file() || depth < self.min_depth {
                    continue;
                }

                let ext = ext.unwrap(); // must be Some() due to earlier check

                let metadata = match metadata {
                    Some(m) => m,
                    None => entry.metadata()?,
                };

                if !(self.min_size..=self.max_size).contains(&metadata.len()) {
                    continue;
                }

                let f = observer.files.get(ext);

                f.found.fetch_add(1, Ordering::Relaxed);
                f.bytes.fetch_add(metadata.len(), Ordering::Relaxed);

                current_files.push(FileEntry::new(path, ext, metadata));
            }

            files.append(&mut current_files);
        }

        match (self.sort, self.sort_order) {
            (SortMethod::Name, SortOrder::Asc) => files.sort_by(|a, b| a.path.cmp(&b.path)),
            (SortMethod::Name, SortOrder::Desc) => files.sort_by(|a, b| b.path.cmp(&a.path)),

            (SortMethod::Size, SortOrder::Asc) => files.sort_by_key(|f| f.metadata.len()),
            (SortMethod::Size, SortOrder::Desc) => files.sort_by_key(|f| std::cmp::Reverse(f.metadata.len())),

            (SortMethod::ATime, SortOrder::Asc) => files.sort_by_key(|f| f.metadata.accessed().ok()),
            (SortMethod::CTime, SortOrder::Asc) => files.sort_by_key(|f| f.metadata.created().ok()),
            (SortMethod::MTime, SortOrder::Asc) => files.sort_by_key(|f| f.metadata.modified().ok()),

            (SortMethod::ATime, SortOrder::Desc) => {
                files.sort_by_key(|f| std::cmp::Reverse(f.metadata.accessed().ok()))
            }
            (SortMethod::CTime, SortOrder::Desc) => files.sort_by_key(|f| std::cmp::Reverse(f.metadata.created().ok())),
            (SortMethod::MTime, SortOrder::Desc) => {
                files.sort_by_key(|f| std::cmp::Reverse(f.metadata.modified().ok()))
            }

            (SortMethod::None, _) => {}
        }

        if self.randomize > 0.0 {
            use rand::{Rng, SeedableRng, rngs::SmallRng, seq::SliceRandom};

            let mut rng = SmallRng::from_os_rng();

            if self.randomize >= 1.0 {
                files.shuffle(&mut rng);
            } else {
                // partial shuffle based on the randomization factor, using a variant of the Fisher-Yates shuffle
                // and an offset based on the randomization factor to control the degree of shuffling
                // from nearby to fully random
                let width = ((self.randomize * files.len() as f64).ceil() as usize).max(1);

                for i in (1..files.len()).rev() {
                    if rng.random_bool(self.randomize) {
                        let start = i.saturating_sub(width);
                        files.swap(i, rng.random_range(start..=i));
                    }
                }
            }
        }

        if let Some(limit) = self.limit {
            files.truncate(limit);
        }

        let mut progress: PerFileType<Box<ConversionProgress>> = PerFileType::default();

        let mut final_counts = PerFileType::<(u64, u64)>::default(); // (count, bytes)

        for file in &files {
            let progress = progress.get_mut(file.ext);

            *progress.total_bytes.get_mut() += file.metadata.len();
            progress.total += 1;

            let (count, bytes) = final_counts.get_mut(file.ext);

            *count += 1;
            *bytes += file.metadata.len();
        }

        for (ext, &(count, bytes)) in final_counts.iter() {
            let p = observer.files.get(ext);

            p.bytes.store(bytes, Ordering::Relaxed);
            p.found.store(count, Ordering::Relaxed);
        }

        Ok(ConversionState {
            excluded,
            files,
            idx: AtomicUsize::new(0),
            active: Vec::from_iter((0..self.parallel).map(|_| ThreadState {
                file_idx: AtomicUsize::new(usize::MAX),
                start_time: AtomicU64::new(0),
            })),
            non_success: Default::default(),
            progress,
            paused: Default::default(),
        })
    }
}
