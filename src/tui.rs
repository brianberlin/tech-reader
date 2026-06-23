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
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

use crate::audio::Spine;
use crate::transport::Transport;
use crate::types::Sentence;

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

/// Which view fills the body: the spoken prose, or the original source with the
/// narrated region highlighted.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Pane {
    Prose,
    Source,
}

impl Pane {
    fn toggle(self) -> Pane {
        match self {
            Pane::Prose => Pane::Source,
            Pane::Source => Pane::Prose,
        }
    }
}

/// Scroll/follow/speed/view state owned by the loop (not shared).
struct View {
    /// When true, the highlight follows the audible sentence and auto-scrolls.
    follow: bool,
    /// The highlighted row: the audible sentence while following, else the
    /// browse cursor. Always a *sentence* index, in both panes — so ↑/↓ and
    /// Enter-to-seek behave identically whichever view is shown.
    selected: usize,
    /// Index into `SPEED_LADDER`.
    speed_step: usize,
    /// Which body view is shown.
    pane: Pane,
    /// In the source view, whether long lines wrap (else they are truncated).
    wrap_source: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            follow: true,
            selected: 0,
            speed_step: DEFAULT_SPEED_STEP,
            pane: Pane::Prose,
            wrap_source: true,
        }
    }
}

/// Run the TUI until the user quits (or the audio device dies). Restores the
/// terminal before returning, even on error, so later stderr is not swallowed.
pub fn run(
    sentences: Arc<Mutex<Vec<Sentence>>>,
    source_lines: Arc<[String]>,
    spine: &Spine,
    transport: Arc<Transport>,
    synth_idle: Arc<AtomicBool>,
) -> io::Result<()> {
    let mut terminal = setup()?;
    let res = event_loop(
        &mut terminal,
        &sentences,
        &source_lines,
        spine,
        &transport,
        &synth_idle,
    );
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
    sentences: &Arc<Mutex<Vec<Sentence>>>,
    source_lines: &[String],
    spine: &Spine,
    transport: &Transport,
    synth_idle: &AtomicBool,
) -> io::Result<()> {
    let mut view = View::default();

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
            terminal.draw(|f| ui(f, &sents, source_lines, current, &view, &status))?;
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
                        KeyCode::Enter => commit_selection(transport, &mut view),
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

/// Commit the browse selection (Enter): jump the playhead to the highlighted
/// section. A no-op while following, since the cursor already tracks the
/// playhead and there is no pending selection to apply.
fn commit_selection(transport: &Transport, view: &mut View) {
    if !view.follow {
        let target = view.selected;
        seek(transport, view, target);
    }
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
        KeyCode::Tab => view.pane = view.pane.toggle(),
        KeyCode::Char('w') => view.wrap_source = !view.wrap_source,
        _ => {}
    }
    false
}

fn ui(
    frame: &mut Frame,
    sentences: &[Sentence],
    source_lines: &[String],
    current: Option<usize>,
    view: &View,
    status: &Status,
) {
    let rows = Layout::vertical([
        Constraint::Length(1), // header
        Constraint::Min(0),    // narration
        Constraint::Length(2), // footer (top-border separator + hint line)
    ])
    .split(frame.area());

    frame.render_widget(header(sentences.len(), current, view, status), rows[0]);

    match view.pane {
        Pane::Prose => render_prose(frame, rows[1], sentences, current, view),
        Pane::Source => render_source(frame, rows[1], source_lines, sentences, current, view),
    }

    frame.render_widget(footer(view), rows[2]);
}

/// The style of the playhead block (what is audible right now) — a solid cyan bar.
fn playhead_style() -> Style {
    Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)
}

/// The style of the browse selection (the pending jump target) — solid yellow,
/// deliberately distinct from the playhead.
fn selection_style() -> Style {
    Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
}

