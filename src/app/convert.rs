/// Effort to fall back to when cjxl refuses a lossy encode at the requested
/// effort. The highest level known to work where 8 and 9 do not.
const EFFORT_FALLBACK: u8 = 7;

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
        self.idx.load(Ordering::Relaxed) >= self.files.count()
            && self
                .active
                .iter()
                .all(|a| a.file_idx.load(Ordering::Relaxed) == usize::MAX)
    }

    pub fn stop(&self) {
        // Set shutdown first so any worker about to wait sees it. Bumping idx
        // past the end stops new claims in both watch and non-watch modes, and
        // the wake notify releases anyone already blocked.
        self.shutdown.store(true, Ordering::Relaxed);
        self.idx.store(self.files.count(), Ordering::Relaxed);

        // Briefly hold the lock so a waiter in the middle of
        // `cvar.wait(guard)` can't miss the notification, then notify_all.
        drop(self.wake.0.lock());
        self.wake.1.notify_all();
    }

    /// Append a newly-discovered file (from the --watch promoter) and notify
    /// any workers blocked waiting for new work. Keeps `progress.total` /
    /// `total_bytes` in sync so the gauge and Breakdown counts stay correct.
    pub fn add_file(&self, entry: FileEntry) {
        let progress = self.progress.get(entry.ext);
        progress.total_bytes.fetch_add(entry.metadata.len(), Ordering::Relaxed);
        progress.total.fetch_add(1, Ordering::Relaxed);

        self.files.push(entry);

        // Briefly hold the lock so a waiter in the middle of
        // `cvar.wait(guard)` can't miss the notification, then notify_all.
        drop(self.wake.0.lock());
        self.wake.1.notify_all();
    }

    pub fn add_error(&self, idx: usize, last_active: u64) {
        let src = &self.files[idx];
        self.progress.get(src.ext).errored(src.metadata.len());

        self.non_success.write().unwrap().insert((Reverse(last_active), idx));
        self.logs.outcome(src);
    }

    pub fn add_inefficient(&self, idx: usize, last_active: u64) {
        let src = &self.files[idx];
        self.progress.get(src.ext).inefficient(src.metadata.len());

        self.non_success.write().unwrap().insert((Reverse(last_active), idx));
        self.logs.outcome(src);
    }

    pub fn add_skipped(&self, idx: usize, last_active: u64) {
        let src = &self.files[idx];
        self.progress.get(src.ext).skipped(src.metadata.len());

        self.non_success.write().unwrap().insert((Reverse(last_active), idx));
        self.logs.outcome(src);
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

        // Set the active thread idx and keep start_time at 0 (the "not started
        // yet" sentinel) until there is a file to process. Otherwise a worker
        // that sits in either of the waits below would later display a
        // per-file elapsed time that includes the wait, e.g. a worker that had
        // been blocked for an hour waiting for the next watch arrival would
        // show that arrival as having "elapsed = 1 hour" from the moment it
        // began processing.
        let thread = &self.active[thread_idx];
        thread.file_idx.store(i, Ordering::Relaxed);
        thread.start_time.store(0, Ordering::Relaxed);
        thread.quality.store(QUALITY_UNSET, Ordering::Relaxed);

        self.wait_paused();

        if i >= self.files.count() {
            // Non-watch mode: we're past the end, all done.
            if !args.watch {
                *stop = true;
                return;
            }

            // Watch mode: block until either new files appear (the promoter
            // pushed and notified) or shutdown was requested. Workers may have
            // each claimed an index past the current end. Each waits for _its_
            // index to be in range, so notify_all is correct (others re-wait).
            let (lock, cvar) = &self.wake;
            let mut guard = lock.lock().unwrap();
            while !self.shutdown.load(Ordering::Relaxed) && i >= self.files.count() {
                guard = cvar.wait(guard).unwrap();
            }

            // shutdown requested with no file to satisfy our claim, so exit
            if i >= self.files.count() {
                *stop = true;
                return;
            }
        }

        // We have a real file to process now. Stamp the start time so the
        // per-file elapsed display reflects encode time only. +1 keeps the
        // value distinct from the "not started yet" sentinel of 0.
        thread
            .start_time
            .store(1 + program_start.elapsed().as_millis() as u64, Ordering::Relaxed);

        let src = &self.files[i];

        let mut quality = args.quality;
        let mut tries = 0;
        let mut retried = false;

        // `Some((input, output))` while the conversion is still inefficient and no
        // terminal state has been recorded yet. `None` once `next` has recorded a
        // terminal outcome (success, error, or skipped) itself.
        let mut inefficient: Option<(u64, u64)> = None;

        let lossless_jpeg = args.lossless_jpeg && src.ext == FileType::JPEG;

        // only try twice if the first attempt is inefficient, and there is a fallback quality specified
        while tries < 2 {
            tries += 1;

            if lossless_jpeg {
                quality = 100; // force lossless for JPEG files
            }

            inefficient = self.next(i, src, args, program_start, thread, &mut quality, retried);

            // if it's not inefficient (a terminal state was already recorded), or if
            // lossless_jpeg is enabled (which forces quality 100), we're done
            if inefficient.is_none() || lossless_jpeg {
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
            retried = true;
        }

        // the loop ended while the conversion was still inefficient and nothing
        // salvaged it (no fallback quality, fallback not applicable, or the
        // fallback attempt was also inefficient): record the terminal outcome.
        if let Some((input, output)) = inefficient {
            let last_active = src.set_state(program_start, ConversionOutcome::Inefficient(input, output));
            self.add_inefficient(i, last_active);
        }
    }

    /// Run one conversion attempt. `quality` is in/out: noise analysis may lower
    /// it, and the caller needs to see that to decide whether a further retry at
    /// `--quality-if-inefficient` would actually be lower.
    pub fn next(
        &self,
        i: usize,
        src: &FileEntry,
        args: &Conv2JxlArgs,
        program_start: Instant,
        thread: &ThreadState,
        quality: &mut u8,
        retried: bool,
    ) -> Option<(u64, u64)> {
        let conv_start = Instant::now();

        // A JPEG XL source is re-encoded _in place_: "foo.jxl.jxl" is nonsense,
        // and with -X the output path would be the input path, which would have
        // cjxl writing over the file it is reading. Encode to a sibling temp
        // file instead and swap it in only once the result is accepted.
        let in_place = src.ext == FileType::JXL;

        let output_path = match (in_place, args.no_preserve_extension) {
            (true, _) => src.path.with_extension(format!("jxl.tmp{}-{i}", std::process::id())),
            (false, false) => src.path.with_extension(format!("{}.jxl", src.ext)),
            (false, true) => src.path.with_extension("jxl"),
        };

        if !in_place && output_path.exists() && !args.overwrite {
            let last_active =
                src.set_state(program_start, ConversionOutcome::Skipped("output already exists".into()));
            self.add_skipped(i, last_active); // skipped files are considered non-success for UI purposes
            return None;
        }

        if args.min_width > 0 || args.min_height > 0 || args.max_width < u32::MAX || args.max_height < u32::MAX {
            let Ok(dimensions) = imagesize::size(&src.path) else {
                let last_active = src.set_state(
                    program_start,
                    ConversionOutcome::Error("Failed to read image dimensions.".into()),
                );
                self.add_error(i, last_active);
                return None;
            };

            if !args.width().contains(&(dimensions.width as u32))
                || !args.height().contains(&(dimensions.height as u32))
            {
                let last_active = src.set_state(
                    program_start,
                    ConversionOutcome::Skipped("outside the dimension filter".into()),
                );
                self.add_skipped(i, last_active); // skipped files are considered non-success for UI purposes
                return None;
            }
        }

        // An already-lossy JPEG XL is left alone whether or not the noise pass
        // is running: re-encoding it losslessly would multiply its size for
        // nothing, and re-encoding it lossily would stack a second generation
        // of loss onto the first.
        //
        // This asks the file rather than anything this process remembers, so it
        // holds across separate runs and between a watcher and a manual pass
        // over the same directory, including for files some _other_ instance
        // produced. It is the durable half of the self-output protection. The
        // watcher's own bookkeeping only covers what this process wrote.
        //
        // With the noise pass on, the cheaper filters inside it run first and
        // an unreadable header means skipping, since guessing wrong there costs
        // image quality. Without it the only risk is wasted work that
        // --min-ratio would undo anyway, so an unreadable header proceeds.
        if in_place
            && !args.reencode_lossy_jxl
            && args.quality_if_noisy.is_none()
            && super::noise::is_lossy_jxl(&src.path) == Some(true)
        {
            let last_active =
                src.set_state(program_start, ConversionOutcome::Skipped("already a lossy JPEG XL".into()));
            self.add_skipped(i, last_active);
            return None;
        }

        // Decide whether this image is carrying a random-noise overlay and, if
        // so, drop to the quality that makes it worth storing. This runs once
        // per file: a retry already had its quality chosen for it.
        let mut note: Option<Cow<'static, str>> = None;

        if let Some(noisy_quality) = args.quality_if_noisy
            && !retried
            && !(args.lossless_jpeg && src.ext == FileType::JPEG)
        {
            use super::noise::Verdict;

            // Why the noise pass is leaving this file at the normal quality,
            // if it is.
            let declined: Option<Cow<'static, str>> =
                match super::noise::evaluate(&src.path, src.ext, src.metadata.len(), args) {
                    Verdict::Noisy(stats) if noisy_quality < *quality => {
                        *quality = noisy_quality;
                        note = Some(format!("Noise detected ({stats}), encoded at quality {noisy_quality}").into());
                        None
                    }

                    Verdict::Noisy(stats) => {
                        Some(format!("noisy ({stats}), but --quality-if-noisy is not lower than --quality").into())
                    }

                    Verdict::Clean(stats) => {
                        if args.dry_run {
                            note = Some(format!("Analyzed ({stats})").into());
                        }

                        Some(format!("no noise detected ({stats})").into())
                    }

                    Verdict::NotApplicable => Some("not eligible for the noise pass".into()),

                    // A lossless re-encode of an already-lossy file only
                    // inflates it and a lossy one stacks generation loss, so
                    // leave it be. This is also what keeps repeated runs
                    // idempotent: our own output from a previous run lands here.
                    Verdict::AlreadyLossy => Some("already a lossy JPEG XL".into()),

                    Verdict::Failed(why) => {
                        note = Some(why.clone());
                        Some(why)
                    }
                };

            // A JPEG XL source is in the list for the noise pass and nothing
            // else. Re-encoding one the pass declined would mean a full
            // lossless encode of every image in the library to gain, at best,
            // a few percent, so stop here instead.
            if in_place
                && let Some(reason) = declined
            {
                let last_active = src.set_state(program_start, ConversionOutcome::Skipped(reason));
                self.add_skipped(i, last_active);
                return None;
            }
        }

        // Claim the paths we are about to write before writing them, so the
        // watcher can tell our own output from a file someone else dropped in.
        if args.watch {
            self.mark_produced(&output_path);

            if in_place {
                self.mark_produced(&src.path);
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

                    return None;
                }
            };
        }

        // Publish what this attempt settled on, now that the noise pass has
        // run, so the UI can mark the file while it is being encoded.
        thread.quality.store(*quality, Ordering::Relaxed);

        let source = match tmp_file {
            Some(ref tmp) => tmp.path(),
            None => &src.path,
        };

        let encode = |effort: u8| {
            let mut cmd = std::process::Command::new("cjxl");

            cmd.arg(source).arg(&output_path);

            cmd.arg("-q").arg(quality.to_string());
            cmd.arg("-e").arg(effort.to_string());
            cmd.arg("--num_threads").arg(args.threads.to_string());
            cmd.arg("--lossless_jpeg")
                .arg(if args.lossless_jpeg { "1" } else { "0" });
            cmd.arg("--quiet");

            if args.progressive {
                cmd.arg("--progressive");
            }

            if args.disable_jpeg_reconstruction {
                cmd.arg("--allow_expert_options")
                    .arg("--allow_jpeg_reconstruction")
                    .arg("0");
            }

            cmd.output()
        };

        let input = src.metadata.len();

        if args.dry_run {
            // optionally simulate encoding time so the TUI (throbbers, speed,
            // ETA) can be exercised without invoking cjxl. Sleep in small
            // chunks so a quit (which sets idx past the end) stays responsive.
            let mut remaining = args.dry_run_delay;
            while remaining > 0 {
                let chunk = remaining.min(50);
                std::thread::sleep(std::time::Duration::from_millis(chunk));
                remaining -= chunk;

                if self.idx.load(Ordering::Relaxed) >= self.files.count() {
                    break; // stop requested
                }
            }

            // mark as same-size success and account for it so progress/ETA
            // still reach completion
            self.record_converted(
                i,
                src,
                program_start,
                (input, input),
                conv_start.elapsed().as_millis() as u64,
                note,
            );
            return None;
        }

        let mut output = encode(args.effort);

        // cjxl can fail outright at high effort with lossy settings on some
        // images ("JxlEncoderProcessOutput failed", seen on libjxl 0.12 with
        // -q 95 -e 9 on large noisy photographs), where -e 7 at the same
        // quality encodes fine and, as it happens, usually smaller than nudging
        // the quality up instead. Quality is the user's choice and effort is a
        // default, so give up effort, once, and attach the reduced effort to
        // the file's warning.
        let mut reduced_effort = None;

        if matches!(&output, Ok(done) if !done.status.success())
            && *quality < 100
            && args.effort > EFFORT_FALLBACK
        {
            output = encode(EFFORT_FALLBACK);

            if matches!(&output, Ok(done) if done.status.success()) {
                reduced_effort = Some(EFFORT_FALLBACK);
            }
        }

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

                return None;
            }
            Err(e) => {
                let last_active = src.set_state(
                    program_start,
                    ConversionOutcome::Error(format!("Failed to execute conversion command: {e}").into()),
                );

                self.add_error(i, last_active);

                return None;
            }
        };

        let Ok(file) = std::fs::OpenOptions::new().write(true).open(&output_path) else {
            let last_active = src.set_state(
                program_start,
                ConversionOutcome::Error("Failed to open converted file for verification.".into()),
            );

            self.add_error(i, last_active);

            return None;
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

                return None;
            }

            let last_active = src.set_state(
                program_start,
                ConversionOutcome::Error(
                    "Failed to get metadata for converted file. The output file has been deleted.".into(),
                ),
            );

            self.add_error(i, last_active);

            return None;
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

                return None;
            }

            let last_active = src.set_state(
                program_start,
                ConversionOutcome::Error("Conversion produced an empty file. The empty file has been deleted.".into()),
            );

            self.add_error(i, last_active);

            return None;
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

                return None;
            }

            // do not record a terminal state here: the caller (next_file)
            // decides whether to retry at a lower quality and, if not, records
            // the Inefficient outcome with these sizes.
            return Some((input, output));
        }

        let mut warning =
            note.or_else(|| retried.then_some(Cow::Borrowed("Used lower quality due to inefficiency")));

        if let Some(effort) = reduced_effort {
            let why = format!("cjxl failed at effort {}, encoded at effort {effort}", args.effort);

            warning = Some(match warning {
                Some(had) => format!("{had}; {why}").into(),
                None => why.into(),
            });
        }
        // Carry over whatever timestamps the source and the platform can give
        // us, so the re-encode does not look like a brand-new file.
        let times = crate::utils::preserved_times(&src.metadata);

        if let Some(times) = times
            && let Err(e) = file.set_times(times)
        {
            warning = Some(format!("Failed to set file times: {e}").into());
        }

        drop(file); // close before renaming or changing attributes by path

        if in_place {
            // Swap the accepted re-encode over the original. std's rename maps
            // to MoveFileEx with MOVEFILE_REPLACE_EXISTING on Windows, so this
            // is a single atomic replacement rather than a delete-then-write
            // window where the original is gone.
            if let Err(e) = std::fs::rename(&output_path, &src.path) {
                let _ = std::fs::remove_file(&output_path);

                let last_active = src.set_state(
                    program_start,
                    ConversionOutcome::Error(format!("Failed to replace the original file: {e}").into()),
                );

                self.add_error(i, last_active);

                return None;
            }
        }

        // an in-place re-encode has no separate source left to remove
        if !in_place && (args.delete || args.truncate) && src.path != output_path {
            if args.truncate {
                // truncating requires opening the file for writing, and then setting times if available,
                // because otherwise the modified time would be updated to now, which interferes with
                // some users' workflows
                match std::fs::OpenOptions::new().write(true).truncate(true).open(&src.path) {
                    Err(e) => {
                        warning = Some(format!("Failed to open source file for truncation: {e}").into());
                    }
                    Ok(f) => {
                        // the file is now truncated (open with truncate(true))
                        if let Some(times) = times
                            && let Err(e) = f.set_times(times)
                        {
                            warning =
                                Some(format!("Failed to set file times on truncated source file: {e}").into());
                        }

                        drop(f); // close before changing attributes by path

                        if args.hide_truncated
                            && let Err(e) = crate::utils::set_hidden(&src.path)
                        {
                            warning = Some(format!("Failed to hide truncated source file: {e}").into());
                        }
                    }
                }
            } else if args.delete
                && let Err(e) = std::fs::remove_file(&src.path)
            {
                warning = Some(format!("Failed to delete source file: {e}").into());
            }
        }

        self.record_converted(
            i,
            src,
            program_start,
            (input, output),
            conv_start.elapsed().as_millis() as u64,
            warning,
        );

        None
    }

    /// Record a finished conversion: outcome, the warning list if there is a
    /// note attached, and the per-type progress counters.
    fn record_converted(
        &self,
        i: usize,
        src: &FileEntry,
        program_start: Instant,
        (input, output): (u64, u64),
        elapsed: u64,
        note: Option<Cow<'static, str>>,
    ) {
        let is_warning = note.is_some();

        let last_active = src.set_state(
            program_start,
            match note {
                Some(note) => ConversionOutcome::Warning(input, output, note),
                None => ConversionOutcome::Success(input, output),
            },
        );

        if is_warning {
            self.non_success.write().unwrap().insert((Reverse(last_active), i));
        }

        self.progress.get(src.ext).add(input, output, elapsed);
        self.logs.outcome(src);
    }
}
