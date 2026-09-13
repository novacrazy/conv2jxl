use std::{borrow::Cow, cmp::Reverse, collections::BinaryHeap, fmt::Write as _, sync::atomic::Ordering};

use crate::{
    app::{ConversionOutcome, FileTab},
    formatting::{Bytes, DecimalTime, Speed, TimeBreakdown},
};

use ratatui::{prelude::*, widgets::*};

impl super::App {
    pub fn draw(&mut self, frame: &mut Frame) {
        self.ui_state.time = self.shared.start.elapsed().as_millis() as u64;

        self.render(frame.area(), frame.buffer_mut());
    }
}

impl Widget for &super::App {
    fn render(self, area: Rect, buf: &mut Buffer)
    where
        Self: Sized,
    {
        let layout = Layout::vertical([Constraint::Length(1), Constraint::Length(5), Constraint::Min(0)])
            .flex(layout::Flex::Legacy)
            .split(area);

        let mut progress = 0.0;

        self.stats(&mut progress).render(layout[1], buf);

        let mut guage = Gauge::default()
            .ratio(progress)
            .use_unicode(!self.shared.args.no_unicode);

        if self.ui_state.paused {
            guage = guage.label(Span::raw("Paused").fg(Color::Yellow));
        }

        guage.render(layout[0], buf);

        self.render_file_list(layout[2], buf);
    }
}

const THROBBER: &[&str] = &["-", "\\", "|", "/"];

pub struct SymbolSet {
    pub next_symbol: &'static str,
    pub success_symbol: &'static str,
    pub skipped_symbol: &'static str,
    pub warning_symbol: &'static str,
    pub error_symbol: &'static str,
    pub inefficient_symbol: &'static str,
}

const UNICODE_SYMBOLS: SymbolSet = SymbolSet {
    next_symbol: "»",
    success_symbol: "✓",
    skipped_symbol: "→",
    warning_symbol: "⚠",
    error_symbol: "✗",
    inefficient_symbol: "⚠",
};

const ASCII_SYMBOLS: SymbolSet = SymbolSet {
    next_symbol: ">",
    success_symbol: "v",
    skipped_symbol: "->",
    warning_symbol: "!!",
    error_symbol: "x",
    inefficient_symbol: "!",
};

