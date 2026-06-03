use std::thread;
use std::time::{Duration, Instant};

use crate::state::{
    EventDef, Gender, InProgress, LaneResultRow, Race, State, Stroke, MAX_LANES, NUM_SLOTS,
};
use crate::{fmt_time, play_beep};

#[allow(unused_macros)]
macro_rules! println {
    ($($arg:tt)*) => { $crate::emu_log!($($arg)*) };
}
#[allow(unused_macros)]
macro_rules! print {
    ($($arg:tt)*) => { $crate::emu_log!($($arg)*) };
}

pub(crate) fn parse_gender(s: &str) -> Option<Gender> {
    match s.to_ascii_uppercase().as_str() {
        "M" | "BOYS" | "MEN" => Some(Gender::Male),
        "F" | "GIRLS" | "WOMEN" | "W" => Some(Gender::Female),
        "X" | "MIXED" => Some(Gender::Mixed),
        _ => None,
    }
}

/// Accepts the CTS6 numeric codes (1–7), the short text codes
/// (FR / BK / BR / FL / IM / MED-R / FR-R / DV), and longer English
/// aliases (FREESTYLE, BACKSTROKE, FREE-RELAY, MEDLEY-RELAY, ...).
/// Crucially, bare `FR` always means Free (code 1); free relay must
/// be spelled `FR-R` (or `7` / `FREE-RELAY` / `FREESTYLE-RELAY`).
pub(crate) fn parse_stroke(s: &str) -> Option<Stroke> {
    let up = s.trim().to_ascii_uppercase();
    match up.as_str() {
        "1" | "FR" | "FREE" | "FREESTYLE" => return Some(Stroke::Free),
        "2" | "BK" | "BACK" | "BACKSTROKE" => return Some(Stroke::Back),
        "3" | "BR" | "BREAST" | "BREASTSTROKE" => return Some(Stroke::Breast),
        "4" | "FL" | "FLY" | "BUTTERFLY" => return Some(Stroke::Fly),
        "5" | "IM" | "MEDLEY" => return Some(Stroke::Im),
        "6" | "MED-R" | "MEDLEY-RELAY" | "MEDLEYRELAY" | "MR" => {
            return Some(Stroke::MedleyRelay);
        }
        "7" | "FR-R" | "FREE-RELAY" | "FREERELAY" | "FREESTYLE-RELAY" | "FREESTYLERELAY" => {
            return Some(Stroke::FreestyleRelay)
        }
        "DV" | "DIVE" | "DIVING" => return Some(Stroke::Diving),
        _ => {}
    }
    None
}

/// Parse a lane spread spec like "1..10", "2..8", "1..1" into an
/// inclusive `(low, high)` pair.
pub(crate) fn parse_lane_spread(s: &str) -> Option<(u8, u8)> {
    let (a, b) = s.split_once("..")?;
    let lo: u8 = a.trim().parse().ok()?;
    let hi: u8 = b.trim().parse().ok()?;
    if lo < 1
        || hi < 1
        || lo > u8::try_from(MAX_LANES).unwrap_or(0)
        || hi > u8::try_from(MAX_LANES).unwrap_or(0)
        || lo > hi
    {
        return None;
    }
    Some((lo, hi))
}

const STARTER_BEEP_GAP: Duration = Duration::from_millis(150);

fn starter_beeps() {
    play_beep();
    thread::spawn(move || {
        thread::sleep(STARTER_BEEP_GAP);
        play_beep();
        thread::sleep(STARTER_BEEP_GAP);
        play_beep();
    });
}

