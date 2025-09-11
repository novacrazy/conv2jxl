#![allow(clippy::single_char_add_str)]

pub mod app;
pub mod cli;
pub mod formatting;
pub mod pool;
pub mod utils;

use std::{io::Result, thread::sleep, time::Duration};

use crossterm::event;

use crate::app::scan::ScanObserver;

fn main() -> Result<()> {
    let mut args: cli::Conv2JxlArgs = argh::from_env();

    args.normalize();

    let state = args.scan(&ScanObserver::default()).expect("Failed to scan files");

    let mut terminal = ratatui::init();

    terminal.clear()?;

    let mut app = app::App {
        ui_state: app::ConvertingUIState {
            list_offset: 0,
            last_processing: vec![usize::MAX; args.parallel as usize],
            time: 0,

            file_tab: app::FileTab::Files,
            details: false,
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

    let mut stopped = 0;

    let mut debounce = std::time::Instant::now();

    let mut frame_counter = 0;

    const ACTIVE_FRAME_TIME: Duration = Duration::from_millis(1000 / 10);
    const IDLE_FRAME_TIME: Duration = Duration::from_millis(1000 / 2);

    let mut sleep_time = ACTIVE_FRAME_TIME;

    let mut size = terminal.size()?;

    loop {
        if let Ok(true) = event::poll(Duration::ZERO) {
            let event = event::read()?;

            let now = std::time::Instant::now();

            if now.duration_since(debounce) < Duration::from_millis(250) {
                continue;
            }

            debounce = now;

            match event {
                event::Event::FocusGained => {
                    sleep_time = ACTIVE_FRAME_TIME;
                }
                event::Event::FocusLost => {
                    sleep_time = IDLE_FRAME_TIME;
                }
                event::Event::Key(key) => match key.code {
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

        if stopped > 0 && app.shared.conv.completed() {
            break;
        }

        // do full clear every minute to avoid artifacts
        if frame_counter % (60 * 10) == 0 {
            terminal.clear()?;
        }

        let frame = terminal.draw(|frame| app.draw(frame))?;

        size = frame.area.as_size();

        frame_counter += 1;

        sleep(sleep_time); // limit to 10 FPS
    }

    // wait for threads to finish if graceful stop
    if stopped <= 1 {
        for thread in threads {
            let _ = thread.join();
        }
    }

    ratatui::restore();

    Ok(())
}
