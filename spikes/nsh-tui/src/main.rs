mod app;

use std::{
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

use nsh_platform_tui::{AppOptions, AppView, CacheOptions, Input, Modifiers, Root};
use ratatui::{backend::TestBackend, Terminal};

const COLS: u16 = 120;
const ROWS: u16 = 40;
const TIMEOUT: Duration = Duration::from_secs(10);

struct Harness {
    view: AppView,
    term: Terminal<TestBackend>,
    wake: mpsc::Receiver<()>,
}

impl Harness {
    fn new(rows: usize) -> Self {
        std::env::set_var("NSH_SPIKE_ROWS", rows.to_string());
        let (tx, wake) = mpsc::channel();
        let mut options = AppOptions::default();
        options.wake = Some(Arc::new(move || {
            let _ = tx.send(());
        }));
        options.cache = CacheOptions::off();
        let mut view = AppView::new(options);
        view.load(Root::of::<app::Chat>()).expect("load root");
        view.set_focused(true);
        let term = Terminal::new(TestBackend::new(COLS, ROWS)).expect("terminal");
        Self { view, term, wake }
    }

    fn draw(&mut self) -> Duration {
        let t = Instant::now();
        self.term
            .draw(|f| self.view.render(f, f.area()))
            .expect("draw");
        t.elapsed()
    }

    fn screen(&self) -> String {
        let buf = self.term.backend().buffer();
        buf.content
            .chunks(COLS as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Draws until `done` holds for the screen, waking on engine commits.
    fn wait_for(&mut self, what: &str, done: impl Fn(&str) -> bool) -> Result<(), String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            self.draw();
            let s = self.screen();
            if done(&s) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(format!("timed out waiting for {what}"));
            }
            let due = self.view.next_frame_deadline().map_or(deadline, |d| d.min(deadline));
            let wait = due
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(50));
            let _ = self.wake.recv_timeout(wait);
        }
    }
}

fn key(name: &str) -> Input {
    Input::Key {
        key: name.to_string(),
        modifiers: Modifiers::default(),
        release: false,
    }
}

fn stats(mut v: Vec<Duration>) -> (f64, f64, f64) {
    v.sort();
    let ms = |d: &Duration| d.as_secs_f64() * 1000.0;
    let n = v.len();
    let mean = v.iter().map(ms).sum::<f64>() / n as f64;
    let median = ms(&v[n / 2]);
    let p95 = ms(&v[((n as f64 * 0.95).ceil() as usize - 1).min(n - 1)]);
    (mean, median, p95)
}

fn startup(start: Instant) {
    let mut h = Harness::new(0);
    match h.wait_for("status row", |s| s.contains("status: rows")) {
        Ok(()) => println!("first_frame_ms={:.2}", start.elapsed().as_secs_f64() * 1000.0),
        Err(e) => println!("ERROR {e}"),
    }
}

fn stream(rows: usize) {
    let mut h = Harness::new(rows);
    h.wait_for("status row", |s| s.contains("status: rows")).expect("start");
    let mut times = Vec::new();
    for i in 1..=2000 {
        let t = Instant::now();
        h.view.handle(Input::Paste("tok ".to_string()));
        let want = format!("tokens {i} |");
        if let Err(e) = h.wait_for(&want, |s| s.contains(&want)) {
            println!("ERROR {e} at token {i}");
            return;
        }
        times.push(t.elapsed());
    }
    let (mean, median, p95) = stats(times);
    println!("stream rows={rows} tokens=2000 mean_ms={mean:.3} median_ms={median:.3} p95_ms={p95:.3}");
}

fn scroll() {
    let mut h = Harness::new(5000);
    if let Err(e) = h.wait_for("row 00000", |s| s.contains("row 00000")) {
        println!("ERROR {e}");
        return;
    }
    // The input holds focus, so the keys that scroll the list are PageUp and PageDown.
    let mut down = Vec::new();
    for _ in 0..200 {
        let before = h.screen();
        if before.contains("row 04999") {
            break;
        }
        let t = Instant::now();
        h.view.handle(key("PageDown"));
        if let Err(e) = h.wait_for("PageDown changes screen", |s| s != before) {
            println!("ERROR at page {} {e}; first rows: {:?}", down.len(), h.screen().lines().take(2).collect::<Vec<_>>());
            return;
        }
        down.push(t.elapsed());
    }
    let at_bottom = h.screen().contains("row 04999");
    println!("scroll_down pages={} reached_last_row={} {}", down.len(), at_bottom, fmt_stats(&down));
    let mut up = Vec::new();
    for _ in 0..200 {
        let before = h.screen();
        if before.contains("row 00000") {
            break;
        }
        let t = Instant::now();
        h.view.handle(key("PageUp"));
        if let Err(e) = h.wait_for("PageUp changes screen", |s| s != before) {
            println!("ERROR {e}");
            return;
        }
        up.push(t.elapsed());
    }
    let at_top = h.screen().contains("row 00000");
    println!("scroll_up pages={} back_at_top={} {}", up.len(), at_top, fmt_stats(&up));
}

fn fmt_stats(v: &[Duration]) -> String {
    let (mean, median, p95) = stats(v.to_vec());
    format!("mean_ms={mean:.3} median_ms={median:.3} p95_ms={p95:.3}")
}

fn input() {
    let mut h = Harness::new(0);
    h.wait_for("status row", |s| s.contains("status: rows")).expect("start");
    for c in "hello world".chars() {
        let name = c.to_string();
        h.view.handle(key(&name));
        if let Err(e) = h.wait_for("typed char", |s| s.contains(&c.to_string())) {
            println!("typing stalled at {c:?}: {e}");
        }
    }
    let typed = h.wait_for("typed text", |s| s.contains("hello world"));
    if typed.is_err() {
        println!("SCREEN\n{}", h.screen());
    }
    println!("typed_readback={}", if typed.is_ok() { "ok" } else { "FAIL" });
    h.view.handle(key("Enter"));
    let sub = h.wait_for("submitted row", |s| s.contains("> hello world"));
    println!("enter_submit_readback={}", if sub.is_ok() { "ok" } else { "FAIL" });
    h.view.handle(key("Tab"));
    h.view.handle(Input::Paste("line one line two".to_string()));
    let paste = h.wait_for("paste counted", |s| s.contains("tokens 1 |"));
    if paste.is_err() {
        println!("PASTE SCREEN\n{}", h.screen());
    }
    let shows = h.screen().contains("line one");
    println!("paste_counted={} paste_shows_text={shows}", if paste.is_ok() { "ok" } else { "FAIL" });
}

fn main() {
    let start = Instant::now();
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("startup") => startup(start),
        Some("stream") => stream(args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0)),
        Some("scroll") => scroll(),
        Some("input") => input(),
        Some("dump") => {
            let mut h = Harness::new(args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0));
            for _ in 0..5 { h.draw(); std::thread::sleep(Duration::from_millis(300)); }
            println!("{}", h.screen());
            h.view.handle(key("h"));
            h.view.handle(key("i"));
            for _ in 0..5 { h.draw(); std::thread::sleep(Duration::from_millis(300)); }
            println!("AFTER KEYS\n{}", h.screen());
            h.view.handle(Input::Paste("tok ".to_string()));
            for _ in 0..5 { h.draw(); std::thread::sleep(Duration::from_millis(300)); }
            println!("AFTER PASTE\n{}", h.screen());
        }
        _ => eprintln!("usage: nsh-tui-spike startup|stream <rows>|scroll|input"),
    }
}
