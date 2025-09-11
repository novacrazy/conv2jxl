use std::os::windows::fs::FileTimesExt as _;

use crate::cli::Conv2JxlArgs;

use super::*;

impl SharedState {
    pub fn run(&self, thread_idx: usize) {
        let mut stop = false;

        while !stop {
            self.next(thread_idx, &mut stop);
        }

        // mark this thread as inactive
        self.conv.active[thread_idx]
            .file_idx
            .store(usize::MAX, Ordering::Relaxed);
    }

    pub fn next(&self, thread_idx: usize, stop: &mut bool) {
        self.conv.next_file(thread_idx, &self.args, self.start, stop);
    }

    pub fn stop(&self) {
        self.conv.stop();
    }
}

impl ConversionState {
    pub fn completed(&self) -> bool {
        self.idx.load(Ordering::Relaxed) >= self.files.len()
            && self
                .active
                .iter()
                .all(|a| a.file_idx.load(Ordering::Relaxed) == usize::MAX)
    }

    pub fn stop(&self) {
        self.idx.store(self.files.len(), Ordering::Relaxed);
    }

    pub fn add_error(&self, idx: usize, last_active: u64) {
        let src = &self.files[idx];
        self.progress.get(src.ext).errored(src.metadata.len());

        self.non_success.write().unwrap().insert((Reverse(last_active), idx));
    }

    pub fn add_inefficient(&self, idx: usize, last_active: u64) {
        let src = &self.files[idx];
        self.progress.get(src.ext).inefficient(src.metadata.len());

        self.non_success.write().unwrap().insert((Reverse(last_active), idx));
    }

    pub fn wait_paused(&self) {
        let (lock, cvar) = &*self.paused;
        let mut paused = lock.lock().unwrap();

        while *paused {
            paused = cvar.wait(paused).unwrap();
        }
    }

    pub fn next_file(&self, thread_idx: usize, args: &Conv2JxlArgs, program_start: Instant, stop: &mut bool) {
        let i = self.idx.fetch_add(1, Ordering::Relaxed);

        // set active thread idx
        let thread = &self.active[thread_idx];
        thread.file_idx.store(i, Ordering::Relaxed);
        thread
            .start_time
            .store(program_start.elapsed().as_millis() as u64, Ordering::Relaxed);

        if i >= self.files.len() {
            *stop = true;
            return;
        }

        let src = &self.files[i];

        let mut quality = args.quality;
        let mut inefficient = false;
        let mut tries = 0;

        let lossless_jpeg = args.lossless_jpeg && src.ext == FileType::JPEG;

        // only try twice if the first attempt is inefficient, and there is a fallback quality specified
        while tries < 2 {
            tries += 1;

            if lossless_jpeg {
                quality = 100; // force lossless for JPEG files
            }

            self.next(i, src, args, program_start, quality, &mut inefficient);

            // if it's not inefficient, or if lossless_jpeg is enabled (which forces quality 100),
            // we don't need to try again
            if !inefficient || lossless_jpeg {
                break;
            }

            // if there is a fallback quality for inefficient conversions
            let Some(quality_if_inefficient) = args.quality_if_inefficient else {
                break;
            };

            // don't try again unless the quality is actually lower, and the file is large enough to bother
            if !(quality_if_inefficient < quality && args.min_inefficient_size.unwrap_or(0) < src.metadata.len()) {
                break;
            }

            quality = quality_if_inefficient;
        }
    }

