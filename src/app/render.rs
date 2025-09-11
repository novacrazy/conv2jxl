use std::{cmp::Reverse, fmt::Write as _, sync::atomic::Ordering};

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

        if *self.shared.conv.paused.0.lock().unwrap() {
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

        let num_files = self.shared.conv.files.len();

        let idx = self.shared.conv.idx.load(Ordering::Relaxed).min(num_files);

        let remaining_files = num_files.saturating_sub(idx);

        let mut offset = self.ui_state.list_offset;

        if offset > remaining_files {
            offset = 0;
        }

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

                (FileTab::Converted, Some(&ConversionOutcome::Skipped)) => Text::raw(format!(
                    "{skipped_symbol} [{i:>0d$}/{num_files}] '{file_name}' (skipped)"
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
                        )
                    })
                    .filter(|&(i, _)| i < num_files)
                    .collect::<Vec<_>>(); // TODO: SmallVec?

                let pending_files = (idx..num_files)
                    .filter(|&i| !active.iter().any(|&(i2, _)| i2 == i))
                    .filter_map(list_files)
                    .skip(offset);

                let width = rect.width.saturating_sub(2) as usize; // account for borders

                let active_conversions = active.iter().map(|&(i, start)| {
                    let file = &self.shared.conv.files[i];
                    let file_name = file.path.file_name().unwrap_or("Invalid file name".as_ref()).display();

                    let elapsed = self.ui_state.time.saturating_sub(start);

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

                    let elapsed = DecimalTime(elapsed as f64).to_string();

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

                    let mut text = Text::from(Line::raw(text).fg(Color::Green));

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
                //find the minimum index that a thread is currently processing
                let min_idx = self
                    .shared
                    .conv
                    .active
                    .iter()
                    .map(|active| active.file_idx.load(Ordering::Relaxed))
                    .filter(|&i| i < num_files)
                    .min()
                    .unwrap_or(0);

                // let items = {
                //     let mut items = BinaryHeap::with_capacity(rect.height as usize);

                //     let mut skipped = 0;

                //     for i in (0..idx).rev() {
                //         if items.len() > rect.height as usize {
                //             if i < min_idx {
                //                 break;
                //             } else {
                //                 items.pop();
                //             }
                //         }

                //         let file = &self.shared.conv.files[i];

                //         if let Some(ConversionOutcome::Success(..) | ConversionOutcome::Warning(..) | ConversionOutcome::Skipped) = file.state.get() {
                //             if skipped < offset {
                //                 skipped += 1;
                //                 continue;
                //             }

                //             let last_active = file.last_active.load(Ordering::Relaxed);

                //             items.push((Reverse(last_active), i))
                //         }
                //     }

                //     items.into_sorted_vec()
                // };

                let items = {
                    let mut items = Vec::with_capacity(rect.height as usize);

                    for i in (0..idx).rev() {
                        let file = &self.shared.conv.files[i];

                        if let Some(
                            ConversionOutcome::Success(..)
                            | ConversionOutcome::Warning(..)
                            | ConversionOutcome::Skipped,
                        ) = file.state.get()
                        {
                            let last_active = file.last_active.load(Ordering::Relaxed);

                            items.push((Reverse(last_active), i));

                            if i < min_idx && items.len() >= (rect.height as usize + offset) {
                                break;
                            }
                        }
                    }

                    items.sort_unstable_by_key(|(k, _)| *k);

                    items
                };

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

                    let count = processed + errored + inefficient;

                    if count == 0 {
                        return None;
                    }

                    let bytes = progress.total_bytes.load(Ordering::Acquire);
                    let input = progress.input_bytes.load(Ordering::Relaxed);
                    let output = progress.output_bytes.load(Ordering::Relaxed);

                    let compression_ratio = if input > 0 { output as f64 / input as f64 * 100.0 } else { 0.0 };

                    Some(ListItem::new(Text::raw(format!(
                        "'{ft}': {count}/{} files ({:.2}% of {}), {} in -> {} out ({:.2}%), {} saved | {} success, {} errors, {} inefficient",
                        progress.total,
                        (input as f64 / bytes as f64) * 100.0,
                        Bytes(bytes),
                        Bytes(input),
                        Bytes(output),
                        compression_ratio,
                        Bytes(input.saturating_sub(output)),
                        processed,
                        errored,
                        inefficient
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
        let total_files = self.shared.conv.files.len();

        let mut processed = 0;
        let mut errored = 0;
        let mut inefficient = 0;
        let mut total_bytes = 0;
        let mut input_bytes = 0;
        let mut output_bytes = 0;
        let mut elapsed = 0;

        let real_elapsed = self.shared.start.elapsed().as_millis() as f64;

        let mut estimated_eta = 0f64;
        let mut estimated_savings = 0;

        // for each file type, aggregate the stats and estimate the overall ETA and savings
        for (_ft, progress) in self.shared.conv.progress.iter() {
            let current_total_bytes = progress.total_bytes.load(Ordering::Acquire);

            if current_total_bytes == 0 {
                continue;
            }

            let current_input_bytes = progress.input_bytes.load(Ordering::Relaxed);
            let current_output_bytes = progress.output_bytes.load(Ordering::Relaxed);
            let current_elapsed = progress.elapsed.load(Ordering::Relaxed);

            processed += progress.processed.load(Ordering::Relaxed);
            errored += progress.errored.load(Ordering::Relaxed);
            inefficient += progress.inefficient.load(Ordering::Relaxed);

            total_bytes += current_total_bytes;
            input_bytes += current_input_bytes;
            output_bytes += current_output_bytes;
            elapsed += current_elapsed;

            let remaining_bytes = current_total_bytes.saturating_sub(current_input_bytes);

            if current_elapsed > 0 {
                let current_speed = current_input_bytes as f64 / current_elapsed as f64;

                // add remaining time for this file type to the overall ETA
                estimated_eta += remaining_bytes as f64 / current_speed;
            }

            let current_compression_ratio = if current_input_bytes > 0 {
                current_output_bytes as f64 / current_input_bytes as f64
            } else {
                0.0
            };

            // estimate savings for remaining bytes based on current compression ratio
            estimated_savings += ((1.0 - current_compression_ratio) * remaining_bytes as f64) as u64
                + (current_input_bytes - current_output_bytes);
        }

        for thread in &self.shared.conv.active {
            let start_time = thread.start_time.load(Ordering::Relaxed);
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

        let stats_text = Text::raw(format!(
            "Processed: {}/{total_files} ({:.02}% of {}) | Errored: {errored} | Inefficient: {inefficient}\n\
            In: {} | Out: {} ({total_compression_ratio:.02}%) | Saved: {} ({:.02}%)\n\
            Elapsed: {} | Speed: {} | ETA: {} | Estimated Savings: {}",
            processed + errored + inefficient,
            *progress * 100.0,
            Bytes(total_bytes),
            // ---
            Bytes(input_bytes),
            Bytes(output_bytes),
            Bytes(input_bytes.saturating_sub(output_bytes)),
            (100.0 - total_compression_ratio),
            // ---
            TimeBreakdown(real_elapsed),
            Speed::new(input_bytes, elapsed as f64 / self.shared.args.parallel as f64),
            DecimalTime(estimated_eta / self.shared.args.parallel as f64),
            Bytes(estimated_savings),
        ))
        .fg(Color::Cyan);

        Paragraph::new(stats_text).block(Block::new().borders(Borders::all()).title_top("Statistics"))
    }
}
