//! Real-terminal host for the unified client app: crossterm events in,
//! ratatui frames out.
//!
//! With no flags it connects to the local server (`MODRON_URL`, else
//! `ws://127.0.0.1:7699`). When nothing answers there it starts `modron serve`
//! as its own child and stops it on exit (`--keep-server` leaves it running).
//! `--url <ws url>` connects to that server, `--demo` plays the built-in
//! script. See `modron_app::startup` for the order and the flags.
//! Ctrl+C interrupts a running turn; a second Ctrl+C within two seconds, or
//! Ctrl+C when idle, exits. Esc also exits while the connection is being
//! retried. Bracketed paste is on, so a pasted block arrives as one input.

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
        time::{Duration, Instant},
    };

    use crossterm::{
        event::{self, Event, KeyCode as K, KeyEvent, KeyEventKind, KeyModifiers as M},
        event::{DisableBracketedPaste, EnableBracketedPaste},
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
    use modron_mobile_core::{default_socket_path, device_token_from_env};
    use nsh_platform_tui::{
        run_app, AppOptions, AppView, CacheOptions, Input, Modifiers, Root, RunEvent,
    };
    use ratatui::{backend::CrosstermBackend, widgets::Paragraph, Terminal};
    use uuid::Uuid;

    /// Ctrl+J (a raw line feed) and Ctrl+Enter are the newline keys: they
    /// reach the composer as Shift+Enter, which NSH's multiline input breaks
    /// the line on.
    fn is_newline_chord(key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(M::CONTROL);
        ctrl && matches!(key.code, K::Char('j' | 'J') | K::Enter)
    }

    /// The key name NSH's keymap reads for a crossterm key. Keys the app has
    /// no use for are `Unknown`, which the keymap ignores.
    fn key_name(code: K, shift: bool) -> String {
        match code {
            K::Char(c) => c.to_string(),
            K::Enter => "Enter".into(),
            K::Esc => "Escape".into(),
            K::Backspace => "Backspace".into(),
            K::Tab if shift => "BackTab".into(),
            K::Tab => "Tab".into(),
            K::BackTab => "BackTab".into(),
            K::Up => "ArrowUp".into(),
            K::Down => "ArrowDown".into(),
            K::Left => "ArrowLeft".into(),
            K::Right => "ArrowRight".into(),
            K::Home => "Home".into(),
            K::End => "End".into(),
            K::Delete => "Delete".into(),
            K::PageUp => "PageUp".into(),
            K::PageDown => "PageDown".into(),
            K::F(n @ 1..=12) => format!("F{n}"),
            _ => "Unknown".into(),
        }
    }

    fn key_input(key: KeyEvent) -> Input {
        let mut m = key.modifiers;
        let (code, newline) = (key.code, is_newline_chord(&key));
        if newline {
            m.insert(M::SHIFT);
        }
        let name = if newline { "Enter".to_string() } else { key_name(code, m.contains(M::SHIFT)) };
        let modifiers = Modifiers {
            ctrl: m.contains(M::CONTROL) && !newline,
            alt: m.contains(M::ALT),
            shift: m.contains(M::SHIFT),
            cmd: m.contains(M::SUPER),
        };
        Input::Key { key: name, modifiers, release: false }
    }

    /// What Ctrl+C does: the first press during a turn interrupts it; a press
    /// when idle, or a second one within [`CTRL_C_WINDOW`], exits.
    #[derive(Debug, PartialEq, Eq)]
    enum CtrlC {
        Interrupt,
        Exit,
    }

    const CTRL_C_WINDOW: Duration = Duration::from_secs(2);
    const CTRL_C_HINT: &str = "press ctrl+c again to quit";

    fn ctrl_c(running: bool, last: Option<Instant>, now: Instant) -> CtrlC {
        let again = last.is_some_and(|t| now.saturating_duration_since(t) <= CTRL_C_WINDOW);
        if running && !again {
            CtrlC::Interrupt
        } else {
            CtrlC::Exit
        }
    }

    /// The line printed after the terminal is restored, so the session can
    /// be reopened.
    fn resume_hint(id: Uuid) -> String {
        format!("session: {id} - resume with modron-app-tui --session {id}")
    }

    /// Raw mode off and the alternate screen left. Safe to call more than once,
    /// and on every exit path (the panic hook calls it too).
    fn restore_terminal() {
        let _ = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen);
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
        let env = startup::Env { url: env_url.clone(), socket: default_socket_path() };
        let plan = startup::decide(&args, &env, startup::probe_ws, startup::probe_unix, || {
            let env_bin = std::env::var("MODRON_BIN").ok();
            let path = std::env::var("PATH").ok();
            startup::find_bin(env_bin.as_deref(), exe_dir().as_deref(), path.as_deref(), |p| {
                p.is_file()
            })
        });
        let device = device_token_from_env();
        let token = startup::resolve_token(
            args.token.as_deref(),
            args.url.as_deref().or(env_url.as_deref()),
            device.as_deref(),
        );
        install_panic_hook();
        let log = log_path();
        let (live, mut server) = match plan {
            Plan::Demo => (None, None),
            Plan::Connect { url, .. } => (Some(connect(&args, url, token)), None),
            Plan::Spawn { url, listen, bin } => {
                let spec = ServerSpec {
                    bin,
                    listen,
                    provider: args.provider.clone(),
                    model: args.model.clone(),
                    log: log.clone(),
                    dir: None,
                    home: None,
                    working_dir: args.working_dir.clone(),
                    sessions_dir: args.sessions_dir.clone(),
                };
                match ServerChild::start(&spec, startup::START_TIMEOUT) {
                    Ok(child) => (Some(connect(&args, url, token)), Some(child)),
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
        let (outcome, session) = match outcome {
            Ok((reason, session)) => (Ok(reason), session),
            Err(e) => (Err(e), None),
        };
        // The terminal is restored by now: the hint lands in the normal buffer.
        if let Some(id) = session {
            println!("{}", resume_hint(id));
        }
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

    fn connect(args: &Args, url: String, token: Option<String>) -> Live {
        session::spawn(LiveOptions {
            url,
            token,
            session: args.session,
            resume: startup::resume_of(args),
            cwd: current_dir(),
            insecure_lan: args.insecure_lan,
            startup: startup::startup_patches(args),
        })
        .unwrap_or_else(|e| panic!("cannot start the session driver: {e}"))
    }

    /// Runs the UI until Ctrl+C, Esc during a reconnect, or the driver's own
    /// stop. `Ok(Some(reason))` when the driver stopped with a reason; the
    /// session the app was on, for the resume hint.
    fn run(live: Option<Live>) -> io::Result<(Option<String>, Option<Uuid>)> {
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
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        let (start, mut demo) = (Instant::now(), Player::default());
        let mut last_ctrl_c: Option<Instant> = None;
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
                            let now = Instant::now();
                            match ctrl_c(host.running(), last_ctrl_c, now) {
                                CtrlC::Interrupt => {
                                    last_ctrl_c = Some(now);
                                    host.interrupt();
                                    status.set_note(CTRL_C_HINT);
                                    RunEvent::Redraw
                                }
                                CtrlC::Exit => RunEvent::Exit,
                            }
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
        let session = host.session();
        restore_terminal();
        result?;
        Ok((status.fatal(), session))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn press(code: K, mods: M) -> KeyEvent {
            KeyEvent::new(code, mods)
        }

        /// (crossterm key, expected key name, ctrl, alt, shift).
        #[test]
        fn key_map_forwards_the_keys_the_app_uses() {
            let table: &[(K, M, &str, bool, bool, bool)] = &[
                (K::Tab, M::NONE, "Tab", false, false, false),
                (K::Tab, M::SHIFT, "BackTab", false, false, true),
                (K::BackTab, M::SHIFT, "BackTab", false, false, true),
                (K::F(1), M::NONE, "F1", false, false, false),
                (K::F(2), M::NONE, "F2", false, false, false),
                (K::F(12), M::NONE, "F12", false, false, false),
                (K::F(13), M::NONE, "Unknown", false, false, false),
                (K::Left, M::NONE, "ArrowLeft", false, false, false),
                (K::Right, M::ALT, "ArrowRight", false, true, false),
                (K::Left, M::CONTROL, "ArrowLeft", true, false, false),
                (K::Home, M::NONE, "Home", false, false, false),
                (K::End, M::SHIFT, "End", false, false, true),
                (K::Delete, M::NONE, "Delete", false, false, false),
                (K::PageUp, M::NONE, "PageUp", false, false, false),
                (K::PageDown, M::ALT, "PageDown", false, true, false),
                (K::Up, M::NONE, "ArrowUp", false, false, false),
                (K::Down, M::NONE, "ArrowDown", false, false, false),
                (K::Esc, M::NONE, "Escape", false, false, false),
                (K::Backspace, M::NONE, "Backspace", false, false, false),
                (K::Enter, M::NONE, "Enter", false, false, false),
                (K::Enter, M::SHIFT, "Enter", false, false, true),
                (K::Enter, M::ALT, "Enter", false, true, false),
                (K::Char('x'), M::ALT, "x", false, true, false),
                (K::Char('a'), M::CONTROL, "a", true, false, false),
                (K::Null, M::NONE, "Unknown", false, false, false),
            ];
            for &(code, mods, name, ctrl, alt, shift) in table {
                let Input::Key { key, modifiers, release } = key_input(press(code, mods)) else {
                    panic!("not a key: {code:?}");
                };
                assert_eq!(key, name, "{code:?} {mods:?}");
                assert_eq!(
                    (modifiers.ctrl, modifiers.alt, modifiers.shift),
                    (ctrl, alt, shift),
                    "{code:?} {mods:?}"
                );
                assert!(!release);
            }
        }

        #[test]
        fn ctrl_j_and_ctrl_enter_are_shift_enter() {
            for code in [K::Char('j'), K::Char('J'), K::Enter] {
                let Input::Key { key, modifiers, .. } = key_input(press(code, M::CONTROL)) else {
                    panic!("not a key");
                };
                assert_eq!(key, "Enter");
                assert!(modifiers.shift && !modifiers.ctrl, "{code:?}");
            }
            // A plain j is a letter.
            let Input::Key { key, .. } = key_input(press(K::Char('j'), M::NONE)) else {
                panic!("not a key");
            };
            assert_eq!(key, "j");
        }

        #[test]
        fn ctrl_c_interrupts_once_mid_turn_and_exits_otherwise() {
            let t0 = Instant::now();
            // Idle: exit at once.
            assert_eq!(ctrl_c(false, None, t0), CtrlC::Exit);
            // Running: the first press interrupts.
            assert_eq!(ctrl_c(true, None, t0), CtrlC::Interrupt);
            // A second press within two seconds exits, still running.
            let t1 = t0 + Duration::from_millis(1500);
            assert_eq!(ctrl_c(true, Some(t0), t1), CtrlC::Exit);
            // After the window the press interrupts again.
            let t2 = t0 + Duration::from_secs(3);
            assert_eq!(ctrl_c(true, Some(t0), t2), CtrlC::Interrupt);
            // Idle after an interrupt: exit.
            assert_eq!(ctrl_c(false, Some(t0), t1), CtrlC::Exit);
        }

        #[test]
        fn resume_hint_names_the_session_and_the_command() {
            let id = Uuid::from_u128(0xabc);
            assert_eq!(
                resume_hint(id),
                format!("session: {id} - resume with modron-app-tui --session {id}")
            );
        }
    }
}
