use std::collections::{HashMap, HashSet, VecDeque};
use std::io;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use ratatui::{
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    },
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
    Frame, Terminal,
};

use crate::repl::{handle_command, touch_lane};
use crate::state::{EventDef, LaneResultRow, State};
use crate::{fmt_time, place_label};

/// Maximum number of log lines retained in the TUI's scrollback ring.
pub(crate) const MAX_LOG: usize = 2000;

pub(crate) struct TuiApp {
    pub(crate) state: Arc<Mutex<State>>,
    pub(crate) log_rx: mpsc::Receiver<String>,
    pub(crate) log: VecDeque<String>,
    pub(crate) input: String,
    pub(crate) cursor: usize,
    pub(crate) history: Vec<String>,
    pub(crate) history_idx: Option<usize>,
    pub(crate) scratch: String,
    pub(crate) quit: bool,
    pub(crate) show_help: bool,
    pub(crate) help_scroll: u16,
    pub(crate) show_results: Option<u16>,
    pub(crate) show_lineup: bool,
    pub(crate) lineup_scroll: u16,
    pub(crate) focus: Focus,
    pub(crate) tree_expanded_events: HashSet<u16>,
    pub(crate) tree_expanded_heats: HashSet<(u16, u8)>,
    pub(crate) tree_selected: usize,
    pub(crate) tree_scroll: usize,
}

