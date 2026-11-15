//! Real-terminal host for the unified client app: crossterm events in,
//! ratatui frames out.
//!
//! With no flags it connects to the local server (`MODRON_URL`, else
//! `ws://127.0.0.1:7699`). When nothing answers there it starts `modron serve`
//! as its own child and stops it on exit (`--keep-server` leaves it running).
//! `--url <ws url>` connects to that server, `--demo` plays the built-in
//! script. See `modron_app::startup` for the order and the flags.
//! Ctrl+C exits; Esc also exits while the connection is being retried.

#[cfg(target_arch = "wasm32")]
fn main() {
    // The web build is the `modron_app` library, mounted by the page.
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    terminal::main()
}

#[cfg(not(target_arch = "wasm32"))]
mod terminal {
    use std::{
        io,
        path::{Path, PathBuf},
        process::ExitCode,
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
        session::{self, Live, LiveOptions, Status},
        startup::{self, Args, Plan, ServerChild, ServerSpec},
        App,
    };
    use nsh_platform_tui::{
        run_app, AppOptions, AppView, CacheOptions, Input, Modifiers, Root, RunEvent,
    };
    use ratatui::{backend::CrosstermBackend, widgets::Paragraph, Terminal};

    fn key_input(key: KeyEvent) -> Input {
        let name = match key.code {
            K::Char(c) => c.to_string(),
            K::Enter => "Enter".into(),
            K::Esc => "Escape".into(),
            K::Backspace => "Backspace".into(),
            K::Up => "ArrowUp".into(),
            K::Down => "ArrowDown".into(),
            K::F(2) => "F2".into(),
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

    /// Raw mode off and the alternate screen left. Safe to call more than once,
    /// and on every exit path (the panic hook calls it too).
    fn restore_terminal() {
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }

    /// Restores the terminal before the default panic report is printed, so
    /// the report is readable and the shell gets its terminal back.
    fn install_panic_hook() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    }

    /// A full-screen message that waits for one key press.
    fn message_screen(lines: &[String]) -> io::Result<()> {
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        let text = lines.join("\n");
        let result = (|| -> io::Result<()> {
            let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
            loop {
                term.draw(|f| f.render_widget(Paragraph::new(text.clone()), f.area()))?;
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        return Ok(());
                    }
                }
            }
        })();
        restore_terminal();
        result
    }

    fn exe_dir() -> Option<PathBuf> {
        std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf))
    }

    fn current_dir() -> Option<String> {
        std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned())
    }

    fn log_path() -> PathBuf {
        startup::log_path(
            std::env::var("XDG_STATE_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }

    fn fatal_lines(reason: &str, log: &Path) -> Vec<String> {
        vec![
            "modron-app could not reach a server.".into(),
            String::new(),
            reason.to_string(),
            String::new(),
            "Ways out:".into(),
            "  modron-app-tui --demo     run the built-in demo".into(),
            "  modron serve              start a server yourself, then run modron-app-tui".into(),
            String::new(),
            format!("Server log: {}", log.display()),
            String::new(),
            "Press any key to exit.".into(),
        ]
    }

    pub fn main() -> ExitCode {
        let args = match startup::parse_args(std::env::args().skip(1)) {
            Ok(args) => args,
            Err(message) => {
                eprintln!("modron-app-tui: {message}\n{}", startup::USAGE);
                return ExitCode::from(2);
            }
        };
        if args.help {
            println!("{}", startup::USAGE);
            return ExitCode::SUCCESS;
        }
        let env_url = std::env::var("MODRON_URL").ok().filter(|s| !s.is_empty());
        let plan = startup::decide(&args, env_url.as_deref(), startup::probe_ws, || {
            let env_bin = std::env::var("MODRON_BIN").ok();
            let path = std::env::var("PATH").ok();
            startup::find_bin(env_bin.as_deref(), exe_dir().as_deref(), path.as_deref(), |p| {
                p.is_file()
            })
        });
        install_panic_hook();
        let log = log_path();
        let (live, mut server) = match plan {
            Plan::Demo => (None, None),
            Plan::Connect { url, .. } => (Some(connect(&args, url)), None),
            Plan::Spawn { url, listen, bin } => {
                let spec = ServerSpec {
                    bin,
                    listen,
                    provider: args.provider.clone(),
                    model: args.model.clone(),
                    log: log.clone(),
                    dir: None,
                    home: None,
                };
                match ServerChild::start(&spec, startup::START_TIMEOUT) {
                    Ok(child) => (Some(connect(&args, url)), Some(child)),
                    Err(reason) => return fatal(&reason, &log),
                }
            }
            Plan::Fail { reason } => return fatal(&reason, &log),
        };
        if let (Some(live), Some(_)) = (live.as_ref(), server.as_ref()) {
            live.status.set_note(modron_mobile_core::view::OWN_SERVER_NOTE);
        }
        let outcome = run(live);
        if args.keep_server {
            if let Some(child) = server.as_mut() {
                child.detach();
            }
        }
        // The app's own server stops here (unless --keep-server).
        drop(server);
        match outcome {
            Ok(None) => ExitCode::SUCCESS,
            Ok(Some(reason)) => fatal(&reason, &log),
            Err(e) => {
                eprintln!("modron-app-tui: {e}");
                ExitCode::FAILURE
            }
        }
    }

    /// Shows `reason` on a full screen, waits for a key, and fails.
    fn fatal(reason: &str, log: &Path) -> ExitCode {
        if let Err(e) = message_screen(&fatal_lines(reason, log)) {
            eprintln!("modron-app-tui: {reason} ({e})");
        }
        ExitCode::FAILURE
    }

    fn connect(args: &Args, url: String) -> Live {
        session::spawn(LiveOptions {
            url,
            token: args.token.clone(),
            session: args.session,
            resume: startup::resume_of(args),
            cwd: current_dir(),
        })
        .unwrap_or_else(|e| panic!("cannot start the session driver: {e}"))
    }

    /// Runs the UI until Ctrl+C, Esc during a reconnect, or the driver's own
    /// stop. `Ok(Some(reason))` when the driver stopped with a reason.
    fn run(live: Option<Live>) -> io::Result<Option<String>> {
        let status: Arc<Status> =
            live.as_ref().map(|live| Arc::clone(&live.status)).unwrap_or_default();
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

        let mut live = live;
        terminal::enable_raw_mode()?;
        execute!(io::stdout(), EnterAlternateScreen)?;
        let (start, mut demo) = (Instant::now(), Player::default());
        let result = (|| -> io::Result<()> {
            let mut term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
            run_app(&mut term, &mut view, |wait| {
                if status.fatal().is_some() {
                    return Ok(RunEvent::Exit);
                }
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
                        // Esc leaves while the connection is down: there is
                        // nothing to interrupt, and the retry has no end.
                        Event::Key(k)
                            if k.kind == KeyEventKind::Press
                                && k.code == K::Esc
                                && status.reconnecting() =>
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
            })
        })();
        restore_terminal();
        result?;
        Ok(status.fatal())
    }
}
