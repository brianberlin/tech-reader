//! Full-screen TUI (F6 / M4): scrolling narration with the current sentence
//! highlighted and auto-scrolled into view.
//!
//! The highlight is driven by the audio spine — `Spine::current_sentence()`
//! resolves the callback's consumed-sample count against the boundary table, so
//! the highlight tracks what is *actually audible*, not what has merely been
//! synthesized. The sentence list is the shared `Vec<String>` the synth worker
//! appends to as narration proceeds.
//!
//! Transport (pause/seek/speed) lands in M5; this milestone wires the render
//! loop, the audible-sentence highlight, scroll-follow, and quit/teardown.

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::audio::Spine;
use crate::transport::Transport;

type Backend = CrosstermBackend<Stdout>;

/// Discrete speed steps mapping to a pitch-preserving multiplier (§6.4). The
/// default is 1.0× (index 1).
const SPEED_LADDER: &[f32] = &[0.75, 1.0, 1.25, 1.5, 1.75, 2.0];
const DEFAULT_SPEED_STEP: usize = 1;

/// Status shown in the header, computed once per frame.
struct Status {
    paused: bool,
    finished: bool,
    speed: f32,
    underruns: u64,
}

/// Scroll/follow/speed state owned by the loop (not shared).
struct View {
    /// When true, the highlight follows the audible sentence and auto-scrolls.
    follow: bool,
    /// The highlighted row: the audible sentence while following, else the
    /// browse cursor.
    selected: usize,
    /// Index into `SPEED_LADDER`.
    speed_step: usize,
}

/// Run the TUI until the user quits (or the audio device dies). Restores the
/// terminal before returning, even on error, so later stderr is not swallowed.
pub fn run(
    sentences: Arc<Mutex<Vec<String>>>,
    spine: &Spine,
    transport: Arc<Transport>,
    synth_idle: Arc<AtomicBool>,
) -> io::Result<()> {
    let mut terminal = setup()?;
    let res = event_loop(&mut terminal, &sentences, spine, &transport, &synth_idle);
    restore(&mut terminal);
    res
}

fn setup() -> io::Result<Terminal<Backend>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    // Restore the terminal on panic so a crash doesn't wreck the user's shell.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut out = io::stdout();
        let _ = execute!(out, LeaveAlternateScreen);
        let _ = disable_raw_mode();
        prev(info);
    }));
    Terminal::new(CrosstermBackend::new(stdout))
}

fn restore(terminal: &mut Terminal<Backend>) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = terminal.show_cursor();
}