impl TuiApp {
    pub(crate) fn new(state: Arc<Mutex<State>>, log_rx: mpsc::Receiver<String>) -> Self {
        Self {
            state,
            log_rx,
            log: VecDeque::with_capacity(MAX_LOG),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_idx: None,
            scratch: String::new(),
            quit: false,
            show_help: false,
            help_scroll: 0,
            show_results: None,
            show_lineup: false,
            lineup_scroll: 0,
            focus: Focus::Input,
            tree_expanded_events: HashSet::new(),
            tree_expanded_heats: HashSet::new(),
            tree_selected: 0,
            tree_scroll: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Input,
    Tree,
}

/// One visible row in the stored-events tree.
#[derive(Debug, Clone)]
enum TreeRow {
    Event {
        event_no: u16,
        expanded: bool,
        heat_count: usize,
        race_count: usize,
    },
    Heat {
        event_no: u16,
        heat: u8,
        expanded: bool,
        race_count: usize,
    },
    Race {
        event_no: u16,
        heat: u8,
        race_no: u16,
    },
}

pub(crate) fn run_tui(app: &mut TuiApp) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = (|| -> io::Result<()> {
        while !app.quit {
            while let Ok(line) = app.log_rx.try_recv() {
                if app.log.len() >= MAX_LOG {
                    app.log.pop_front();
                }
                app.log.push_back(line);
            }
            terminal.draw(|f| draw(f, app))?;
            if event::poll(Duration::from_millis(100))? {
                handle_event(app, &event::read()?);
            }
        }
        Ok(())
    })();

    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
    res
}

fn draw(f: &mut Frame, app: &TuiApp) {
    let area = f.area();
    let lane_rows = {
        let s = app.state.lock().unwrap();
        let (lo, hi) = s.lane_spread;
        u16::from(hi.saturating_sub(lo).saturating_add(1))
    };
    let scoreboard_h = (lane_rows + 5).max(7);
    let chunks = Layout::vertical([
        Constraint::Length(scoreboard_h),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .split(area);
    let top = Layout::horizontal([Constraint::Min(40), Constraint::Length(42)]).split(chunks[0]);
    draw_scoreboard(f, top[0], &app.state);
    draw_stored_events(f, top[1], app);
    draw_log(f, chunks[1], &app.log);
    draw_input(f, chunks[2], app);
    if app.show_help {
        draw_help_popup(f, area, app);
    }
    if let Some(race_no) = app.show_results {
        draw_results_popup(f, area, &app.state, race_no);
    }
    if app.show_lineup {
        draw_lineup_popup(f, area, app);
    }
}

fn draw_scoreboard(f: &mut Frame, area: Rect, state: &Arc<Mutex<State>>) {
    let mut time_buf1 = [0u8; 16];
    let mut time_buf2 = [0u8; 16];
    let s = state.lock().unwrap();
    let (lo, hi) = s.lane_spread;
    let mut lines: Vec<Line> = Vec::with_capacity(12);
    match &s.in_progress {
        None => {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    "idle",
                    Style::new()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!(
                    "    event {}    heat {}    next race {}    lanes {lo}..{hi}",
                    s.current_event, s.current_heat, s.next_race_no,
                )),
            ]));
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "  press <Enter> in the prompt to start a race",
                Style::new().fg(Color::Gray),
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "  type {lo}..{hi} to record lane touches  ·  / to finalize  ·  /dq L to disqualify"
                ),
                Style::new().fg(Color::Gray),
            )));
        }
        Some(ip) => {
            let elapsed_ms = u32::try_from(
                Instant::now()
                    .saturating_duration_since(ip.started_at)
                    .as_millis()
                    .min(u128::from(u32::MAX)),
            )
            .unwrap_or(0);
            let event_def = s.lineup.get(ip.event as usize - 1);
            let label = event_def.map_or_else(|| format!("event {}", ip.event), EventDef::label);
            let total_segments = event_def.map_or(1, EventDef::total_segments);
            lines.push(Line::from(vec![
                Span::raw("  race "),
                Span::styled(
                    format!("{}", ip.race_no),
                    Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
                Span::raw("    running time  "),
                Span::styled(
                    fmt_time(elapsed_ms, &mut time_buf1),
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("    {label}  heat {}", ip.heat)),
            ]));
            lines.push(Line::from(Span::styled(
                "  ─────────────────────────────────────────────────────────────",
                Style::new().fg(Color::DarkGray),
            )));
            let places: HashMap<usize, u8> = {
                let mut ranked: Vec<(usize, u32)> = ip
                    .lanes
                    .iter()
                    .enumerate()
                    .filter(|(i, lt)| {
                        let ln = (u8::try_from(*i).unwrap_or(0)) + 1;
                        ln >= lo && ln <= hi && !lt.dq && lt.finished_at.is_some()
                    })
                    .map(|(i, lt)| (i, lt.finished_at.unwrap()))
                    .collect();
                ranked.sort_by_key(|(_, t)| *t);
                ranked
                    .into_iter()
                    .enumerate()
                    .map(|(rank, (idx, _))| (idx, u8::try_from(rank + 1).unwrap_or(0)))
                    .collect()
            };
            for (i, lt) in ip.lanes.iter().enumerate() {
                let lane_no = u8::try_from(i + 1).unwrap_or(0);
                if lane_no < lo || lane_no > hi {
                    continue;
                }
                let mut spans: Vec<Span> = vec![Span::styled(
                    format!("  Lane{lane_no:>2}  "),
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )];
                let place_span = match places.get(&i).copied() {
                    Some(p) => Span::styled(
                        format!("{:>3}  ", place_label(p)),
                        Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    ),
                    None => Span::raw("     "),
                };
                spans.push(place_span);
                if lt.dq {
                    spans.push(Span::styled(
                        "       DQ",
                        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
                    ));
                } else if let Some(&t) = lt.touches_ms.last() {
                    let (time_style, tag) = if let Some(fin_at) = lt.finished_at {
                        let since = elapsed_ms.saturating_sub(fin_at);
                        let phase = since / 250;
                        let bright = Style::new()
                            .fg(Color::Black)
                            .bg(Color::Green)
                            .add_modifier(Modifier::BOLD);
                        let steady = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
                        let style = if phase >= 6 {
                            steady
                        } else if phase % 2 == 0 {
                            bright
                        } else {
                            Style::new().fg(Color::DarkGray).add_modifier(Modifier::DIM)
                        };
                        (style, "  FINAL")
                    } else {
                        (
                            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                            "",
                        )
                    };
                    spans.push(Span::styled(
                        format!("{:>10}", fmt_time(t, &mut time_buf2)),
                        time_style,
                    ));
                    if !tag.is_empty() {
                        spans.push(Span::styled(
                            tag.to_string(),
                            Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                        ));
                    }
                    spans.push(Span::styled(
                        format!(
                            "  ({} touch{})",
                            lt.touches_ms.len(),
                            if lt.touches_ms.len() == 1 { "" } else { "es" }
                        ),
                        Style::new().fg(Color::DarkGray),
                    ));
                } else {
                    spans.push(Span::styled("        --", Style::new().fg(Color::DarkGray)));
                }
                if !lt.dq && lt.finished_at.is_none() && total_segments > 0 {
                    let next_touch = u16::try_from(lt.touches_ms.len())
                        .unwrap_or(0)
                        .saturating_add(1);
                    if next_touch == total_segments {
                        let on = (elapsed_ms / 500).is_multiple_of(2);
                        let style = if on {
                            Style::new()
                                .fg(Color::Black)
                                .bg(Color::Red)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::new()
                                .fg(Color::Red)
                                .add_modifier(Modifier::BOLD | Modifier::DIM)
                        };
                        spans.push(Span::styled("    FINISH ARMED", style));
                    } else if next_touch < total_segments {
                        spans.push(Span::styled(
                            format!("    SPLIT ARMED ({}/{})", next_touch, total_segments - 1),
                            Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                        ));
                    }
                }
                lines.push(Line::from(spans));
            }
        }
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(Span::styled(
            " scoreboard ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn build_tree_rows(state: &State, app: &TuiApp) -> Vec<TreeRow> {
    let mut by_event: HashMap<u16, Vec<(u8, &Vec<u16>)>> = HashMap::new();
    for ((event_no, heat), races) in &state.history {
        by_event.entry(*event_no).or_default().push((*heat, races));
    }
    let mut events: Vec<u16> = by_event.keys().copied().collect();
    events.sort_unstable();
    let mut rows = Vec::new();
    for event_no in events {
        let mut heats = by_event.remove(&event_no).unwrap();
        heats.sort_by_key(|(h, _)| *h);
        let heat_count = heats.len();
        let race_count: usize = heats.iter().map(|(_, r)| r.len()).sum();
        let event_expanded = app.tree_expanded_events.contains(&event_no);
        rows.push(TreeRow::Event {
            event_no,
            expanded: event_expanded,
            heat_count,
            race_count,
        });
        if !event_expanded {
            continue;
        }
        for (heat, race_list) in heats {
            let heat_expanded = app.tree_expanded_heats.contains(&(event_no, heat));
            rows.push(TreeRow::Heat {
                event_no,
                heat,
                expanded: heat_expanded,
                race_count: race_list.len(),
            });
            if !heat_expanded {
                continue;
            }
            for race_no in race_list {
                rows.push(TreeRow::Race {
                    event_no,
                    heat,
                    race_no: *race_no,
                });
            }
        }
    }
    rows
}

fn draw_stored_events(f: &mut Frame, area: Rect, app: &TuiApp) {
    let mut time_buf = [0u8; 16];
    let s = app.state.lock().unwrap();
    let rows = build_tree_rows(&s, app);
    let focused = app.focus == Focus::Tree;
    let border_color = if focused {
        Color::Magenta
    } else {
        Color::DarkGray
    };
    let title_style = Style::new().fg(border_color).add_modifier(Modifier::BOLD);
    let view_h = area.height.saturating_sub(2) as usize;
    let sel = app.tree_selected.min(rows.len().saturating_sub(1));
    let scroll = if rows.is_empty() {
        0
    } else if sel < app.tree_scroll {
        sel
    } else if view_h > 0 && sel >= app.tree_scroll + view_h {
        sel + 1 - view_h
    } else {
        app.tree_scroll
            .min(rows.len().saturating_sub(view_h.max(1)))
    };
    let mut lines: Vec<Line> = Vec::with_capacity(view_h.max(1));
    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no stored races yet)",
            Style::new().fg(Color::DarkGray),
        )));
    } else {
        for (i, row) in rows.iter().enumerate().skip(scroll).take(view_h.max(1)) {
            let is_sel = focused && i == sel;
            let cursor = if is_sel { "▶" } else { " " };
            let line = match row {
                TreeRow::Event {
                    event_no,
                    expanded,
                    heat_count,
                    race_count,
                } => {
                    let arrow = if *expanded { "▾" } else { "▸" };
                    let label = s
                        .lineup
                        .get(*event_no as usize - 1)
                        .map_or_else(|| format!("event {event_no}"), EventDef::label);
                    let txt = format!(
                        " {cursor} {arrow} Event {event_no:>2}  {label}  ({heat_count}h/{race_count}r)"
                    );
                    let mut style = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
                    if is_sel {
                        style = style.bg(Color::DarkGray);
                    }
                    Line::from(Span::styled(txt, style))
                }
                TreeRow::Heat {
                    heat,
                    expanded,
                    race_count,
                    ..
                } => {
                    let arrow = if *expanded { "▾" } else { "▸" };
                    let txt = format!(
                        "    {cursor} {arrow} Heat {heat:>2}  ({race_count} race{})",
                        if *race_count == 1 { "" } else { "s" }
                    );
                    let mut style = Style::new().fg(Color::White);
                    if is_sel {
                        style = style.bg(Color::DarkGray);
                    }
                    Line::from(Span::styled(txt, style))
                }
                TreeRow::Race { race_no, .. } => {
                    let summary = s.races.get(race_no).and_then(|r| {
                        r.lanes
                            .iter()
                            .filter_map(|opt| opt.as_ref())
                            .filter(|row| row.place == 1)
                            .map(|row| {
                                let t = row.lane.finish_ms();
                                if t > 0 {
                                    fmt_time(t, &mut time_buf).to_string()
                                } else {
                                    "--".to_string()
                                }
                            })
                            .next()
                    });
                    let summary = summary.unwrap_or_else(|| "--".to_string());
                    let txt = format!("       {cursor} race #{race_no}   1st {summary}");
                    let mut style = Style::new().fg(Color::Gray);
                    if is_sel {
                        style = style.bg(Color::DarkGray).fg(Color::White);
                    }
                    Line::from(Span::styled(txt, style))
                }
            };
            lines.push(line);
        }
    }
    let title = if focused {
        " stored events  [↑↓ ←/→ collapse/expand  ⏎ open race  Tab→input] "
    } else {
        " stored events  [Tab to focus] "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(border_color))
        .title(Span::styled(title, title_style));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_log(f: &mut Frame, area: Rect, log: &VecDeque<String>) {
    let inner_h = area.height.saturating_sub(2) as usize;
    let all: Vec<Line> = log
        .iter()
        .flat_map(|s| s.split('\n').map(Line::from).collect::<Vec<_>>())
        .collect();
    let start = all.len().saturating_sub(inner_h);
    let lines: Vec<Line> = all.into_iter().skip(start).collect();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::styled(" log ", Style::new().fg(Color::DarkGray)));
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::new().fg(Color::Gray))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_input(f: &mut Frame, area: Rect, app: &TuiApp) {
    const PROMPT: &str = "cts6> ";
    let focused = app.focus == Focus::Input;
    let (border_color, prompt_style, input_style) = if focused {
        (
            Color::Cyan,
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
        )
    } else {
        (
            Color::DarkGray,
            Style::new().fg(Color::DarkGray),
            Style::new().fg(Color::DarkGray),
        )
    };
    let title = if focused {
        " input  [Tab→stored events  F1 help] "
    } else {
        " input  [Tab to focus] "
    };
    let line = Line::from(vec![
        Span::styled(PROMPT, prompt_style),
        Span::styled(app.input.clone(), input_style),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(border_color))
        .title(title);
    f.render_widget(Paragraph::new(line).block(block), area);
    if focused {
        let cx = area.x
            + 1
            + u16::try_from(PROMPT.len()).unwrap_or(0)
            + u16::try_from(app.cursor).unwrap_or(0);
        let cy = area.y + 1;
        let max_x = area.x + area.width.saturating_sub(2);
        f.set_cursor_position((cx.min(max_x), cy));
    }
}

fn draw_help_popup(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let lines = build_help_lines();
    let content_h = u16::try_from(lines.len()).unwrap_or(0);
    let content_w = lines
        .iter()
        .map(|l| {
            u16::try_from(
                l.spans
                    .iter()
                    .map(|s| s.content.chars().count())
                    .sum::<usize>(),
            )
            .unwrap_or(0)
        })
        .max()
        .unwrap_or(0);
    let want_w = content_w.saturating_add(4);
    let want_h = content_h.saturating_add(2);
    let w = want_w.min(area.width.saturating_sub(2)).max(20);
    let h = want_h.min(area.height.saturating_sub(2)).max(5);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    let view_h = h.saturating_sub(2);
    let max_scroll = content_h.saturating_sub(view_h);
    let scroll = app.help_scroll.min(max_scroll);
    let title = if max_scroll > 0 {
        format!(
            " help [{}/{}]  ↑↓ PgUp/PgDn scroll  Esc to close ",
            scroll + 1,
            max_scroll + 1,
        )
    } else {
        String::from(" help — Esc/Enter to close ")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::new().fg(Color::White).bg(Color::Black));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        popup,
    );
}