    pub fn next(
        &self,
        i: usize,
        src: &FileEntry,
        args: &Conv2JxlArgs,
        program_start: Instant,
        quality: u8,
        inefficient: &mut bool,
    ) {
        self.wait_paused();

        let conv_start = Instant::now();

        let output_path = match args.no_preserve_extension {
            false => src.path.with_extension(format!("{}.jxl", src.ext)),
            true => src.path.with_extension("jxl"),
        };

        if output_path.exists() && !args.overwrite {
            let last_active = src.set_state(program_start, ConversionOutcome::Skipped);
            self.non_success.write().unwrap().insert((Reverse(last_active), i)); // skipped files are considered non-success for UI purposes
            return;
        }

        if args.min_width > 0 || args.min_height > 0 || args.max_width < u32::MAX || args.max_height < u32::MAX {
            let Ok(dimensions) = imagesize::size(&src.path) else {
                let last_active = src.set_state(
                    program_start,
                    ConversionOutcome::Error("Failed to read image dimensions.".into()),
                );
                self.add_error(i, last_active);
                return;
            };

            if !args.width().contains(&(dimensions.width as u32))
                || !args.height().contains(&(dimensions.height as u32))
            {
                let last_active = src.set_state(program_start, ConversionOutcome::Skipped);
                self.non_success.write().unwrap().insert((Reverse(last_active), i)); // skipped files are considered non-success for UI purposes
                return;
            }
        }

        let mut tmp_file = None;

        if src.ext.needs_conversion() {
            tmp_file = match super::conv2png::conv2png(&src.path, src.ext) {
                Ok(tmp) => Some(tmp),
                Err(e) => {
                    let last_active = src.set_state(
                        program_start,
                        ConversionOutcome::Error(format!("Failed to convert image to PNG: {e}").into()),
                    );

                    self.add_error(i, last_active);

                    return;
                }
            };
        }

        let mut cmd = std::process::Command::new("cjxl");

        cmd.arg(match tmp_file {
            Some(ref tmp) => tmp.path(),
            None => &src.path,
        })
        .arg(&output_path);

        cmd.arg("-q").arg(quality.to_string());
        cmd.arg("-e").arg(args.effort.to_string());
        cmd.arg("--num_threads").arg(args.threads.to_string());
        cmd.arg("--lossless_jpeg")
            .arg(if args.lossless_jpeg { "1" } else { "0" });
        cmd.arg("--quiet");

        if args.disable_jpeg_reconstruction {
            cmd.arg("--allow_expert_options")
                .arg("--allow_jpeg_reconstruction")
                .arg("0");
        }

        let input = src.metadata.len();

        if args.dry_run {
            // in dry-run mode, just print the command and mark as same-size success
            src.set_state(program_start, ConversionOutcome::Success(input, input));
            return;
        }

        let output = cmd.output();

        drop(tmp_file); // ensure temporary file is deleted after conversion

        let _output = match output {
            Ok(output) if output.status.success() => output,
            Ok(output) => {
                let last_active = src.set_state(
                    program_start,
                    ConversionOutcome::Error(
                        format!(
                            "Conversion command failed with {}: {}",
                            output.status,
                            String::from_utf8_lossy(&output.stderr).trim()
                        )
                        .into(),
                    ),
                );

                self.add_error(i, last_active);

                return;
            }
            Err(e) => {
                let last_active = src.set_state(
                    program_start,
                    ConversionOutcome::Error(format!("Failed to execute conversion command: {e}").into()),
                );

                self.add_error(i, last_active);

                return;
            }
        };

        let Ok(file) = std::fs::OpenOptions::new().write(true).open(&output_path) else {
            let last_active = src.set_state(
                program_start,
                ConversionOutcome::Error("Failed to open converted file for verification.".into()),
            );

            self.add_error(i, last_active);

            return;
        };

        let Ok(meta) = file.metadata() else {
            if let Err(e) = std::fs::remove_file(&output_path) {
                let last_active = src.set_state(
                    program_start,
                    ConversionOutcome::Error(
                        format!(
                            "Failed to get metadata for converted file and also failed to delete corrupted file: {e}",
                        )
                        .into(),
                    ),
                );

                self.add_error(i, last_active);

                return;
            }

            let last_active = src.set_state(
                program_start,
                ConversionOutcome::Error(
                    "Failed to get metadata for converted file. The output file has been deleted.".into(),
                ),
            );

            self.add_error(i, last_active);

            return;
        };

        let output = meta.len();

        if output == 0 {
            if let Err(e) = std::fs::remove_file(&output_path) {
                let last_active = src.set_state(
                    program_start,
                    ConversionOutcome::Error(
                        format!("Conversion produced an empty file, and failed to delete it: {e}").into(),
                    ),
                );

                self.add_error(i, last_active);

                return;
            }

            let last_active = src.set_state(
                program_start,
                ConversionOutcome::Error("Conversion produced an empty file. The empty file has been deleted.".into()),
            );

            self.add_error(i, last_active);

            return;
        }

        let ratio = output as f32 / input as f32;

        if ratio > args.min_ratio {
            if let Err(e) = std::fs::remove_file(&output_path) {
                let last_active = src.set_state(
                    program_start,
                    ConversionOutcome::Error(
                        format!(
                            "Converted file is larger than the original '{}', and failed to delete it: {e}.",
                            src.ext
                        )
                        .into(),
                    ),
                );

                self.add_error(i, last_active);

                return;
            }

            if *inefficient {
                let last_active = src.set_state(program_start, ConversionOutcome::Inefficient(input, output));

                self.add_inefficient(i, last_active);
            }

            *inefficient = true;

            return;
        }

        let mut warning = inefficient.then_some(Cow::Borrowed("Used lower quality due to inefficiency"));
        let mut times = None;

        if let (Ok(ctime), Ok(mtime), Ok(atime)) =
            (src.metadata.created(), src.metadata.modified(), src.metadata.accessed())
        {
            times = Some(
                std::fs::FileTimes::new()
                    .set_created(ctime)
                    .set_modified(mtime)
                    .set_accessed(atime),
            );

            if let Err(e) = file.set_times(times.unwrap()) {
                warning = Some(format!("Failed to set file times: {e}").into());
            }
        }

        if (args.delete || args.truncate) && src.path != output_path {
            if args.truncate {
                // truncating requires opening the file for writing, and then setting times if available
                // because otherwise the modified time would be updated to now, and that interferes with
                // some users' workflows
                match std::fs::OpenOptions::new().write(true).truncate(true).open(&src.path) {
                    Err(e) => {
                        warning = Some(format!("Failed to open source file for truncation: {e}").into());
                    }
                    Ok(f) if times.is_some() => {
                        if let Err(e) = f.set_times(times.unwrap()) {
                            warning = Some(format!("Failed to set file times on truncated source file: {e}").into());
                        }
                    }
                    Ok(_) => { /* success truncating file */ }
                }
            } else if args.delete
                && let Err(e) = std::fs::remove_file(&src.path)
            {
                warning = Some(format!("Failed to delete source file: {e}").into());
            }
        }

        let is_warning = warning.is_some();

        let _last_active = src.set_state(
            program_start,
            match warning {
                Some(w) => ConversionOutcome::Warning(input, output, w),
                None => ConversionOutcome::Success(input, output),
            },
        );

        if is_warning {
            self.non_success.write().unwrap().insert((Reverse(_last_active), i));
        }

        self.progress
            .get(src.ext)
            .add(input, output, conv_start.elapsed().as_millis() as u64);
    }
}