pub(crate) fn start_race(state: &mut State) {
    if state.in_progress.is_some() {
        println!("(race already in progress — type / to finalize first)");
        return;
    }
    let race_no = state.next_race_no;
    state.next_race_no = state.next_race_no.wrapping_add(1);
    state.in_progress = Some(InProgress {
        event: state.current_event,
        heat: state.current_heat,
        race_no,
        started_at: Instant::now(),
        lanes: Default::default(),
    });
    let label = state
        .lineup
        .get(state.current_event as usize - 1)
        .map_or_else(|| format!("event {}", state.current_event), EventDef::label);
    println!(
        "race {race_no} started — {label} heat {heat} (touch lanes 1..8 — each touch is a split, / to finalize)",
        heat = state.current_heat,
    );
    starter_beeps();
}

pub(crate) fn touch_lane(state: &mut State, lane: u8) {
    let mut time_buf = [0u8; 16];
    let (lo, hi) = state.lane_spread;
    if lane < lo || lane > hi {
        println!("(lane {lane} outside active spread {lo}..={hi})");
        return;
    }
    let lane_idx = (lane - 1) as usize;
    if lane_idx >= MAX_LANES {
        println!("(lane {lane} out of range 1..={MAX_LANES})");
        return;
    }
    let total_segments: u16 = state
        .in_progress
        .as_ref()
        .and_then(|ip| state.lineup.get(ip.event as usize - 1))
        .map_or(0, EventDef::total_segments);
    let Some(ip) = state.in_progress.as_mut() else {
        println!("(no race in progress — press <enter> to start)");
        return;
    };
    let elapsed = u32::try_from(
        Instant::now()
            .saturating_duration_since(ip.started_at)
            .as_millis()
            .min(u128::from(u32::MAX)),
    )
    .unwrap_or(0);
    let cum = elapsed.max(1);
    let entry = &mut ip.lanes[lane_idx];
    if entry.finished_at.is_some() {
        println!("  lane {lane} already FINAL — touch ignored");
        return;
    }
    let cum = match entry.touches_ms.last().copied() {
        Some(prev) if cum <= prev => prev.saturating_add(1),
        _ => cum,
    };
    entry.touches_ms.push(cum);
    play_beep();
    let n = u16::try_from(entry.touches_ms.len()).unwrap_or(0);
    if total_segments > 0 && n >= total_segments {
        entry.finished_at = Some(cum);
        println!(
            "  lane {lane} → FINISH @ {} ({}/{} touches)",
            fmt_time(cum, &mut time_buf),
            n,
            total_segments,
        );
    } else {
        println!(
            "  lane {lane} → split {} @ {}",
            n,
            fmt_time(cum, &mut time_buf),
        );
    }
}

pub(crate) fn dq_lane(state: &mut State, lane: u8) {
    let (lo, hi) = state.lane_spread;
    if lane < lo || lane > hi {
        println!("(lane {lane} outside active spread {lo}..={hi})");
        return;
    }
    let lane_idx = (lane - 1) as usize;
    if lane_idx >= MAX_LANES {
        println!("(lane {lane} out of range 1..={MAX_LANES})");
        return;
    }
    let Some(ip) = state.in_progress.as_mut() else {
        println!("(no race in progress)");
        return;
    };
    ip.lanes[lane_idx].dq = true;
    println!("  lane {lane} → DQ");
}