fn event_loop(
    terminal: &mut Terminal<Backend>,
    sentences: &Arc<Mutex<Vec<String>>>,
    spine: &Spine,
    transport: &Transport,
    synth_idle: &AtomicBool,
) -> io::Result<()> {
    let mut view = View {
        follow: true,
        selected: 0,
        speed_step: DEFAULT_SPEED_STEP,
    };

    loop {
        // Resolve the audible sentence first (locks the boundary table only), so
        // we never hold the sentence lock and the boundary lock at once.
        let current = spine.current_sentence();
        let status = Status {
            paused: spine.is_paused(),
            // The synth thread stays alive for seek, so "done" = it has nothing
            // left to produce and every pushed sample has played.
            finished: synth_idle.load(Ordering::Relaxed) && spine.is_drained(),
            speed: SPEED_LADDER[view.speed_step],
            underruns: spine.underruns(),
        };

        if view.follow {
            if let Some(c) = current {
                view.selected = c;
            }
        }

        {
            let sents = sentences.lock().unwrap();
            if !sents.is_empty() {
                view.selected = view.selected.min(sents.len() - 1);
            }
            terminal.draw(|f| ui(f, &sents, current, &view, &status))?;
        }

        // A dead device means the highlight can never advance — leave.
        if spine.is_consumer_dead() {
            break;
        }

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    // The transport position to act from: the audible sentence,
                    // or the browse cursor before playback has started.
                    let pos = current.unwrap_or(view.selected);
                    let total = sentences.lock().unwrap().len();
                    match key.code {
                        KeyCode::Char(' ') | KeyCode::Char('p') => {
                            spine.set_paused(!spine.is_paused());
                        }
                        KeyCode::Left => seek(transport, &mut view, pos.saturating_sub(1)),
                        KeyCode::Right => {
                            seek(transport, &mut view, (pos + 1).min(total.saturating_sub(1)))
                        }
                        KeyCode::Char('-') | KeyCode::Char('_') => {
                            change_speed(transport, &mut view, pos, -1)
                        }
                        KeyCode::Char('+') | KeyCode::Char('=') => {
                            change_speed(transport, &mut view, pos, 1)
                        }
                        _ => {
                            if handle_key(key, &mut view, total) {
                                break; // quit
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

/// Jump playback to `target` and snap the highlight there immediately (follow
/// keeps it pinned as the audio catches up).
fn seek(transport: &Transport, view: &mut View, target: usize) {
    transport.seek_to(target);
    view.selected = target;
    view.follow = true;
}

/// Step the speed by `delta` along the ladder and resume at `current`.
fn change_speed(transport: &Transport, view: &mut View, current: usize, delta: isize) {
    let last = SPEED_LADDER.len() - 1;
    let step = (view.speed_step as isize + delta).clamp(0, last as isize) as usize;
    if step != view.speed_step {
        view.speed_step = step;
        transport.set_speed(SPEED_LADDER[step], current);
    }
}

/// Apply a scroll/quit key. Returns true to quit.
fn handle_key(key: KeyEvent, view: &mut View, total: usize) -> bool {
    let last = total.saturating_sub(1);
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Up | KeyCode::Char('k') => {
            view.follow = false;
            view.selected = view.selected.saturating_sub(1);
        }
        KeyCode::Down | KeyCode::Char('j') => {
            view.follow = false;
            view.selected = (view.selected + 1).min(last);
        }
        KeyCode::PageUp => {
            view.follow = false;
            view.selected = view.selected.saturating_sub(10);
        }
        KeyCode::PageDown => {
            view.follow = false;
            view.selected = (view.selected + 10).min(last);
        }
        KeyCode::Home => {
            view.follow = false;
            view.selected = 0;
        }
        KeyCode::End => {
            view.follow = false;
            view.selected = last;
        }
        KeyCode::Char('f') => view.follow = true,
        _ => {}
    }
    false
}

fn ui(frame: &mut Frame, sentences: &[String], current: Option<usize>, view: &View, status: &Status) {
    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(0),    // narration
        Constraint::Length(2), // footer (top-border separator + hint line)
    ])
    .split(frame.area());

    frame.render_widget(header(sentences.len(), current, view.follow, status), rows[0]);

    if sentences.is_empty() {
        let waiting = Paragraph::new("Starting narration…").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(waiting, rows[1]);
    } else {
        // Wrap each sentence to the available width; the current/selected row is
        // highlighted and auto-scrolled into view by ListState.
        let width = rows[1].width.saturating_sub(2).max(1) as usize;
        let items: Vec<ListItem> = sentences
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let is_current = current == Some(i);
                let base = if is_current {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::Gray)
                };
                let lines: Vec<Line> = wrap(s, width)
                    .into_iter()
                    .map(|l| Line::from(Span::styled(l, base)))
                    .collect();
                ListItem::new(Text::from(lines))
            })
            .collect();

        let list = List::new(items)
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD))
            .highlight_symbol("▸ ")
            .scroll_padding(2);

        let mut state = ListState::default();
        state.select(Some(view.selected));
        frame.render_stateful_widget(list, rows[1], &mut state);
    }

    frame.render_widget(footer(view.follow), rows[2]);
}

fn header(total: usize, current: Option<usize>, follow: bool, status: &Status) -> Paragraph<'static> {
    let pos = current.map(|c| c + 1).unwrap_or(0);
    let state = if status.paused {
        Span::styled("⏸ paused", Style::default().fg(Color::Yellow))
    } else if status.finished {
        Span::styled("✓ done", Style::default().fg(Color::Green))
    } else {
        Span::styled("▶ reading", Style::default().fg(Color::Cyan))
    };
    let mut spans = vec![
        Span::styled("tech-reader", Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::raw("   "),
        state,
        Span::raw(format!("   {pos}/{total}")),
        Span::styled(
            format!("   {}", fmt_speed(status.speed)),
            Style::default().fg(Color::Gray),
        ),
        Span::styled(
            if follow { "   ⟳ follow" } else { "   ‖ browsing" },
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if status.underruns > 0 {
        spans.push(Span::styled(
            format!("   ⚠ {} underruns", status.underruns),
            Style::default().fg(Color::Yellow),
        ));
    }
    Paragraph::new(Line::from(spans))
}

/// Format a speed multiplier compactly: 1.0 -> "1×", 1.25 -> "1.25×".
fn fmt_speed(speed: f32) -> String {
    let s = format!("{speed:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s}×")
}

fn footer(follow: bool) -> Paragraph<'static> {
    let hint = if follow {
        "space pause · ←/→ seek · −/+ speed · ↑/↓ scroll · q quit"
    } else {
        "space pause · ←/→ seek · −/+ speed · ↑/↓ scroll · f follow · q quit"
    };
    Paragraph::new(Line::from(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    )))
    .block(Block::default().borders(Borders::TOP).border_style(Style::default().fg(Color::DarkGray)))
}

