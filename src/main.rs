#![allow(clippy::single_char_add_str)]

pub mod app;
pub mod cli;
pub mod formatting;
pub mod pool;
pub mod utils;

use std::{io::Result, sync::atomic::Ordering, time::Duration};

use crossterm::event;

use crate::app::scan::ScanObserver;

const ACTIVE_FRAME_TIME: Duration = Duration::from_millis(1000 / 10);
const IDLE_FRAME_TIME: Duration = Duration::from_millis(1000 / 2);

fn main() -> Result<()> {
    let mut args: cli::Conv2JxlArgs = argh::from_env();

    args.normalize();

    // Validate regexes before entering the TUI: an invalid pattern is a
    // user-config error, not the kind of I/O error the scan tolerates, so fail
    // fast with a plain message instead of an error screen.
    for (name, pat) in [("--filter", &args.filter), ("--exclude", &args.exclude)] {
        if let Some(pat) = pat
            && let Err(e) = regex::Regex::new(pat)
        {
            eprintln!("invalid {name} regex: {e}");
            std::process::exit(2);
        }
    }

    // Printed before the TUI takes the alternate screen, so they are there
    // again when it exits.
    for warning in args.conflicts() {
        eprintln!("warning: {warning}");
    }

    // A missing encoder would otherwise show up as the same error on every
    // row. Decoding is in-process, so cjxl is the only external tool.
    if !args.dry_run {
        require_tool("cjxl", "to encode");
    }

    let logs = match app::report::Logs::open(&args) {
        Ok(logs) => logs,
        Err(e) => {
            eprintln!("could not open log file {e}");
            std::process::exit(2);
        }
    };

    // In --watch mode, create the filesystem watcher BEFORE the scan so that
    // events fired during phase 1 are buffered in its channel instead of being
    // lost. The promoter thread that consumes them is started after phase 2
    // sets up SharedState. Until then, events just queue up. We do this before
    // entering the TUI so a failed backend can report a plain stderr error
    // rather than a screen.
    let pending_watcher = if args.watch {
        match app::watch::prepare(&args) {
            Ok(p) => Some(p),
            Err(e) => {
                eprintln!("failed to start watcher: {e}");
                std::process::exit(2);
            }
        }
    } else {
        None
    };

    // Restore the terminal before the panic message prints, so it lands on a
    // readable screen instead of the TUI, then exit. A panicked worker never
    // marks itself finished, and the main loop would otherwise wait on it
    // forever with one row stuck in progress.
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        ratatui::restore();
        default_panic(info);
        std::process::exit(101);
    }));

    let mut terminal = ratatui::init();

    terminal.clear()?;

    // ---- Phase 1: scan on a background thread, render progress ----

    let observer = ScanObserver::default();
    let (tx, rx) = std::sync::mpsc::channel::<app::ConversionState>();

    let scan_start = std::time::Instant::now();

    let state = std::thread::scope(|s| -> Result<Option<app::ConversionState>> {
        let observer = &observer;
        let args = &args;

        s.spawn(move || {
            let _ = tx.send(args.scan(observer));
        });

        let mut frame_time = ACTIVE_FRAME_TIME;
        let mut frame_counter = 0u64;

        loop {
            if frame_counter % (60 * 10) == 0 {
                terminal.clear()?;
            }

            let elapsed = scan_start.elapsed().as_millis() as u64;
            terminal.draw(|frame| app::render::draw_scan(frame, observer, args, elapsed))?;

            frame_counter += 1;

            match rx.try_recv() {
                Ok(state) => return Ok(Some(state)),
                // the scan thread dropped its sender without producing a state
                // (it panicked), so bail out rather than spin forever
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(None),
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }

            if event::poll(frame_time)? {
                match event::read()? {
                    event::Event::FocusGained => frame_time = ACTIVE_FRAME_TIME,
                    event::Event::FocusLost => frame_time = IDLE_FRAME_TIME,
                    // cancel the scan on Press only (Windows also reports Release)
                    event::Event::Key(key)
                        if key.kind == event::KeyEventKind::Press
                            && matches!(key.code, event::KeyCode::Char('q' | 'Q') | event::KeyCode::Esc) =>
                    {
                        observer.cancel.store(true, Ordering::Relaxed);
                        return Ok(None);
                    }
                    _ => {}
                }
            }
        }
    })?;

    // user cancelled the scan (or it panicked), so there is nothing to convert
    let Some(mut state) = state else {
        ratatui::restore();
        return Ok(());
    };

    state.logs = logs;
    state.logs.note(format_args!(
        "conv2jxl started: {} files to process, args: {}",
        state.files.count(),
        std::env::args().skip(1).collect::<Vec<_>>().join(" ")
    ));

    // ---- Phase 2: convert, render progress ----

    let mut app = app::App {
        ui_state: app::ConvertingUIState {
            list_offset: 0,
            last_processing: vec![usize::MAX; args.parallel as usize],
            time: 0,

            file_tab: app::FileTab::Files,
            details: false,
            paused: false,
        },

        shared: std::sync::Arc::new(app::SharedState {
            args,
            conv: state,
            start: std::time::Instant::now(),
        }),
    };

    let mut threads = Vec::new();

    for i in 0..app.shared.args.parallel {
        let shared = app.shared.clone();

        threads.push(std::thread::spawn(move || {
            shared.run(i as usize);
        }));
    }

    // In --watch mode, promote the watcher we prepared before the scan into a
    // running promoter thread. Any events that arrived during phase 1 are
    // still in the channel and will be drained by the promoter. The
    // known-paths set is seeded from the scan's results so duplicates are
    // suppressed. conv.completed() only returns true once shutdown has been
    // signalled and the queue has drained, so the main loop won't exit
    // prematurely between batches.
    let watch_handle = pending_watcher.map(|p| app::watch::start(p, app.shared.clone()));

    let mut stopped = 0;

    let mut frame_counter = 0;

    let mut frame_time = ACTIVE_FRAME_TIME;

    loop {
        // do full clear periodically to avoid artifacts
        if frame_counter % (60 * 10) == 0 {
            terminal.clear()?;
        }

        let frame = terminal.draw(|frame| app.draw(frame))?;

        let size = frame.area.as_size();

        frame_counter += 1;

        if stopped > 0 && app.shared.conv.completed() {
            break;
        }

        // Block until an event arrives or the frame budget elapses. This paces
        // rendering without busy-waiting and responds to input immediately.
        if event::poll(frame_time)? {
            match event::read()? {
                event::Event::FocusGained => {
                    frame_time = ACTIVE_FRAME_TIME;
                }
                event::Event::FocusLost => {
                    frame_time = IDLE_FRAME_TIME;
                }
                // Windows reports both press AND release for every keystroke,
                // so only act on Press or one tap fires the handler twice.
                event::Event::Key(key) if key.kind == event::KeyEventKind::Press => match key.code {
                    event::KeyCode::Char('q' | 'Q') | event::KeyCode::Esc => {
                        stopped += 1;

                        if stopped >= 2 {
                            break; // kill immediately if already stopping
                        }

                        app.shared.stop();
                    }
                    event::KeyCode::Char('d' | 'D') => {
                        app.ui_state.details = !app.ui_state.details;
                    }
                    event::KeyCode::PageUp => app.add_offset(-(size.height as i32 * 3 / 2 + 1)),
                    event::KeyCode::PageDown => app.add_offset(size.height as i32 * 3 / 2 + 1),
                    event::KeyCode::Up => app.add_offset(-1),
                    event::KeyCode::Down => app.add_offset(1),
                    event::KeyCode::Tab => {
                        app.ui_state.list_offset = 0;

                        if key.modifiers.contains(event::KeyModifiers::SHIFT) {
                            app.ui_state.file_tab = app.ui_state.file_tab.prev();
                        } else {
                            app.ui_state.file_tab = app.ui_state.file_tab.next();
                        }
                    }
                    event::KeyCode::Char(' ') => app.toggle_pause(),
                    _ => {}
                },
                _ => {}
            }
        }
    }

    // wait for threads to finish if graceful stop
    if stopped <= 1 {
        for thread in threads {
            let _ = thread.join();
        }
    }

    // The watcher polls shutdown on a short timeout, so joining here doesn't
    // block for long once stop() has been called. On hard-quit (stopped >= 2)
    // we still join so the watcher backend is dropped cleanly.
    let watch_degraded = watch_handle.is_some_and(|h| h.join());

    ratatui::restore();

    // Reported after the TUI is gone, since it has nowhere to put a notice and
    // this is something the user has to act on outside the program anyway.
    if watch_degraded {
        eprintln!(concat!(
            "warning: the OS file-watch limit was reached during this run, so directories ",
            "created while it was watching were not watched. On Linux, raise it with ",
            "`sudo sysctl -w fs.inotify.max_user_watches=524288`."
        ));
    }

    let elapsed_ms = app.shared.start.elapsed().as_millis() as f64;
    app.shared.conv.report(elapsed_ms, stopped > 0);

    Ok(())
}

/// Exit with a plain message if `name` cannot be run at all. Its exit status
/// does not matter here, only whether the OS could find and start it.
fn require_tool(name: &str, purpose: &str) {
    use std::process::{Command, Stdio};

    if let Err(e) = Command::new(name)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        eprintln!("{name} is needed {purpose} but could not be run: {e}");
        eprintln!("install the libjxl tools and make sure {name} is on PATH");
        std::process::exit(2);
    }
}