fn help_max_scroll() -> u16 {
    let lines = build_help_lines();
    let content_h = u16::try_from(lines.len()).unwrap_or(0);
    let (term_w, term_h) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    let content_w = lines
        .iter()
        .map(|l| {
            u16::try_from(
                l.spans
                    .iter()
                    .map(|s| s.content.chars().count())
                    .sum::<usize>(),
            )
            .unwrap_or(0)
        })
        .max()
        .unwrap_or(0);
    let want_w = content_w.saturating_add(4);
    let want_h = content_h.saturating_add(2);
    let w = want_w.min(term_w.saturating_sub(2)).max(20);
    let h = want_h.min(term_h.saturating_sub(2)).max(5);
    let _ = w;
    let view_h = h.saturating_sub(2);
    content_h.saturating_sub(view_h)
}

fn build_help_lines() -> Vec<Line<'static>> {
    let header = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
    let cmd = Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let key = Style::new().fg(Color::Green).add_modifier(Modifier::BOLD);
    let dim = Style::new().fg(Color::Gray);

    let entry = |c: &'static str, d: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{c:<22}"), cmd),
            Span::raw(" "),
            Span::raw(d),
        ])
    };
    let key_entry = |c: &'static str, d: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{c:<22}"), key),
            Span::raw(" "),
            Span::raw(d),
        ])
    };
    let cont = |d: &'static str| -> Line<'static> {
        Line::from(vec![
            Span::raw("  "),
            Span::raw(" ".repeat(22)),
            Span::raw(" "),
            Span::styled(d, dim),
        ])
    };
    let section = |t: &'static str| Line::from(Span::styled(t, header));
    let blank = || Line::from("");

    let v: Vec<Line<'static>> = vec![
        Line::from(Span::styled(
            "cts6 emulator commands",
            Style::new()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
        )),
        blank(),
        section("meet setup:"),
        entry("/event N", "set active event"),
        entry("/heat N", "set active heat"),
        entry("/race N", "set next race number (resume mid-meet)"),
        entry("/lanes A..B", "set active lane spread (1..=10,"),
        cont("e.g. /lanes 1..10, 2..8, 1..1)"),
        entry("/lineup show", "list configured events"),
        entry(
            "/lineup preset P [G]",
            "load preset lineup; gender defaults to Mixed:",
        ),
        cont("P = hs | ncaa13 | ncaa15 | ncaa16"),
        cont("G = M | F | X"),
        entry("/lineup add D G S [Y]", "add event (distance, M|F|X,"),
        cont("stroke 1/FR 2/BK 3/BR 4/FL 5/IM"),
        cont("6/MED-R 7/FR-R DV, optional split-yds)"),
        entry("/lineup clear", "remove all events"),
        blank(),
        section("running a race:"),
        key_entry("<enter>", "start the race (timestamp 0.000)"),
        key_entry("1..9", "touch lane 1..9 (single keypress —"),
        cont("no Enter needed during a race)"),
        key_entry("0", "touch lane 10"),
        entry("/dq L", "mark lane L as DQ"),
        entry("/  or  /print", "finalize: last touch per lane = finish,"),
        cont("earlier touches = cumulative splits;"),
        cont("places assigned by ascending finish."),
        cont("Heat auto-bumps by 1."),
        blank(),
        section("inspection:"),
        entry("/races", "list stored races"),
        entry("/status", "show current state"),
        entry("/help", "this popup (Esc/Enter to close)"),
        entry("/quit", "exit"),
        blank(),
        section("stored events tree (right pane):"),
        key_entry("Tab", "focus the tree (Tab/Esc returns to input)"),
        key_entry("\u{2191} \u{2193} Home End", "navigate rows"),
        key_entry("\u{2192} / Enter", "expand event/heat (or open results"),
        cont("popup on a Race row)"),
        key_entry("\u{2190}", "collapse (or jump to parent on a race)"),
    ];

    v
}