/// Prose view: the spoken sentences, the audible one highlighted (playhead) and
/// the browse cursor shown distinctly while browsing.
fn render_prose(
    frame: &mut Frame,
    area: Rect,
    sentences: &[Sentence],
    current: Option<usize>,
    view: &View,
) {
    if sentences.is_empty() {
        let waiting = Paragraph::new("Starting narration…").style(Style::default().fg(Color::DarkGray));
        frame.render_widget(waiting, area);
        return;
    }
    // Wrap each sentence to the available width; the active cursor row is
    // auto-scrolled into view by ListState.
    let width = area.width.saturating_sub(2).max(1) as usize;
    // The playhead (what's audible right now) is always painted, so the reading
    // position stays visible even while the user browses elsewhere.
    let playhead = playhead_style();
    let items: Vec<ListItem> = sentences
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let base = if current == Some(i) {
                playhead
            } else {
                Style::default().fg(Color::Gray)
            };
            let lines: Vec<Line> = wrap(&s.text, width)
                .into_iter()
                .map(|l| Line::from(Span::styled(l, base)))
                .collect();
            ListItem::new(Text::from(lines))
        })
        .collect();

    // The active cursor (the row ListState scrolls to and marks). While
    // following it *is* the playhead; while browsing it's the pending jump
    // target — shown in a distinct colour and symbol so it reads as a
    // selection to commit with Enter, not the reading position.
    let (cursor_style, cursor_symbol) = if view.follow {
        (playhead, "▸ ")
    } else {
        (selection_style(), "» ")
    };

    let list = List::new(items)
        .highlight_style(cursor_style)
        .highlight_symbol(cursor_symbol)
        .scroll_padding(2);

    let mut state = ListState::default();
    state.select(Some(view.selected));
    frame.render_stateful_widget(list, area, &mut state);
}

/// Source view: the original document with the narrated region highlighted. The
/// playhead's block is painted cyan; while browsing, the selected sentence's
/// block is painted yellow. ListState is used only to scroll the active block
/// into view — the highlight itself is per-line styling (a block spans many
/// rows, which `highlight_style` cannot express).
fn render_source(
    frame: &mut Frame,
    area: Rect,
    source_lines: &[String],
    sentences: &[Sentence],
    current: Option<usize>,
    view: &View,
) {
    if source_lines.is_empty() {
        return;
    }
    // 1-based inclusive source-line range of the block a sentence came from.
    let range_of = |i: usize| -> Option<(usize, usize)> {
        sentences.get(i).map(|s| (s.start_line, s.end_line))
    };
    let playhead_range = current.and_then(&range_of);
    let browse_range = (!view.follow).then(|| range_of(view.selected)).flatten();

    let total = source_lines.len();
    let gutter_digits = total.to_string().len();
    // Gutter is "<n> │ ": digits + space + bar + space.
    let gutter_w = gutter_digits + 3;
    let content_w = (area.width as usize).saturating_sub(gutter_w).max(1);

    let playhead = playhead_style();
    let selection = selection_style();
    let gutter_style = Style::default().fg(Color::DarkGray);
    let contains = |range: Option<(usize, usize)>, line_no: usize| {
        matches!(range, Some((lo, hi)) if line_no >= lo && line_no <= hi)
    };

    let items: Vec<ListItem> = source_lines
        .iter()
        .enumerate()
        .map(|(i, text)| {
            let line_no = i + 1; // 1-based, to match block ranges
            // Browse selection wins over the playhead on overlap, matching prose.
            let base = if contains(browse_range, line_no) {
                selection
            } else if contains(playhead_range, line_no) {
                playhead
            } else {
                Style::default().fg(Color::Gray)
            };
            let highlighted = contains(browse_range, line_no) || contains(playhead_range, line_no);

            let rows: Vec<String> = if view.wrap_source {
                wrap(text, content_w)
            } else {
                vec![text.chars().take(content_w).collect()]
            };
            let lines: Vec<Line> = rows
                .into_iter()
                .enumerate()
                .map(|(ri, mut row)| {
                    // Fill highlighted rows to the content width so the block
                    // reads as a solid bar rather than ragged-right.
                    if highlighted {
                        let pad = content_w.saturating_sub(row.chars().count());
                        row.extend(std::iter::repeat_n(' ', pad));
                    }
                    let gutter = if ri == 0 {
                        format!("{line_no:>gutter_digits$} │ ")
                    } else {
                        format!("{:gutter_digits$} │ ", "")
                    };
                    Line::from(vec![Span::styled(gutter, gutter_style), Span::styled(row, base)])
                })
                .collect();
            ListItem::new(Text::from(lines))
        })
        .collect();

    // Scroll the first line of the active block into view (no highlight styling
    // here — appearance is entirely per-line above).
    let anchor = if view.follow { playhead_range } else { browse_range }
        .map(|(lo, _)| lo)
        .unwrap_or(1)
        .saturating_sub(1)
        .min(total - 1);

    let list = List::new(items).scroll_padding(2);
    let mut state = ListState::default();
    state.select(Some(anchor));
    frame.render_stateful_widget(list, area, &mut state);
}