pub(crate) fn finalize_race(state: &mut State) {
    let Some(ip) = state.in_progress.take() else {
        println!("(no race in progress)");
        return;
    };

    let mut rows: [Option<LaneResultRow>; MAX_LANES] = Default::default();
    let mut finishers: Vec<(usize, u32)> = Vec::new();
    let mut splits_max: u8 = 0;
    for (i, lt) in ip.lanes.into_iter().enumerate() {
        let touched = !lt.touches_ms.is_empty();
        if !touched && !lt.dq {
            continue;
        }
        let split_count = u8::try_from(lt.split_prefix().len()).unwrap_or(0);
        if split_count > splits_max {
            splits_max = split_count;
        }
        if touched && !lt.dq {
            finishers.push((i, lt.finish_ms()));
        }
        rows[i] = Some(LaneResultRow { lane: lt, place: 0 });
    }
    finishers.sort_by_key(|&(_, ms)| ms);
    for (place_minus_1, (lane_idx, _)) in finishers.iter().enumerate() {
        if let Some(r) = rows[*lane_idx].as_mut() {
            r.place = u8::try_from(place_minus_1 + 1).unwrap_or(0);
        }
    }

    let lane_count = rows.iter().filter(|r| r.is_some()).count();
    let race = Race {
        event: ip.event,
        heat: ip.heat,
        id: ip.race_no,
        lanes: rows,
        splits_count: splits_max,
    };
    let key = (race.event, race.heat);
    state.races.insert(race.id, race);
    state.history.entry(key).or_default().push(ip.race_no);
    state.cursor.remove(&key);
    println!(
        "race {} finalized: event {} heat {} ({lane_count} lane(s), {splits} interim split(s)) — bumped to next heat",
        ip.race_no,
        ip.event,
        ip.heat,
        splits = splits_max,
    );
    state.current_heat = state.current_heat.saturating_add(1);
}

pub(crate) fn print_slots(state: &State) {
    let sel = state.selected_slot;
    for (i, s) in state.slots.iter().enumerate() {
        let n = i + 1;
        let marker = if sel == Some(n as u8) { " ←" } else { "" };
        let count = s.lineup.len();
        println!(
            "  slot {n}: \"{}\" ({count} event{}){marker}",
            s.name,
            if count == 1 { "" } else { "s" },
        );
    }
    if sel.is_none() {
        println!("(no slot selected — /slot N to load one)");
    } else if state.lineup_dirty {
        println!("(working lineup has unsaved edits — /slot save to commit)");
    }
}

/// Print the lineup stored in slot `n` (1-based) — i.e. the slot's
/// persisted contents, NOT the (possibly edited) working `lineup`.
pub(crate) fn print_slot_lineup(state: &State, n: u8) {
    let idx = (n - 1) as usize;
    let slot = &state.slots[idx];
    println!(
        "slot {n}: \"{}\" ({} event{}){}",
        slot.name,
        slot.lineup.len(),
        if slot.lineup.len() == 1 { "" } else { "s" },
        if state.selected_slot == Some(n) {
            " ← selected"
        } else {
            ""
        },
    );
    if slot.lineup.is_empty() {
        println!("  (empty slot)");
        return;
    }
    for (i, e) in slot.lineup.iter().enumerate() {
        let segs = e.total_segments();
        println!(
            "  event {:>3}: {} (splits/{}yd, {} touch{})",
            i + 1,
            e.label(),
            e.split_yards,
            segs,
            if segs == 1 { "" } else { "es" },
        );
    }
}

fn dirty_hint(state: &State) -> &'static str {
    if state.selected_slot.is_some() && state.lineup_dirty {
        "  [unsaved — /slot save]"
    } else {
        ""
    }
}