fn draw_results_popup(frame: &mut Frame, area: Rect, state: &Arc<Mutex<State>>, race_no: u16) {
    let s = state.lock().unwrap();
    let Some(race) = s.lookup_by_race(race_no) else {
        return;
    };

    let event_label = s
        .lineup
        .get((race.event as usize).saturating_sub(1))
        .map_or_else(|| format!("event {}", race.event), EventDef::label);

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            format!("  Race {}  ", race.id),
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("Event {}  {}  ", race.event, event_label),
            Style::new().fg(Color::White),
        ),
        Span::styled(
            format!("Heat {}", race.heat),
            Style::new().fg(Color::Yellow),
        ),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Pl   Lane    Finish        Splits (cumulative)",
        Style::new()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "  ─────────────────────────────────────────────────────────────",
        Style::new().fg(Color::DarkGray),
    )));

    let mut rows: Vec<(usize, &LaneResultRow)> = race
        .lanes
        .iter()
        .enumerate()
        .filter_map(|(i, slot)| slot.as_ref().map(|r| (i, r)))
        .collect();
    rows.sort_by_key(|(i, r)| {
        let bucket: u8 = if r.place > 0 {
            0
        } else if r.lane.dq {
            1
        } else {
            2
        };
        (bucket, r.place, *i)
    });

    if rows.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no lane data recorded)",
            Style::new().fg(Color::DarkGray),
        )));
    }
    for (i, row) in &rows {
        let mut time_buf = [0u8; 16];
        let mut time_buf2 = [0u8; 16];
        let lane_no = (u8::try_from(*i).unwrap_or(0)) + 1;
        let place_str = if row.place > 0 {
            place_label(row.place)
        } else {
            "—".to_string()
        };
        let mut spans: Vec<Span> = vec![
            Span::styled(
                format!("  {place_str:>3}  "),
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("Lane{lane_no:>2}  "),
                Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
        ];
        if row.lane.dq {
            spans.push(Span::styled(
                format!("{:>10}  ", "DQ"),
                Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        } else if row.lane.touches_ms.is_empty() {
            spans.push(Span::styled(
                format!("{:>10}  ", "—"),
                Style::new().fg(Color::DarkGray),
            ));
        } else {
            spans.push(Span::styled(
                format!("{:>10}  ", fmt_time(row.lane.finish_ms(), &mut time_buf2)),
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            ));
        }
        let splits = row.lane.split_prefix();
        if splits.is_empty() {
            spans.push(Span::styled(
                "(no splits)",
                Style::new().fg(Color::DarkGray),
            ));
        } else {
            for (j, ms) in splits.iter().enumerate() {
                if j > 0 {
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::styled(
                    fmt_time(*ms, &mut time_buf).to_string(),
                    Style::new().fg(Color::Gray),
                ));
            }
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Esc/Enter to close",
        Style::new().fg(Color::DarkGray),
    )));

    let content_w = lines
        .iter()
        .map(|l| u16::try_from(l.width()).unwrap_or(0))
        .max()
        .unwrap_or(0);
    let content_h = u16::try_from(lines.len()).unwrap_or(0);
    let want_w = content_w.saturating_add(4);
    let want_h = content_h.saturating_add(2);
    let width = want_w.min(area.width.saturating_sub(2)).max(30);
    let height = want_h.min(area.height.saturating_sub(2)).max(7);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = Rect {
        x,
        y,
        width,
        height,
    };
    let title = format!(" race {} results — Esc/Enter to close ", race.id);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::new().fg(Color::White).bg(Color::Black));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn build_lineup_lines(s: &State) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            "  Event Lineup  ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                "({} event{})",
                s.lineup.len(),
                if s.lineup.len() == 1 { "" } else { "s" }
            ),
            Style::new().fg(Color::Gray),
        ),
    ]));
    lines.push(Line::from(""));

    if s.lineup.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (no lineup configured — try /lineup preset hs)",
            Style::new().fg(Color::DarkGray),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  #     Event              Splits    Touches",
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "  ──────────────────────────────────────────────────",
            Style::new().fg(Color::DarkGray),
        )));
        for (i, e) in s.lineup.iter().enumerate() {
            let event_no = u16::try_from(i + 1).unwrap_or(0);
            let is_current = event_no == s.current_event;
            let segs = e.total_segments();
            let marker = if is_current { "→" } else { " " };
            let row_style = if is_current {
                Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(Color::White)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {marker} {event_no:>3}  "),
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("{:<18} ", e.label()), row_style),
                Span::styled(
                    format!("{:>3}yd     ", e.split_yards),
                    Style::new().fg(Color::Gray),
                ),
                Span::styled(
                    format!("{:>3} touch{}", segs, if segs == 1 { " " } else { "es" }),
                    Style::new().fg(Color::Gray),
                ),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  ↑↓ PgUp/PgDn scroll  Esc/Enter to close",
        Style::new().fg(Color::DarkGray),
    )));
    lines
}

