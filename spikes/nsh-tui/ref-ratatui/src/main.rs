use ratatui::{crossterm::event, widgets::Paragraph, DefaultTerminal};

// Minimal ratatui app: one paragraph, exits on the first key. Built only for size.
fn main() -> std::io::Result<()> {
    let mut terminal: DefaultTerminal = ratatui::init();
    let result = run(&mut terminal);
    ratatui::restore();
    result
}

fn run(terminal: &mut DefaultTerminal) -> std::io::Result<()> {
    loop {
        terminal.draw(|f| f.render_widget(Paragraph::new("hello ratatui"), f.area()))?;
        if let event::Event::Key(_) = event::read()? {
            return Ok(());
        }
    }
}