fn handle_slot_command(state: &mut State, parts: &mut std::str::SplitWhitespace<'_>) {
    let sub = parts.next().unwrap_or("list");
    match sub {
        "list" => print_slots(state),
        "show" => {
            let target = parts
                .next()
                .and_then(|s| s.parse::<u8>().ok())
                .or(state.selected_slot);
            match target {
                Some(n) if n >= 1 && (n as usize) <= NUM_SLOTS => {
                    print_slot_lineup(state, n);
                }
                Some(n) => println!("slot {n} out of range (1..={NUM_SLOTS})"),
                None => println!("no slot selected — usage: /slot show <1..={NUM_SLOTS}>"),
            }
        }
        "save" => {
            if state.in_progress.is_some() {
                println!("cannot /slot save while a race is in progress (finalize with / first)");
            } else if let Some(n) = state.save_to_selected_slot() {
                let slot = &state.slots[(n - 1) as usize];
                println!(
                    "saved working lineup to slot {n}: \"{}\" ({} events)",
                    slot.name,
                    slot.lineup.len(),
                );
            } else {
                println!("no slot selected — /slot use N first");
            }
        }
        "rename" => {
            let Some(n) = state.selected_slot else {
                println!("no slot selected — /slot use N first");
                return;
            };
            let rest: Vec<&str> = parts.collect();
            if rest.is_empty() {
                println!("usage: /slot rename <new name>");
                return;
            }
            let mut name = rest.join(" ");
            if name.len() > 20 {
                name.truncate(20);
            }
            state.slots[(n - 1) as usize].name = name.clone();
            println!("slot {n} renamed to \"{name}\"");
        }
        "use" | "select" | "load" => {
            let Some(n) = parts.next().and_then(|s| s.parse::<u8>().ok()) else {
                println!("usage: /slot use <1..={NUM_SLOTS}>");
                return;
            };
            slot_use(state, n);
        }
        n_str => {
            if let Ok(n) = n_str.parse::<u8>() {
                if (1..=NUM_SLOTS as u8).contains(&n) {
                    // Bare `/slot N` shows the slot's stored lineup.
                    // Use `/slot use N` to actually load it as the
                    // active working copy.
                    print_slot_lineup(state, n);
                } else {
                    println!("slot {n} out of range (1..={NUM_SLOTS})");
                }
            } else {
                println!(
                    "unknown slot subcommand: {sub} (try /slot | /slot N | /slot use N | /slot save | /slot rename <name>)"
                );
            }
        }
    }
}

fn slot_use(state: &mut State, n: u8) {
    if state.in_progress.is_some() {
        println!("cannot change /slot while a race is in progress (finalize with / first)");
        return;
    }
    if !(1..=NUM_SLOTS as u8).contains(&n) {
        println!("slot {n} out of range (1..={NUM_SLOTS})");
        return;
    }
    if state.lineup_dirty && state.selected_slot.is_some() && state.selected_slot != Some(n) {
        println!(
            "(discarding unsaved working-lineup edits from slot {})",
            state.selected_slot.unwrap()
        );
    }
    state.select_slot(n);
    let slot = &state.slots[(n - 1) as usize];
    println!(
        "selected slot {n}: \"{}\" ({} events)",
        slot.name,
        slot.lineup.len(),
    );
}

pub(crate) fn print_lineup(state: &State) {
    let header = match state.selected_slot {
        Some(n) => {
            let dirty = if state.lineup_dirty {
                " [unsaved edits]"
            } else {
                ""
            };
            format!(
                "working lineup (from slot {n}: \"{}\"){dirty}",
                state.slots[(n - 1) as usize].name
            )
        }
        None => "working lineup (no slot selected)".to_string(),
    };
    println!("{header}");
    if state.lineup.is_empty() {
        println!("  (no events configured)");
        return;
    }
    for (i, e) in state.lineup.iter().enumerate() {
        let marker = if u16::try_from(i + 1).unwrap_or(0) == state.current_event {
            " ←"
        } else {
            ""
        };
        let segs = e.total_segments();
        println!(
            "  event {:>3}: {} (splits/{}yd, {} touch{}){marker}",
            i + 1,
            e.label(),
            e.split_yards,
            segs,
            if segs == 1 { "" } else { "es" },
        );
    }
}

pub(crate) fn print_races(state: &State) {
    if state.races.is_empty() {
        println!("(no races stored)");
        return;
    }
    let mut all: Vec<&Race> = state.races.values().collect();
    all.sort_by_key(|r| r.id);
    for r in all {
        let lanes = r.lanes.iter().filter(|l| l.is_some()).count();
        println!(
            "  race {:>3}: event {} heat {} ({lanes} lanes, {} splits)",
            r.id, r.event, r.heat, r.splits_count,
        );
    }
}

