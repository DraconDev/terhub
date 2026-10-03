use ratatui::{backend::TestBackend, Terminal, widgets::{Block, Borders, Paragraph, Wrap}, style::Style};
fn main() {
    let raw = std::fs::read("/tmp/cap76.bin").unwrap();
    let s = String::from_utf8_lossy(&raw).into_owned();
    let w: usize = std::env::args().nth(1).unwrap().parse().unwrap();
    let text = terhub::ui::preview_text(&s, w);
    let be = TestBackend::new(100, 48);
    let mut term = Terminal::new(be).unwrap();
    term.draw(|f| {
        let area = f.area();
        let block = Block::default().borders(Borders::ALL);
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).style(Style::new()), inner);
    }).unwrap();
    let buf = term.backend().buffer();
    for y in 0..buf.area.height {
        let mut line = String::new();
        let mut bgs: Vec<(u16, u16)> = vec![];
        let mut run: Option<(u16, u16)> = None;
        for x in 0..buf.area.width {
            let c = &buf[(x, y)];
            line.push_str(c.symbol());
            if c.bg != ratatui::style::Color::Reset {
                match run.as_mut() { Some(r) => r.1 = x, None => run = Some((x, x)) }
            } else if let Some(r) = run.take() { bgs.push(r); }
        }
        if let Some(r) = run.take() { bgs.push(r); }
        if !bgs.is_empty() {
            println!("row={y:2} bg_runs={bgs:?} text={:?}", line.trim_end().chars().take(60).collect::<String>());
        }
    }
}