impl super::App {
    fn render_file_list(&self, rect: Rect, buf: &mut Buffer) {
        let tab = self.ui_state.file_tab;

        let tabs = Tabs::new(FileTab::ALL.iter().map(|&t| match t {
            FileTab::Files => Line::raw("Files"),
            FileTab::Converted => Line::raw("Converted"),
            FileTab::Errors => Line::raw("Errors"),
            FileTab::Warnings => Line::raw("Warnings"),
            FileTab::Inefficient => Line::raw("Inefficient"),
            FileTab::Breakdown => Line::raw("Breakdown"),
        }))
        .highlight_style(
            Style::new()
                .bg(tab.accent_color())
                .fg(tab.text_color())
                .add_modifier(Modifier::BOLD),
        )
        .select(self.ui_state.file_tab.idx());

        let num_files = self.shared.conv.files.count();

        let idx = self.shared.conv.idx.load(Ordering::Relaxed).min(num_files);

        // The Files tab shrinks as the queue drains, so an offset that was
        // valid when the key was pressed may point past the end now. Pin it
        // to the last row rather than snapping back to the top.
        let offset = self.ui_state.list_offset.min(self.tab_len().saturating_sub(1));

        let SymbolSet {
            next_symbol,
            success_symbol,
            skipped_symbol,
            warning_symbol,
            error_symbol,
            inefficient_symbol,
        } = if self.shared.args.no_unicode {
            ASCII_SYMBOLS
        } else {
            UNICODE_SYMBOLS
        };

        // number of digits in num_files, for padding
        let d = num_files.max(1).ilog10() as usize + 1;

        let list_files = |i: usize| {
            let file = &self.shared.conv.files[i];

            let mut file_name = file
                .path
                .file_name()
                .unwrap_or("Invalid file name".as_ref())
                .display()
                .to_string();

            if self.shared.args.no_unicode {
                file_name = crate::formatting::strip_non_ascii(file_name, None);
            }

            let i = i + 1; // for formatting

            let mut text = match (tab, file.state.get()) {
                (FileTab::Files, None) => Text::raw(format!(
                    "{next_symbol} [{i:>0d$}/{num_files}] '{}' ({})",
                    file_name,
                    Bytes(file.metadata.len())
                )),

                (FileTab::Converted, Some(&ConversionOutcome::Success(input, output))) => {
                    let compression_ratio = output as f64 / input as f64 * 100.0;
                    Text::raw(format!(
                        "{success_symbol} [{i:>0d$}/{num_files}] {compression_ratio:.2}% '{file_name}' ({} -> {})",
                        Bytes(input),
                        Bytes(output)
                    ))
                }

                (
                    FileTab::Warnings | FileTab::Converted,
                    Some(&ConversionOutcome::Warning(input, output, ref warning)),
                ) => {
                    let compression_ratio = output as f64 / input as f64 * 100.0;
                    Text::raw(format!(
                        "{warning_symbol} [{i:>0d$}/{num_files}] {compression_ratio:.2}% '{file_name}' ({} -> {}) | {warning}",
                        Bytes(input),
                        Bytes(output),
                    ))
                    .fg(if tab == FileTab::Converted { Color::Yellow } else { Color::Gray })
                }

                (FileTab::Converted, Some(ConversionOutcome::Skipped(reason))) => Text::raw(format!(
                    "{skipped_symbol} [{i:>0d$}/{num_files}] '{file_name}' (skipped: {reason})"
                )),

                (FileTab::Errors, Some(ConversionOutcome::Error(error))) => {
                    Text::raw(format!("{error_symbol} [{i:>0d$}/{num_files}] '{file_name}' | {error}"))
                }

                (FileTab::Inefficient, Some(&ConversionOutcome::Inefficient(input, output))) => {
                    let compression_ratio = output as f64 / input as f64 * 100.0;
                    Text::raw(format!(
                        "{inefficient_symbol} [{i:>0d$}/{num_files}] {compression_ratio:.2}% '{file_name}' (reverted) ({} -> {})",
                        Bytes(input),
                        Bytes(output)
                    ))
                }

                // filtered out by tab
                _ => return None,
            };

            if self.ui_state.details
                && let Some(parent) = file.path.parent()
            {
                let parent_path = parent.display();

                let parent_path = if self.shared.args.no_unicode {
                    crate::formatting::strip_non_ascii(parent_path.to_string(), None)
                } else {
                    parent_path.to_string()
                };

                text.push_line(format!("  - '{}'", parent_path.trim_start_matches(r#"\\?\"#)));
            }

            Some(ListItem::new(text))
        };

        let list = match tab {
            FileTab::Files => {
                // get the active states of all workers
                let active = self
                    .shared
                    .conv
                    .active
                    .iter()
                    .map(|active| {
                        (
                            active.file_idx.load(Ordering::Relaxed),
                            active.start_time.load(Ordering::Relaxed),
                            active.quality.load(Ordering::Relaxed),
                        )
                    })
                    .filter(|&(i, ..)| i < num_files)
                    .collect::<smallvec::SmallVec<[_; 32]>>();

                let pending_files = (idx..num_files)
                    .filter(|&i| !active.iter().any(|&(i2, ..)| i2 == i))
                    .filter_map(list_files)
                    .skip(offset);

                let width = rect.width.saturating_sub(2) as usize; // account for borders

                let active_conversions = active.iter().map(|&(i, start, quality)| {
                    let file = &self.shared.conv.files[i];
                    let file_name = file.path.file_name().unwrap_or("Invalid file name".as_ref()).display();

                    let elapsed = self.ui_state.time.saturating_sub(start) + 1;

                    // use length as a simple way to get some variation between files
                    // so they don't all spin in perfect unison
                    let throbber_idx = (((self.ui_state.time + file.metadata.len()) / 400) as usize) % THROBBER.len();

                    let mut text = format!(
                        "{} [{i:>0d$}/{num_files}] '{file_name}' ({})",
                        THROBBER[throbber_idx],
                        Bytes(file.metadata.len()),
                    );

                    if self.shared.args.no_unicode {
                        text = crate::formatting::strip_non_ascii(text, None);
                    }

                    let progress = self.shared.conv.progress.get(file.ext);
                    let speed = Speed::new(
                        progress.input_bytes.load(Ordering::Relaxed),
                        progress.elapsed.load(Ordering::Relaxed) as f64,
                    );

                    let (color, elapsed) = match start {
                        0 if self.ui_state.paused => (Color::Yellow, Cow::Borrowed("N/A")),
                        _ => (Color::Green, Cow::Owned(DecimalTime(elapsed as f64).to_string())),
                    };

                    const MIN_SPACE_FOR_ELAPSED: usize = " | 999.99ms ".len();
                    let text_width = text.chars().count();

                    if let Some(pipe_padding) = width.checked_sub(text_width + MIN_SPACE_FOR_ELAPSED * 2) {
                        for _ in 0..pipe_padding {
                            text.push_str(" ");
                        }

                        text.push_str(" | ");
                        text.push_str(&elapsed);

                        let used_width = text_width + pipe_padding;

                        if !speed.is_zero()
                            && let Some(eta_padding) =
                                (width - used_width).checked_sub(elapsed.chars().count() + MIN_SPACE_FOR_ELAPSED + 3)
                        {
                            for _ in 0..eta_padding {
                                text.push_str(" ");
                            }

                            text.push_str(" / ");

                            let _ = write!(
                                &mut text,
                                "{}",
                                speed.estimate_time(file.metadata.len()).map(DecimalTime).unwrap()
                            );
                        }
                    }

                    // Red marks a file the tool decided to take lossy on its
                    // own: a noisy image dropping to --quality-if-noisy, or an
                    // inefficient one retrying at --quality-if-inefficient. A
                    // run that is lossy throughout because of --quality is the
                    // user's own choice and stays green.
                    let color = match quality {
                        q if q != super::QUALITY_UNSET && q < self.shared.args.quality => Color::Red,
                        _ => color,
                    };

                    let mut text = Text::from(Line::raw(text).fg(color));

                    if self.ui_state.details
                        && let Some(parent) = file.path.parent()
                    {
                        let parent_path = parent.display();

                        let parent_path = if self.shared.args.no_unicode {
                            crate::formatting::strip_non_ascii(parent_path.to_string(), None)
                        } else {
                            parent_path.to_string()
                        };

                        text.push_line(format!("  - '{}'", parent_path.trim_start_matches(r#"\\?\"#)));
                    }

                    ListItem::new(text)
                });

                List::new(active_conversions.chain(pending_files).take(rect.height as usize))
            }

            FileTab::Converted => List::new({
                // A file's index does not track its completion time under
                // parallel, out-of-order completion (a slow low-index file can
                // finish long after higher indices), so we cannot stop the scan
                // early by index. Instead keep the newest `cap` completed files
                // by `last_active` in a bounded min-heap: the oldest falls out
                // whenever we exceed capacity.
                let cap = rect.height as usize + offset;

                let mut heap: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::with_capacity(cap + 1);

                for i in 0..idx {
                    let file = &self.shared.conv.files[i];

                    if let Some(
                        ConversionOutcome::Success(..)
                        | ConversionOutcome::Warning(..)
                        | ConversionOutcome::Skipped(..),
                    ) = file.state.get()
                    {
                        let last_active = file.last_active.load(Ordering::Relaxed);

                        heap.push(Reverse((last_active, i)));

                        if heap.len() > cap {
                            heap.pop(); // drop the oldest of the tracked set
                        }
                    }
                }

                // the heap holds the newest `cap`, emitted most-recent first
                let mut items: Vec<(u64, usize)> = heap.into_iter().map(|Reverse(x)| x).collect();
                items.sort_unstable_by(|a, b| b.cmp(a));

                items
                    .into_iter()
                    .filter_map(|(_, i)| list_files(i))
                    .skip(offset)
                    .take(rect.height as usize)
            }),

            FileTab::Errors | FileTab::Warnings | FileTab::Inefficient => {
                let non_success = self.shared.conv.non_success.read().unwrap();

                List::new(
                    non_success
                        .iter()
                        .rev()
                        .copied()
                        .filter_map(|(_, i)| list_files(i))
                        .skip(offset)
                        .take(rect.height as usize),
                )
            }

            FileTab::Breakdown => {
                // This tab shows a breakdown of files by type, with counts and total sizes.

                List::new(self.shared.conv.progress.iter().filter_map(|(ft, progress)| {
                    let processed = progress.processed.load(Ordering::Relaxed);
                    let errored = progress.errored.load(Ordering::Relaxed);
                    let inefficient = progress.inefficient.load(Ordering::Relaxed);
                    let skipped = progress.skipped.load(Ordering::Relaxed);

                    let count = processed + errored + inefficient + skipped;

                    if count == 0 {
                        return None;
                    }

                    let bytes = progress.total_bytes.load(Ordering::Acquire);
                    let input = progress.input_bytes.load(Ordering::Relaxed);
                    let output = progress.output_bytes.load(Ordering::Relaxed);

                    let compression_ratio = if input > 0 { output as f64 / input as f64 * 100.0 } else { 0.0 };

                    Some(ListItem::new(Text::raw(format!(
                        "'{ft}': {count}/{} files ({:.2}% of {}), {} in -> {} out ({:.2}%), {} saved | {} success, {} errors, {} inefficient, {} skipped",
                        progress.total.load(Ordering::Relaxed),
                        (input as f64 / bytes as f64) * 100.0,
                        Bytes(bytes),
                        Bytes(input),
                        Bytes(output),
                        compression_ratio,
                        Bytes(input.saturating_sub(output)),
                        processed,
                        errored,
                        inefficient,
                        skipped
                    ))))
                }))
            }
        };

        let list = list.block(
            Block::new()
                .border_style(Style::new().fg(tab.accent_color()).bg(tab.accent_color()))
                .border_set(symbols::border::FULL)
                .title_bottom(
                    Line::raw("D - Details, PgUp/PgDn/Up/Down - Scroll, Q - Quit, Tab - Switch Tab")
                        .right_aligned()
                        .fg(tab.text_color())
                        .bg(tab.accent_color()),
                )
                .borders(Borders::all()),
        );

        let layout = Layout::vertical([Constraint::Length(1), Constraint::Min(0)])
            .flex(layout::Flex::Legacy)
            .split(rect);

        Widget::render(tabs, layout[0], buf);
        Widget::render(list, layout[1], buf);
    }

    fn stats(&self, progress: &mut f64) -> impl Widget {
        let total_files = self.shared.conv.files.count();

        let mut processed = 0;
        let mut errored = 0;
        let mut inefficient = 0;
        let mut skipped = 0;
        let mut total_bytes = 0;
        let mut input_bytes = 0;
        let mut output_bytes = 0;

        let real_elapsed = self.shared.start.elapsed().as_millis() as f64;

        let mut estimated_eta = 0f64;
        let mut estimated_savings = 0;
        // Sum of per-type EWMA speeds (bytes per worker-ms). Multiplied by
        // `parallel` below to express the current observed wall throughput.
        let mut ewma_sum = 0f64;

        // for each file type, aggregate the stats and estimate the overall ETA and savings
        for (_ft, progress) in self.shared.conv.progress.iter() {
            // counts must be accumulated regardless of remaining bytes: a type
            // whose files were all skipped/errored/inefficient has had its
            // total_bytes decremented to zero but still contributed work
            processed += progress.processed.load(Ordering::Relaxed);
            errored += progress.errored.load(Ordering::Relaxed);
            inefficient += progress.inefficient.load(Ordering::Relaxed);
            skipped += progress.skipped.load(Ordering::Relaxed);

            let current_total_bytes = progress.total_bytes.load(Ordering::Acquire);

            if current_total_bytes == 0 {
                continue;
            }

            let current_input_bytes = progress.input_bytes.load(Ordering::Relaxed);
            let current_output_bytes = progress.output_bytes.load(Ordering::Relaxed);

            total_bytes += current_total_bytes;
            input_bytes += current_input_bytes;
            output_bytes += current_output_bytes;

            let remaining_bytes = current_total_bytes.saturating_sub(current_input_bytes);

            // Use the per-type EWMA speed (bytes per worker-ms) for ETA so a
            // changing throughput is reflected promptly. Until the first
            // sample for this type, contribute nothing. A guess here would
            // swing the ETA wildly.
            if let Some(speed_per_worker_ms) = *progress.speed_ewma.lock().unwrap()
                && speed_per_worker_ms > 0.0
            {
                estimated_eta += remaining_bytes as f64 / speed_per_worker_ms;
                ewma_sum += speed_per_worker_ms;
            }

            let current_compression_ratio = if current_input_bytes > 0 {
                current_output_bytes as f64 / current_input_bytes as f64
            } else {
                0.0
            };

            // estimate savings for remaining bytes based on current compression ratio
            // saturating: with --min-ratio > 1.0 a kept output can be larger
            // than its input, and an unsigned underflow here would panic in
            // debug and print an absurd "savings" figure in release
            estimated_savings += ((1.0 - current_compression_ratio) * remaining_bytes as f64).max(0.0) as u64
                + current_input_bytes.saturating_sub(current_output_bytes);
        }

        // Adjust ETA for work already in progress: each currently-active
        // worker has spent some time on its claimed file that won't be
        // reflected in `remaining_bytes` until the file completes. Skip slots
        // for threads that have finished (file_idx == usize::MAX, so
        // file_idx >= total_files) and threads that are paused or have just
        // claimed but not started yet (start_time == 0). Otherwise stale or
        // sentinel values would collapse ETA to zero.
        for thread in &self.shared.conv.active {
            let file_idx = thread.file_idx.load(Ordering::Relaxed);
            if file_idx >= total_files {
                continue;
            }
            let start_time = thread.start_time.load(Ordering::Relaxed);
            if start_time == 0 {
                continue;
            }
            estimated_eta -= self.ui_state.time.saturating_sub(start_time) as f64;
        }

        if total_bytes == 0 {
            *progress = 1.0;
            estimated_eta = 0.0;
        } else {
            *progress = input_bytes as f64 / total_bytes as f64;
        }

        let total_compression_ratio = if input_bytes > 0 {
            output_bytes as f64 / input_bytes as f64 * 100.0
        } else {
            0.0
        };

        // Current observed wall throughput: each type's EWMA is in bytes per
        // worker-ms, and multiplied by `parallel` gives wall bytes/ms. We pack
        // that into `Speed` by passing the equivalent bytes-per-second and
        // 1000ms so Speed's existing Display impl yields "{wall bps}/s".
        let wall_speed = if ewma_sum > 0.0 {
            Speed::new((ewma_sum * self.shared.args.parallel as f64 * 1000.0) as u64, 1000.0)
        } else {
            Speed::new(0, 0.0) // displays as N/A
        };

        // `estimated_eta` is accumulated worker-ms, so divide by parallel for wall time.
        let eta_wall = (estimated_eta / self.shared.args.parallel as f64).max(0.0);

        let stats_text = Text::raw(format!(
            "Processed: {}/{total_files} ({:.02}% of {}) | Errored: {errored} | Inefficient: {inefficient} | Skipped: {skipped}\n\
            In: {} | Out: {} ({total_compression_ratio:.02}%) | Saved: {} ({:.02}%)\n\
            Elapsed: {} | Speed: {} | ETA: {} | Estimated Savings: {}",
            processed + errored + inefficient + skipped,
            *progress * 100.0,
            Bytes(total_bytes),
            // ---
            Bytes(input_bytes),
            Bytes(output_bytes),
            Bytes(input_bytes.saturating_sub(output_bytes)),
            (100.0 - total_compression_ratio),
            // ---
            TimeBreakdown(real_elapsed),
            wall_speed,
            DecimalTime(eta_wall),
            Bytes(estimated_savings),
        ))
        .fg(Color::Cyan);

        Paragraph::new(stats_text).block(Block::new().borders(Borders::all()).title_top("Statistics"))
    }
}

/// Render the scan-progress screen. Mirrors the converting layout: a throbber
/// status line, a "Scan" stats block, and a per-type breakdown list.
pub fn draw_scan(
    frame: &mut Frame,
    observer: &crate::app::scan::ScanObserver,
    args: &crate::cli::Conv2JxlArgs,
    elapsed_ms: u64,
) {
    let area = frame.area();
    let buf = frame.buffer_mut();

    let layout = Layout::vertical([Constraint::Length(1), Constraint::Length(5), Constraint::Min(0)])
        .flex(layout::Flex::Legacy)
        .split(area);

    let dir_read = observer.dir_read.load(Ordering::Relaxed);
    let dir_found = observer.dir_found.load(Ordering::Relaxed);
    let excluded = observer.excluded.load(Ordering::Relaxed);
    let errors = observer.errors.load(Ordering::Relaxed);

    let mut total_files = 0u64;
    let mut total_bytes = 0u64;
    for (_ft, f) in observer.files.iter() {
        total_files += f.found.load(Ordering::Relaxed);
        total_bytes += f.bytes.load(Ordering::Relaxed);
    }

    let secs = (elapsed_ms as f64 / 1000.0).max(0.001);
    let dirs_per_s = dir_read as f64 / secs;
    let files_per_s = total_files as f64 / secs;

    // status line
    let throbber = THROBBER[((elapsed_ms / 250) as usize) % THROBBER.len()];
    Line::raw(format!(
        "{throbber} Scanning...   {dirs_per_s:.0} dirs/s . {files_per_s:.0} files/s . {}",
        TimeBreakdown(elapsed_ms as f64)
    ))
    .fg(Color::Cyan)
    .render(layout[0], buf);

    // current directory, trimmed of the Windows verbatim prefix
    let current = observer
        .current
        .lock()
        .ok()
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let mut current = current.trim_start_matches(r#"\\?\"#).to_string();
    if args.no_unicode {
        current = crate::formatting::strip_non_ascii(current, None);
    }

    let stats_text = Text::raw(format!(
        "Directories: {dir_read} read / {dir_found} discovered\n\
         Matched: {total_files} files ({})    Excluded: {excluded}    Errors: {errors}\n\
         Current: {current}",
        Bytes(total_bytes),
    ))
    .fg(Color::Cyan);

    Paragraph::new(stats_text)
        .block(Block::new().borders(Borders::all()).title_top("Scan"))
        .render(layout[1], buf);

    // per-type breakdown
    let rows = observer.files.iter().filter_map(|(ft, f)| {
        let found = f.found.load(Ordering::Relaxed);

        if found == 0 {
            return None;
        }

        Some(ListItem::new(Text::raw(format!(
            "{ft}: {found} files ({})",
            Bytes(f.bytes.load(Ordering::Relaxed))
        ))))
    });

    let list = List::new(rows).block(
        Block::new()
            .borders(Borders::all())
            .border_style(Style::new().fg(Color::Blue))
            .title_bottom(Line::raw("Q - Cancel").right_aligned().fg(Color::Blue)),
    );

    Widget::render(list, layout[2], buf);
}