pub(crate) fn handle_command(state: &mut State, line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        start_race(state);
        return true;
    }
    if let Some(cmd) = line.strip_prefix('/') {
        let cmd = cmd.trim();
        if cmd.is_empty() || cmd == "print" {
            finalize_race(state);
            return true;
        }
        let mut parts = cmd.split_whitespace();
        let head = parts.next().unwrap_or("");
        match head {
            "event" => {
                if let Some(n) = parts.next().and_then(|s| s.parse::<u16>().ok()) {
                    state.current_event = n.max(1);
                    println!("event = {}", state.current_event);
                } else {
                    println!("usage: /event N");
                }
            }
            "heat" => {
                if let Some(n) = parts.next().and_then(|s| s.parse::<u8>().ok()) {
                    state.current_heat = n.max(1);
                    println!("heat = {}", state.current_heat);
                } else {
                    println!("usage: /heat N");
                }
            }
            "race" => {
                if state.in_progress.is_some() {
                    println!(
                        "cannot change /race while a race is in progress (finalize with / first)"
                    );
                } else if let Some(n) = parts.next().and_then(|s| s.parse::<u16>().ok()) {
                    state.next_race_no = n.max(1);
                    println!("next_race_no = {}", state.next_race_no);
                } else {
                    println!("usage: /race N");
                }
            }
            "dq" => {
                if let Some(n) = parts.next().and_then(|s| s.parse::<u8>().ok()) {
                    dq_lane(state, n);
                } else {
                    println!("usage: /dq L");
                }
            }
            "lineup" => match parts.next().unwrap_or("show") {
                "show" => print_lineup(state),
                "add" => {
                    let dist = parts.next().and_then(|s| s.parse::<u16>().ok());
                    let gender = parts.next().and_then(parse_gender);
                    let stroke = parts.next().and_then(parse_stroke);
                    let split_yards = parts
                        .next()
                        .and_then(|s| s.parse::<u16>().ok())
                        .unwrap_or(50);
                    if let (Some(d), Some(g), Some(s)) = (dist, gender, stroke) {
                        state.lineup.push(EventDef {
                            distance: d,
                            gender: g,
                            stroke: s,
                            split_yards,
                        });
                        state.lineup_dirty = true;
                        println!(
                            "added event {}: {} (splits every {split_yards} yd){}",
                            state.lineup.len(),
                            state.lineup.last().unwrap().label(),
                            dirty_hint(state),
                        );
                    } else {
                        println!(
                        "usage: /lineup add <distance> <M|F|X> <stroke> [split_yards]\n  stroke: 1/FR  2/BK  3/BR  4/FL  5/IM  6/MED-R  7/FR-R  DV"
                    );
                    }
                }
                "remove" | "rm" | "del" | "delete" => {
                    if let Some(n) = parts.next().and_then(|s| s.parse::<usize>().ok()) {
                        if n == 0 || n > state.lineup.len() {
                            println!(
                                "event {n} out of range (lineup has {} event{})",
                                state.lineup.len(),
                                if state.lineup.len() == 1 { "" } else { "s" },
                            );
                        } else {
                            let removed = state.lineup.remove(n - 1);
                            state.lineup_dirty = true;
                            println!(
                                "removed event {n}: {}{}",
                                removed.label(),
                                dirty_hint(state),
                            );
                        }
                    } else {
                        println!("usage: /lineup remove N  (1-based event index)");
                    }
                }
                "clear" => {
                    state.lineup.clear();
                    state.lineup_dirty = true;
                    println!("lineup cleared{}", dirty_hint(state));
                }
                other => println!("unknown lineup subcommand: {other}"),
            },
            "slot" | "slots" => handle_slot_command(state, &mut parts),
            "races" => print_races(state),
            "lanes" => match parts.next() {
                None => {
                    let (lo, hi) = state.lane_spread;
                    println!("lane spread = {lo}..{hi}");
                }
                Some(spec) => {
                    if let Some((lo, hi)) = parse_lane_spread(spec) {
                        if state.in_progress.is_some() {
                            println!("cannot change /lanes while a race is in progress (finalize with / first)");
                        } else {
                            state.lane_spread = (lo, hi);
                            println!("lane spread = {lo}..{hi}");
                        }
                    } else {
                        println!(
                    "usage: /lanes A..B  (1..={MAX_LANES}, e.g. /lanes 1..10, /lanes 2..8, /lanes 1..1)"
                );
                    }
                }
            },
            "status" => {
                let (lo, hi) = state.lane_spread;
                let slot_str = match state.selected_slot {
                    Some(n) => format!("{n} \"{}\"", state.slots[(n - 1) as usize].name),
                    None => "(none)".to_string(),
                };
                println!(
                    "event={} heat={} next_race_no={} slot={slot_str} lanes={lo}..{hi} in_progress={} stored={}",
                    state.current_event,
                    state.current_heat,
                    state.next_race_no,
                    state.in_progress.is_some(),
                    state.races.len(),
                );
            }
            "help" | "?" => print_help(),
            "quit" | "exit" => return false,
            other => println!("unknown command: /{other} (try /help)"),
        }
        return true;
    }
    let mut any = false;
    for tok in line.split_whitespace() {
        if let Ok(n) = tok.parse::<u8>() {
            touch_lane(state, n);
            any = true;
        } else {
            println!("unknown input: {tok}");
        }
    }
    if !any {
        println!("(nothing to do — try /help)");
    }
    true
}