fn header(total: usize, current: Option<usize>, view: &View, status: &Status) -> Paragraph<'static> {
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
            match view.pane {
                Pane::Prose => "   ¶ prose",
                Pane::Source => "   </> source",
            },
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            if view.follow { "   ⟳ follow" } else { "   ‖ browsing" },
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

fn footer(view: &View) -> Paragraph<'static> {
    let mut parts: Vec<&str> = Vec::new();
    if !view.follow {
        parts.push("⏎ jump");
        parts.push("f follow");
    }
    parts.push("↑/↓ select");
    parts.push("Tab view");
    if matches!(view.pane, Pane::Source) {
        parts.push("w wrap");
    }
    parts.extend(["space pause", "←/→ seek", "−/+ speed", "q quit"]);
    let hint = parts.join(" · ");
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
    use ratatui::buffer::Buffer;

    /// A sentence carrying an explicit source-line range.
    fn sent(text: &str, start_line: usize, end_line: usize) -> Sentence {
        Sentence {
            text: text.to_string(),
            start_line,
            end_line,
        }
    }

    /// Prose sentences with throwaway 1-line ranges (the prose pane ignores them).
    fn prose(texts: &[&str]) -> Vec<Sentence> {
        texts
            .iter()
            .enumerate()
            .map(|(i, t)| sent(t, i + 1, i + 1))
            .collect()
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Render one frame to a test buffer with the given panes/state.
    fn draw(
        sentences: &[Sentence],
        source_lines: &[String],
        current: Option<usize>,
        view: &View,
        paused: bool,
    ) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(60, 12)).unwrap();
        let status = Status {
            paused,
            finished: false,
            speed: SPEED_LADDER[DEFAULT_SPEED_STEP],
            underruns: 0,
        };
        terminal
            .draw(|f| ui(f, sentences, source_lines, current, view, &status))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn render(sentences: &[Sentence], current: Option<usize>, follow: bool) -> Buffer {
        render_status(sentences, current, follow, false)
    }

    fn render_status(
        sentences: &[Sentence],
        current: Option<usize>,
        follow: bool,
        paused: bool,
    ) -> Buffer {
        let view = View {
            follow,
            selected: current.unwrap_or(0),
            ..View::default()
        };
        draw(sentences, &[], current, &view, paused)
    }

    /// Render the prose pane with an explicit `View`, so a test can set a browse
    /// cursor (`selected`) independent of the audible `current`.
    fn render_view(sentences: &[Sentence], current: Option<usize>, view: &View) -> Buffer {
        draw(sentences, &[], current, view, false)
    }

    /// The background colour of the first highlighted cell on the row holding
    /// `needle`, if any (the per-row highlight is a solid block).
    fn row_highlight_bg(buf: &ratatui::buffer::Buffer, needle: &str) -> Option<Color> {
        for y in 0..buf.area.height {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            if row.contains(needle) {
                return (0..buf.area.width)
                    .map(|x| buf[(x, y)].bg)
                    .find(|bg| *bg == Color::Cyan || *bg == Color::Yellow);
            }
        }
        None
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
        let s = prose(&[
            "First sentence here.",
            "Second sentence here.",
            "Third sentence here.",
        ]);
        let text = buffer_text(&render(&s, Some(1), true));
        assert!(text.contains("First sentence"), "{text}");
        assert!(text.contains("Second sentence"), "{text}");
        // Header shows the audible position (2 of 3) and follow mode.
        assert!(text.contains("2/3"), "{text}");
        assert!(text.contains("follow"), "{text}");
    }

    #[test]
    fn current_sentence_is_highlighted() {
        let s = prose(&["Alpha line.", "Beta line."]);
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
        let s = prose(&["Alpha."]);
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
        let mut v = View::default();
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
            ..View::default()
        };
        seek(&t, &mut v, 4);
        assert_eq!(v.selected, 4);
        assert!(v.follow, "seek re-enables follow so the highlight pins to the target");
        assert_eq!(t.seek_target(), 4);
        assert_eq!(t.generation(), 1);
    }

    #[test]
    fn browsing_marks_selection_distinct_from_playhead() {
        // Audible at sentence 0 (playhead), browse cursor parked on sentence 2.
        let s = prose(&["Alpha line.", "Beta line.", "Gamma line."]);
        let view = View {
            follow: false,
            selected: 2,
            ..View::default()
        };
        let buf = render_view(&s, Some(0), &view);
        // The playhead stays visible (cyan) even though the cursor is elsewhere,
        // and the selection reads in a distinct colour (yellow), not as a second
        // playhead — so the user can tell "reading here" from "jump target here".
        assert_eq!(
            row_highlight_bg(&buf, "Alpha"),
            Some(Color::Cyan),
            "playhead row should stay cyan while browsing"
        );
        assert_eq!(
            row_highlight_bg(&buf, "Gamma"),
            Some(Color::Yellow),
            "browse selection should be a distinct colour"
        );
    }

    #[test]
    fn following_cursor_is_the_playhead() {
        // While following there is no separate selection: the cursor is cyan.
        let s = prose(&["Alpha line.", "Beta line."]);
        let view = View {
            follow: true,
            selected: 1,
            ..View::default()
        };
        let buf = render_view(&s, Some(1), &view);
        assert_eq!(row_highlight_bg(&buf, "Beta"), Some(Color::Cyan));
        assert!(
            row_highlight_bg(&buf, "Alpha").is_none(),
            "no yellow selection should appear while following"
        );
    }

    #[test]
    fn enter_commits_browse_selection() {
        let t = Transport::new(1.0);
        let mut v = View {
            follow: false,
            selected: 7,
            ..View::default()
        };
        commit_selection(&t, &mut v);
        assert_eq!(t.seek_target(), 7, "Enter jumps the playhead to the selection");
        assert_eq!(t.generation(), 1);
        assert!(v.follow, "committing re-enables follow so the highlight pins to the target");
    }

    #[test]
    fn enter_is_noop_while_following() {
        let t = Transport::new(1.0);
        let mut v = View {
            follow: true,
            selected: 3,
            ..View::default()
        };
        commit_selection(&t, &mut v);
        assert_eq!(t.generation(), 0, "no seek is published when there is no pending selection");
    }

    fn lines(text: &[&str]) -> Vec<String> {
        text.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn source_pane_highlights_block_range() {
        // Sentence 0 maps to a block spanning source lines 2..=3.
        let src = lines(&["one", "two", "three", "four", "five"]);
        let s = vec![sent("spoken text", 2, 3)];
        let view = View {
            pane: Pane::Source,
            ..View::default()
        };
        let buf = draw(&s, &src, Some(0), &view, false);
        // The whole block (lines two & three) is cyan; lines outside it are not.
        assert_eq!(row_highlight_bg(&buf, "two"), Some(Color::Cyan));
        assert_eq!(row_highlight_bg(&buf, "three"), Some(Color::Cyan));
        assert_eq!(row_highlight_bg(&buf, "one"), None);
        assert_eq!(row_highlight_bg(&buf, "four"), None);
    }

    #[test]
    fn source_pane_browse_selection_distinct_from_playhead() {
        // Playhead on the sentence mapped to line 1; browse cursor on line 3.
        let src = lines(&["aaa", "bbb", "ccc", "ddd"]);
        let s = vec![sent("first", 1, 1), sent("second", 3, 3)];
        let view = View {
            pane: Pane::Source,
            follow: false,
            selected: 1,
            ..View::default()
        };
        let buf = draw(&s, &src, Some(0), &view, false);
        assert_eq!(
            row_highlight_bg(&buf, "aaa"),
            Some(Color::Cyan),
            "playhead line stays cyan while browsing the source"
        );
        assert_eq!(
            row_highlight_bg(&buf, "ccc"),
            Some(Color::Yellow),
            "browse selection in the source reads as a distinct colour"
        );
    }

    #[test]
    fn source_pane_shows_line_numbers() {
        let src = lines(&["alpha", "beta"]);
        let s = vec![sent("x", 1, 1)];
        let view = View {
            pane: Pane::Source,
            ..View::default()
        };
        let text = buffer_text(&draw(&s, &src, None, &view, false));
        // The gutter numbers the source lines.
        assert!(text.contains("1 │ alpha"), "{text}");
        assert!(text.contains("2 │ beta"), "{text}");
    }

    #[test]
    fn tab_toggles_pane() {
        let mut v = View::default();
        assert!(matches!(v.pane, Pane::Prose));
        assert!(!handle_key(key(KeyCode::Tab), &mut v, 1));
        assert!(matches!(v.pane, Pane::Source));
        handle_key(key(KeyCode::Tab), &mut v, 1);
        assert!(matches!(v.pane, Pane::Prose));
    }

    #[test]
    fn w_toggles_source_wrap() {
        let mut v = View::default();
        assert!(v.wrap_source, "wrapping is on by default");
        handle_key(key(KeyCode::Char('w')), &mut v, 1);
        assert!(!v.wrap_source);
        handle_key(key(KeyCode::Char('w')), &mut v, 1);
        assert!(v.wrap_source);
    }
}
