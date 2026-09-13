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

            // An in-place re-encode never falls back to a lower quality. A
            // lossless JPEG XL made at effort 9 is almost always larger when
            // re-encoded at a lower effort, so with a fallback this would turn
            // a lossless archive lossy nearly file by file.
            if src.ext == FileType::JXL {
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
        // and with -X the output path would be the input path.
        let in_place = src.ext == FileType::JXL;

        let final_path = match (in_place, args.no_preserve_extension) {
            (true, _) => src.path.clone(),
            (false, false) => src.path.with_extension(format!("{}.jxl", src.ext)),
            (false, true) => src.path.with_extension("jxl"),
        };

        // cjxl writes to a sibling temp file, which is renamed over the final
        // path only once the result is accepted. So a run that dies mid-encode
        // leaves a `.tmp` next to the source rather than a truncated output
        // that the next run would skip as already done, and an in-place
        // re-encode never has cjxl reading and writing the same file.
        let output_path = final_path.with_extension(format!("jxl.tmp{}-{i}", std::process::id()));

        if !in_place && final_path.exists() && !args.overwrite {
            let last_active =
                src.set_state(program_start, ConversionOutcome::Skipped("output already exists".into()));
            self.add_skipped(i, last_active); // skipped files are considered non-success for UI purposes
            return None;
        }

        // Two sources can map to one output: `foo.png` and `foo.jpg` under
        // `-X`. Without this the second would encode over the first's result,
        // and under `--delete` both sources would be gone. A retry is the same
        // source and already holds the claim.
        if !retried && !self.claim_output(&final_path) {
            let last_active = src.set_state(
                program_start,
                ConversionOutcome::Skipped("another source produces the same output path".into()),
            );
            self.add_skipped(i, last_active);
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
        if in_place {
            let info = super::noise::inspect_jxl(&src.path);

            // The noise pass judges frame 0 alone, and whether cjxl carries an
            // animation through a JPEG XL re-encode is untested. Replacing the
            // file on a guess is not worth what it would save.
            if info.as_ref().is_some_and(|info| info.animated) {
                let last_active =
                    src.set_state(program_start, ConversionOutcome::Skipped("animated JPEG XL".into()));
                self.add_skipped(i, last_active);
                return None;
            }

            if !args.reencode_lossy_jxl
                && args.quality_if_noisy.is_none()
                && info.is_some_and(|info| info.lossy == Some(true))
            {
                let last_active =
                    src.set_state(program_start, ConversionOutcome::Skipped("already a lossy JPEG XL".into()));
                self.add_skipped(i, last_active);
                return None;
            }
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

        // The final path was claimed above. The temp path is ours too, so the
        // watcher can tell it from a file someone else dropped in.
        self.mark_produced(&output_path);

        // The `image` crate decodes the first page of a TIFF and drops the
        // rest without a word. Converting that and then deleting the source
        // would lose every other page, so leave multi-page files alone.
        if src.ext == FileType::TIFF {
            match super::conv2png::tiff_has_more_pages(&src.path) {
                Ok(false) => {}
                Ok(true) => {
                    let last_active =
                        src.set_state(program_start, ConversionOutcome::Skipped("multi-page TIFF".into()));
                    self.add_skipped(i, last_active);
                    return None;
                }
                Err(e) => {
                    let last_active = src.set_state(
                        program_start,
                        ConversionOutcome::Error(format!("Failed to read TIFF directory: {e}").into()),
                    );
                    self.add_error(i, last_active);
                    return None;
                }
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

        // The existence check at the top has a window: another instance, or a
        // watcher, may have produced this output since. Look again before
        // the rename makes it moot.
        if !in_place && !args.overwrite && final_path.exists() {
            let _ = std::fs::remove_file(&output_path);

            let last_active =
                src.set_state(program_start, ConversionOutcome::Skipped("output already exists".into()));
            self.add_skipped(i, last_active);
            return None;
        }

        // Move the accepted encode to its final name. std's rename maps to
        // MoveFileEx with MOVEFILE_REPLACE_EXISTING on Windows, so for an
        // in-place re-encode this is a single atomic replacement rather than a
        // delete-then-write window where the original is gone.
        if let Err(e) = std::fs::rename(&output_path, &final_path) {
            let _ = std::fs::remove_file(&output_path);

            let last_active = src.set_state(
                program_start,
                ConversionOutcome::Error(format!("Failed to move the converted file into place: {e}").into()),
            );

            self.add_error(i, last_active);

            return None;
        }

        // an in-place re-encode has no separate source left to remove
        if !in_place && (args.delete || args.truncate) && src.path != final_path {
            // Opening for write follows a symlink, so truncating one would
            // zero the target, which may live anywhere. Deleting removes only
            // the link, which is what the flag means for a link.
            let is_link = std::fs::symlink_metadata(&src.path).is_ok_and(|m| m.is_symlink());

            if args.truncate && is_link {
                warning = Some("Source is a symlink and was left untouched instead of truncated".into());
            } else if args.truncate {
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

/// These drive one worker through the real scan-and-convert path against a
/// temp directory, with cjxl doing the encodes. They pass trivially without
/// cjxl on PATH, and say so.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::scan::ScanObserver;
    use image::ImageEncoder as _;
    use std::{fs::File, path::Path};

    fn have_cjxl() -> bool {
        let ok = std::process::Command::new("cjxl").arg("--version").output().is_ok();

        if !ok {
            println!("cjxl not on PATH, skipping");
        }

        ok
    }

    /// 64x64 RGB gradient: smooth, so a lossless encode is well under the
    /// source size and `--min-ratio 1.0` accepts it.
    fn pixels() -> Vec<u8> {
        (0..64 * 64u32)
            .flat_map(|i| [((i % 64) * 4) as u8, ((i / 64) * 4) as u8, 128])
            .collect()
    }

    fn write_png(path: &Path) {
        image::codecs::png::PngEncoder::new(File::create(path).unwrap())
            .write_image(&pixels(), 64, 64, image::ExtendedColorType::Rgb8)
            .unwrap();
    }

    fn write_bmp(path: &Path) {
        let mut file = File::create(path).unwrap();
        image::codecs::bmp::BmpEncoder::new(&mut file)
            .write_image(&pixels(), 64, 64, image::ExtendedColorType::Rgb8)
            .unwrap();
    }

    fn write_tiff(path: &Path, pages: usize) {
        let mut encoder = tiff::encoder::TiffEncoder::new(File::create(path).unwrap()).unwrap();

        for _ in 0..pages {
            encoder
                .write_image::<tiff::encoder::colortype::RGB8>(64, 64, &pixels())
                .unwrap();
        }
    }

    fn prepare(dir: &Path, flags: &[&str]) -> Arc<SharedState> {
        let mut argv: Vec<&str> = vec!["-p", "1"];
        argv.extend_from_slice(flags);
        argv.push(dir.to_str().unwrap());

        let mut args = <Conv2JxlArgs as argh::FromArgs>::from_args(&["conv2jxl"], &argv).unwrap();
        args.normalize();

        let conv = args.scan(&ScanObserver::default());

        Arc::new(SharedState {
            args,
            conv,
            start: Instant::now(),
        })
    }

    /// Scan `dir` with the given flags and run every file on one worker.
    fn run(dir: &Path, flags: &[&str]) -> Arc<SharedState> {
        let shared = prepare(dir, flags);
        shared.run(0);
        shared
    }

    /// Like [`run`] for a directory with one matching file, stopping right
    /// after it so the worker slot still shows that file's last attempt.
    fn run_one(dir: &Path, flags: &[&str]) -> Arc<SharedState> {
        let shared = prepare(dir, flags);
        assert_eq!(shared.conv.files.count(), 1);

        let mut stop = false;
        shared.conv.next_file(0, &shared.args, shared.start, &mut stop);

        shared
    }

    fn outcome<'a>(shared: &'a SharedState, name: &str) -> &'a ConversionOutcome {
        shared
            .conv
            .files
            .iter()
            .find(|(_, f)| f.path.file_name().is_some_and(|n| n == name))
            .unwrap_or_else(|| panic!("{name} was not scanned"))
            .1
            .state
            .get()
            .unwrap_or_else(|| panic!("{name} has no outcome"))
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp"))
            .collect()
    }

    #[test]
    fn same_stem_sources_do_not_collide() {
        if !have_cjxl() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        write_png(&dir.path().join("a.png"));
        write_bmp(&dir.path().join("a.bmp"));

        // A dry run writes nothing, so the second source cannot be stopped by
        // the output already existing. Only the claim can stop it, which is
        // the situation two parallel workers are in.
        let shared = run(dir.path(), &["-X", "--ext", "png,bmp", "--dry-run"]);

        let skipped: Vec<&str> = ["a.png", "a.bmp"]
            .iter()
            .filter_map(|n| match outcome(&shared, n) {
                ConversionOutcome::Skipped(why) => Some(&**why),
                _ => None,
            })
            .collect();

        assert_eq!(skipped, ["another source produces the same output path"]);

        // For real: one converts, the other backs off, and nothing is lost.
        let shared = run(dir.path(), &["-X", "--ext", "png,bmp", "--delete"]);

        let (png, bmp) = (outcome(&shared, "a.png"), outcome(&shared, "a.bmp"));

        let (won, lost) = match (png, bmp) {
            (ConversionOutcome::Success(..), ConversionOutcome::Skipped(..)) => ("a.png", "a.bmp"),
            (ConversionOutcome::Skipped(..), ConversionOutcome::Success(..)) => ("a.bmp", "a.png"),
            other => panic!("expected one success and one skip, got {:?}", other_names(other)),
        };

        assert!(!dir.path().join(won).exists(), "{won} should have been deleted");
        assert!(dir.path().join(lost).exists(), "{lost} must survive");
        assert!(dir.path().join("a.jxl").exists());
        assert!(leftovers(dir.path()).is_empty());
    }

    fn describe(o: &ConversionOutcome) -> String {
        match o {
            ConversionOutcome::Success(i, o) => format!("Success({i} -> {o})"),
            ConversionOutcome::Warning(i, o, w) => format!("Warning({i} -> {o}, {w})"),
            ConversionOutcome::Skipped(why) => format!("Skipped({why})"),
            ConversionOutcome::Error(e) => format!("Error({e})"),
            ConversionOutcome::Inefficient(i, o) => format!("Inefficient({i} -> {o})"),
        }
    }

    fn other_names(o: (&ConversionOutcome, &ConversionOutcome)) -> (String, String) {
        (describe(o.0), describe(o.1))
    }

    #[test]
    fn rejected_output_leaves_nothing_behind() {
        if !have_cjxl() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        write_png(&dir.path().join("a.png"));

        // --min-ratio 0 makes every encode inefficient
        let shared = run(dir.path(), &["--min-ratio", "0"]);

        assert!(matches!(outcome(&shared, "a.png"), ConversionOutcome::Inefficient(..)));
        assert!(!dir.path().join("a.png.jxl").exists());
        assert!(dir.path().join("a.png").exists());
        assert!(leftovers(dir.path()).is_empty());

        // and an accepted one lands at the final name with no temp left
        let shared = run(dir.path(), &[]);

        assert!(matches!(outcome(&shared, "a.png"), ConversionOutcome::Success(..)));
        assert!(dir.path().join("a.png.jxl").exists());
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn in_place_reencode_never_takes_the_lossy_fallback() {
        if !have_cjxl() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        write_png(&dir.path().join("a.png"));
        run(dir.path(), &["-X"]);

        let jxl = dir.path().join("a.jxl");
        let before = std::fs::read(&jxl).unwrap();

        // Every encode is inefficient, and a fallback quality is offered.
        // A PNG source takes it. A JPEG XL source must not.
        let shared = run_one(dir.path(), &["--ext", "jxl", "--min-ratio", "0", "-Q", "50"]);

        assert!(matches!(outcome(&shared, "a.jxl"), ConversionOutcome::Inefficient(..)));
        assert_eq!(shared.conv.active[0].quality.load(Ordering::Relaxed), 100, "retried at -Q");
        assert_eq!(std::fs::read(&jxl).unwrap(), before, "file was touched");
        assert!(leftovers(dir.path()).is_empty());

        let shared = run_one(dir.path(), &["--ext", "png", "--min-ratio", "0", "-Q", "50"]);

        let png = outcome(&shared, "a.png");
        assert!(matches!(png, ConversionOutcome::Inefficient(..)), "{}", describe(png));
        assert_eq!(shared.conv.active[0].quality.load(Ordering::Relaxed), 50, "PNG should have retried");
    }

    #[test]
    fn multi_page_tiff_is_skipped() {
        if !have_cjxl() {
            return;
        }

        let dir = tempfile::tempdir().unwrap();
        write_tiff(&dir.path().join("one.tiff"), 1);
        write_tiff(&dir.path().join("two.tiff"), 2);

        let shared = run(dir.path(), &["--ext", "tiff", "--delete"]);

        assert!(matches!(outcome(&shared, "one.tiff"), ConversionOutcome::Success(..)));
        assert!(matches!(outcome(&shared, "two.tiff"), ConversionOutcome::Skipped(why) if &**why == "multi-page TIFF"));
        assert!(dir.path().join("two.tiff").exists(), "the multi-page source must survive --delete");
        assert!(!dir.path().join("two.tiff.jxl").exists());
    }
}