pub(crate) fn print_help() {
    println!("{HELP_TEXT}");
}

/// Help text shared between the legacy `println!` path (e.g. when
/// the TUI isn't running) and the in-TUI modal popup.
pub(crate) const HELP_TEXT: &str = "cts6 emulator commands:\n\
     \n\
     meet setup:\n\
       /event N                  set active event\n\
       /heat  N                  set active heat\n\
       /race  N                  set next race number (resume mid-meet)\n\
       /lanes A..B               set active lane spread (1..=10,\n\
                                   e.g. /lanes 1..10, 2..8, 1..1)\n\
       /slot                     open sequence-memory popup (TUI):\n\
                                   ↑↓ navigate, Enter to view a slot's\n\
                                   lineup, then Enter to load it,\n\
                                   Esc/Backspace to go back\n\
       /slot N                   show slot N's stored event lineup\n\
       /slot use N               load slot N's lineup into the working\n\
                                   copy (1..=8) for editing/use\n\
       /slot save                save the working lineup back to the\n\
                                   selected slot's memory\n\
       /slot rename <name>       rename the selected slot (≤20 chars)\n\
       /lineup show              list the working-copy events\n\
       /lineup add D G S [Y]     add event to working copy\n\
                                   (distance, M|F|X, stroke 1/FR 2/BK\n\
                                   3/BR 4/FL 5/IM 6/MED-R 7/FR-R DV,\n\
                                   optional split-yds)\n\
       /lineup remove N          delete event N from working copy\n\
       /lineup clear             empty the working-copy events\n\
     \n\
     running a race:\n\
       <enter>                   start the race (timestamp 0.000)\n\
       N (in spread)             touch lane N — each touch is a split\n\
                                   (1st = split 1, 2nd = split 2, …)\n\
       1 3 5                     batch — multiple touches at once\n\
       /dq L                     mark lane L as DQ\n\
       / | /print                finalize: last touch per lane = finish,\n\
                                   earlier touches = cumulative splits;\n\
                                   places assigned by ascending finish.\n\
                                   Heat auto-bumps by 1.\n\
     \n\
     inspection:\n\
       /races                    list stored races\n\
       /status                   show current state\n\
       /help                     this message (popup; Esc/Enter to close)\n\
       /quit                     exit\n\
     \n\
     stored events tree (right pane):\n\
       Tab                       focus the tree (Tab/Esc returns to input)\n\
       ↑ ↓ Home End              navigate rows\n\
       → / Enter                 expand event/heat\n\
       ←                         collapse (or jump to parent on a race)";
