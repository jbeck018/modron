//! Real-terminal host for the unified client app: crossterm events in,
//! ratatui frames out. With `--url <ws url>` [`--token <t>`] [`--session
//! <uuid>`] it drives a live server (`modron_app::session`); without
//! `--url` it plays the built-in demo script (`modron_app::demo`).
//! Ctrl+C exits.

#[cfg(target_arch = "wasm32")]
fn main() {
    // The web build is the `modron_app` library, mounted by the page.
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::io::Result<()> {
    terminal::main()
}

#[cfg(not(target_arch = "wasm32"))]
mod terminal {
    use std::{
        io,
        sync::{mpsc, Arc},
        time::Instant,
    };

    use crossterm::{
        event::{self, Event, KeyCode as K, KeyEvent, KeyEventKind, KeyModifiers as M},
        execute,
        terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
    };
    use modron_app::{
        chat::link,
        demo::Player,
        session::{self, LiveOptions},
        App,
    };
    use nsh_platform_tui::{
        run_app, AppOptions, AppView, CacheOptions, Input, Modifiers, Root, RunEvent,
    };
    use ratatui::{backend::CrosstermBackend, Terminal};

    fn key_input(key: KeyEvent) -> Input {
        let name = match key.code {
            K::Char(c) => c.to_string(),
            K::Enter => "Enter".into(),
            K::Esc => "Escape".into(),
            K::Backspace => "Backspace".into(),
            _ => "Unknown".into(),
        };
        let m = key.modifiers;
        let modifiers = Modifiers {
            ctrl: m.contains(M::CONTROL),
            alt: m.contains(M::ALT),
            shift: m.contains(M::SHIFT),
            cmd: m.contains(M::SUPER),
        };
        Input::Key { key: name, modifiers, release: false }
    }

    pub fn main() -> io::Result<()> {
        let live_options =
            LiveOptions::from_args(std::env::args().skip(1)).unwrap_or_else(|message| {
                eprintln!("modron-app-tui: {message}");
                eprintln!(
                    "usage: modron-app-tui [--url <ws url> [--token <t>] [--session <uuid>]]"
                );
                std::process::exit(2);
            });
        let mut live = live_options.map(session::spawn).transpose()?;
        let (mut host, client) = link();
        *modron_app::CLIENT.lock().expect("client slot") = Some(client);
        let (wake_tx, wake_rx) = mpsc::channel::<()>();
        let mut options = AppOptions::default();
        options.wake = Some(Arc::new(move || {
            let _ = wake_tx.send(());
        }));
        options.cache = CacheOptions::off();
        let mut view = AppView::new(options);
        view.load(Root::of::<App>())?;
        view.set_focused(true);

        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let (start, mut demo) = (Instant::now(), Player::default());
        let result = run_app(&mut term, &mut view, |wait| {
            let mut redraw = wake_rx.try_iter().count() > 0;
            match live.as_mut() {
                Some(live) => redraw |= live.pump(&mut host),
                None => {
                    redraw |= demo.pump(&mut host, start.elapsed().as_millis() as u64);
                }
            }
            if !redraw && event::poll(wait)? {
                return Ok(match event::read()? {
                    Event::Key(k)
                        if k.kind == KeyEventKind::Press
                            && k.code == K::Char('c')
                            && k.modifiers.contains(M::CONTROL) =>
                    {
                        RunEvent::Exit
                    }
                    Event::Key(k) if k.kind == KeyEventKind::Press => {
                        host.filter(key_input(k)).map_or(RunEvent::Redraw, RunEvent::Input)
                    }
                    Event::Paste(text) => RunEvent::Input(Input::Paste(text)),
                    Event::Resize(..) => RunEvent::Redraw,
                    _ => RunEvent::Idle,
                });
            }
            Ok(if redraw { RunEvent::Redraw } else { RunEvent::Idle })
        });
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
        result
    }
}