fn draw_lineup_popup(frame: &mut Frame, area: Rect, app: &TuiApp) {
    let lines = {
        let s = app.state.lock().unwrap();
        build_lineup_lines(&s)
    };

    let content_w = lines
        .iter()
        .map(|l| u16::try_from(l.width()).unwrap_or(0))
        .max()
        .unwrap_or(0);
    let content_h = u16::try_from(lines.len()).unwrap_or(0);
    let want_w = content_w.saturating_add(4);
    let want_h = content_h.saturating_add(2);
    let w = want_w.min(area.width.saturating_sub(2)).max(30);
    let h = want_h.min(area.height.saturating_sub(2)).max(7);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    let popup = Rect {
        x,
        y,
        width: w,
        height: h,
    };
    let view_h = h.saturating_sub(2);
    let max_scroll = content_h.saturating_sub(view_h);
    let scroll = app.lineup_scroll.min(max_scroll);
    let title = if max_scroll > 0 {
        format!(
            " event lineup [{}/{}]  ↑↓ PgUp/PgDn scroll  Esc to close ",
            scroll + 1,
            max_scroll + 1,
        )
    } else {
        String::from(" event lineup — Esc/Enter to close ")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::new().fg(Color::White).bg(Color::Black));
    frame.render_widget(Clear, popup);
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        popup,
    );
}