/// Greedy word-wrap to `width` columns (by char count). Always returns at least
/// one line.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_string()];
    }
    let mut lines = Vec::new();
    let mut cur = String::new();
    let mut cur_len = 0usize;
    for word in text.split_whitespace() {
        let wlen = word.chars().count();
        if cur.is_empty() {
            cur.push_str(word);
            cur_len = wlen;
        } else if cur_len + 1 + wlen <= width {
            cur.push(' ');
            cur.push_str(word);
            cur_len += 1 + wlen;
        } else {
            lines.push(std::mem::take(&mut cur));
            cur.push_str(word);
            cur_len = wlen;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn render(sentences: &[String], current: Option<usize>, follow: bool) -> ratatui::buffer::Buffer {
        render_status(sentences, current, follow, false)
    }

    fn render_status(
        sentences: &[String],
        current: Option<usize>,
        follow: bool,
        paused: bool,
    ) -> ratatui::buffer::Buffer {
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let view = View {
            follow,
            selected: current.unwrap_or(0),
            speed_step: DEFAULT_SPEED_STEP,
        };
        let status = Status {
            paused,
            finished: false,
            speed: SPEED_LADDER[DEFAULT_SPEED_STEP],
            underruns: 0,
        };
        terminal
            .draw(|f| ui(f, sentences, current, &view, &status))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn wrap_respects_width() {
        let lines = wrap("the quick brown fox jumps", 10);
        assert!(lines.iter().all(|l| l.chars().count() <= 10), "{lines:?}");
        assert_eq!(lines.join(" "), "the quick brown fox jumps");
    }

    #[test]
    fn renders_sentences_and_position() {
        let s = vec![
            "First sentence here.".to_string(),
            "Second sentence here.".to_string(),
            "Third sentence here.".to_string(),
        ];
        let text = buffer_text(&render(&s, Some(1), true));
        assert!(text.contains("First sentence"), "{text}");
        assert!(text.contains("Second sentence"), "{text}");
        // Header shows the audible position (2 of 3) and follow mode.
        assert!(text.contains("2/3"), "{text}");
        assert!(text.contains("follow"), "{text}");
    }

    #[test]
    fn current_sentence_is_highlighted() {
        let s = vec!["Alpha line.".to_string(), "Beta line.".to_string()];
        let buf = render(&s, Some(1), true);
        // Find the row containing "Beta" and confirm it carries the cyan
        // highlight background (the current-sentence style).
        let mut found = false;
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            if row.contains("Beta") {
                let has_cyan = (0..buf.area.width).any(|x| buf[(x, y)].bg == Color::Cyan);
                assert!(has_cyan, "current sentence row should be highlighted: {row}");
                found = true;
            }
        }
        assert!(found, "expected to find the Beta row");
    }

    #[test]
    fn empty_shows_starting() {
        let text = buffer_text(&render(&[], None, true));
        assert!(text.contains("Starting"), "{text}");
    }

    #[test]
    fn paused_state_in_header() {
        let s = vec!["Alpha.".to_string()];
        let playing = buffer_text(&render_status(&s, Some(0), true, false));
        assert!(playing.contains("reading"), "{playing}");
        let paused = buffer_text(&render_status(&s, Some(0), true, true));
        assert!(paused.contains("paused"), "{paused}");
    }

    #[test]
    fn speed_label_formats() {
        assert_eq!(fmt_speed(1.0), "1×");
        assert_eq!(fmt_speed(1.25), "1.25×");
        assert_eq!(fmt_speed(0.75), "0.75×");
        assert_eq!(fmt_speed(1.5), "1.5×");
    }

    #[test]
    fn speed_steps_clamp_and_publish() {
        let t = Transport::new(1.0);
        let mut v = View {
            follow: true,
            selected: 0,
            speed_step: DEFAULT_SPEED_STEP,
        };
        change_speed(&t, &mut v, 3, 1); // 1.0× -> 1.25×, resuming at sentence 3
        assert_eq!(v.speed_step, 2);
        assert_eq!(t.speed(), 1.25);
        assert_eq!(t.seek_target(), 3);
        for _ in 0..10 {
            change_speed(&t, &mut v, 0, 1);
        }
        assert_eq!(v.speed_step, SPEED_LADDER.len() - 1); // clamps at the top
        assert_eq!(t.speed(), 2.0);
        for _ in 0..10 {
            change_speed(&t, &mut v, 0, -1);
        }
        assert_eq!(v.speed_step, 0); // clamps at the bottom
        assert_eq!(t.speed(), 0.75);
    }

    #[test]
    fn seek_snaps_highlight_and_publishes_target() {
        let t = Transport::new(1.0);
        let mut v = View {
            follow: false,
            selected: 9,
            speed_step: DEFAULT_SPEED_STEP,
        };
        seek(&t, &mut v, 4);
        assert_eq!(v.selected, 4);
        assert!(v.follow, "seek re-enables follow so the highlight pins to the target");
        assert_eq!(t.seek_target(), 4);
        assert_eq!(t.generation(), 1);
    }
}