fn lineup_max_scroll(state: &Arc<Mutex<State>>) -> u16 {
    let lines = {
        let s = state.lock().unwrap();
        build_lineup_lines(&s)
    };
    let content_h = u16::try_from(lines.len()).unwrap_or(0);
    let (_term_w, term_h) = ratatui::crossterm::terminal::size().unwrap_or((80, 24));
    let content_w = lines
        .iter()
        .map(|l| u16::try_from(l.width()).unwrap_or(0))
        .max()
        .unwrap_or(0);
    let want_w = content_w.saturating_add(4);
    let want_h = content_h.saturating_add(2);
    let _ = want_w;
    let h = want_h.min(term_h.saturating_sub(2)).max(7);
    let view_h = h.saturating_sub(2);
    content_h.saturating_sub(view_h)
}

fn handle_event(app: &mut TuiApp, ev: &Event) {
    let key = match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => k,
        _ => return,
    };
    if app.show_help {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL) => app.quit = true,
            (KeyCode::Down | KeyCode::Char('j'), _) => {
                app.help_scroll = app.help_scroll.saturating_add(1).min(help_max_scroll());
            }
            (KeyCode::Up | KeyCode::Char('k'), _) => {
                app.help_scroll = app.help_scroll.saturating_sub(1);
            }
            (KeyCode::PageDown | KeyCode::Char(' '), _) => {
                app.help_scroll = app.help_scroll.saturating_add(8).min(help_max_scroll());
            }
            (KeyCode::PageUp, _) => {
                app.help_scroll = app.help_scroll.saturating_sub(8);
            }
            (KeyCode::Home, _) => app.help_scroll = 0,
            (KeyCode::End, _) => app.help_scroll = help_max_scroll(),
            _ => {
                app.show_help = false;
                app.help_scroll = 0;
            }
        }
        return;
    }
    if app.show_results.is_some() {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL) => app.quit = true,
            _ => app.show_results = None,
        }
        return;
    }
    if app.show_lineup {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL) => app.quit = true,
            (KeyCode::Down | KeyCode::Char('j'), _) => {
                app.lineup_scroll = app
                    .lineup_scroll
                    .saturating_add(1)
                    .min(lineup_max_scroll(&app.state));
            }
            (KeyCode::Up | KeyCode::Char('k'), _) => {
                app.lineup_scroll = app.lineup_scroll.saturating_sub(1);
            }
            (KeyCode::PageDown | KeyCode::Char(' '), _) => {
                app.lineup_scroll = app
                    .lineup_scroll
                    .saturating_add(8)
                    .min(lineup_max_scroll(&app.state));
            }
            (KeyCode::PageUp, _) => {
                app.lineup_scroll = app.lineup_scroll.saturating_sub(8);
            }
            (KeyCode::Home, _) => app.lineup_scroll = 0,
            (KeyCode::End, _) => app.lineup_scroll = lineup_max_scroll(&app.state),
            _ => {
                app.show_lineup = false;
                app.lineup_scroll = 0;
            }
        }
        return;
    }
    if app.focus == Focus::Tree {
        if matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL)
        ) {
            app.quit = true;
            return;
        }
        let rows = {
            let s = app.state.lock().unwrap();
            build_tree_rows(&s, app)
        };
        let last = rows.len().saturating_sub(1);
        match (key.code, key.modifiers) {
            (KeyCode::Tab | KeyCode::Esc, _) => app.focus = Focus::Input,
            (KeyCode::Up, _) => {
                app.tree_selected = app.tree_selected.saturating_sub(1);
            }
            (KeyCode::Down, _) if app.tree_selected < last => {
                app.tree_selected += 1;
            }
            (KeyCode::Home, _) => app.tree_selected = 0,
            (KeyCode::End, _) => app.tree_selected = last,
            (KeyCode::Right | KeyCode::Enter, _) => {
                if let Some(row) = rows.get(app.tree_selected) {
                    match row {
                        TreeRow::Event { event_no, .. } => {
                            app.tree_expanded_events.insert(*event_no);
                        }
                        TreeRow::Heat { event_no, heat, .. } => {
                            app.tree_expanded_heats.insert((*event_no, *heat));
                        }
                        TreeRow::Race { race_no, .. } => {
                            app.show_results = Some(*race_no);
                        }
                    }
                }
            }
            (KeyCode::Left, _) => {
                if let Some(row) = rows.get(app.tree_selected) {
                    match row {
                        TreeRow::Event { event_no, .. } => {
                            app.tree_expanded_events.remove(event_no);
                        }
                        TreeRow::Heat { event_no, heat, .. } => {
                            app.tree_expanded_heats.remove(&(*event_no, *heat));
                        }
                        TreeRow::Race { event_no, heat, .. } => {
                            app.tree_expanded_heats.remove(&(*event_no, *heat));
                            for (i, r) in rows.iter().enumerate() {
                                if let TreeRow::Heat {
                                    event_no: e,
                                    heat: h,
                                    ..
                                } = r
                                {
                                    if *e == *event_no && *h == *heat {
                                        app.tree_selected = i;
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        return;
    }
    match (key.code, key.modifiers) {
        (KeyCode::Char('c' | 'd'), KeyModifiers::CONTROL) | (KeyCode::Esc, _) => {
            app.quit = true;
        }
        (KeyCode::F(1), _) => {
            app.show_help = true;
            app.help_scroll = 0;
        }
        (KeyCode::Tab, _) => {
            app.focus = Focus::Tree;
            let row_count = {
                let s = app.state.lock().unwrap();
                build_tree_rows(&s, app).len()
            };
            if row_count == 0 {
                app.tree_selected = 0;
            } else if app.tree_selected >= row_count {
                app.tree_selected = row_count - 1;
            }
        }
        (KeyCode::Enter, _) => {
            let line = std::mem::take(&mut app.input);
            app.cursor = 0;
            if !line.trim().is_empty() {
                if app.history.last().map(std::string::String::as_str) != Some(line.as_str()) {
                    app.history.push(line.clone());
                }
            }
            app.history_idx = None;
            app.scratch.clear();
            let trimmed = line.trim();
            if trimmed == "/help" || trimmed == "/?" {
                app.show_help = true;
                app.help_scroll = 0;
                return;
            }
            if trimmed == "/lineup show" || trimmed == "/lineup" {
                app.show_lineup = true;
                app.lineup_scroll = 0;
                return;
            }
            let keep = {
                let mut s = app.state.lock().unwrap();
                handle_command(&mut s, &line)
            };
            if !keep {
                app.quit = true;
            }
        }
        (KeyCode::Backspace, _) if app.cursor > 0 => {
            app.cursor -= 1;
            app.input.remove(app.cursor);
        }
        (KeyCode::Delete, _) if app.cursor < app.input.len() => {
            app.input.remove(app.cursor);
        }
        (KeyCode::Left, _) if app.cursor > 0 => {
            app.cursor -= 1;
        }
        (KeyCode::Right, _) if app.cursor < app.input.len() => {
            app.cursor += 1;
        }
        (KeyCode::Home, _) => app.cursor = 0,
        (KeyCode::End, _) => app.cursor = app.input.len(),
        (KeyCode::Up, _) => {
            if app.history.is_empty() {
                return;
            }
            let new_idx = match app.history_idx {
                None => {
                    app.scratch = app.input.clone();
                    Some(app.history.len() - 1)
                }
                Some(0) => Some(0),
                Some(i) => Some(i - 1),
            };
            if let Some(i) = new_idx {
                app.input = app.history[i].clone();
                app.cursor = app.input.len();
                app.history_idx = new_idx;
            }
        }
        (KeyCode::Down, _) => match app.history_idx {
            None => {}
            Some(i) if i + 1 >= app.history.len() => {
                app.input = std::mem::take(&mut app.scratch);
                app.cursor = app.input.len();
                app.history_idx = None;
            }
            Some(i) => {
                let n = i + 1;
                app.input = app.history[n].clone();
                app.cursor = app.input.len();
                app.history_idx = Some(n);
            }
        },
        (KeyCode::Char(c), m) => {
            if m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                return;
            }
            if app.input.is_empty() && c.is_ascii_digit() {
                let mut s = app.state.lock().unwrap();
                if s.in_progress.is_some() {
                    let lane = if c == '0' {
                        10
                    } else {
                        u8::try_from(c.to_digit(10).unwrap()).unwrap_or(0)
                    };
                    touch_lane(&mut s, lane);
                    return;
                }
            }

            app.input.insert(app.cursor, c);
            app.cursor += 1;
        }
        _ => {}
    }
}
