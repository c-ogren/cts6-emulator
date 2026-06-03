//! Emulator core domain types and in-memory race history.
//!
//! `State` owns the entire run-time model: lineup, in-progress race,
//! finalized races, navigation cursors, and the buffered meet-download
//! accumulator. The TCP server (`crate::handle_client`), TUI, and REPL
//! all share a single `Arc<Mutex<State>>`. Methods that mutate the
//! navigation cursor (`lookup_*`) take `&mut self`.

use std::collections::HashMap;
use std::time::Instant;

use crate::emu_log;

/// Maximum lane capacity tracked internally. Real CTS6 timers come in
/// 8- and 10-lane variants; we provision the larger one and let the
/// operator dial the *active* range via `/lanes A..B`. The on-wire
/// event response remains an 8-lane frame (the wire spec pins it at
/// 8), so lanes 9 and 10 are display-only.
pub(crate) const MAX_LANES: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Gender {
    Male,
    Female,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stroke {
    Free,
    Back,
    Breast,
    Fly,
    Im,
    MedleyRelay,
    FreestyleRelay,
    Diving,
}

impl Stroke {
    /// Short text code matching the CTS6 stroke table. Relay codes
    /// are suffixed with `-R` so that `FR` (freestyle, code 1) is
    /// never ambiguous with `FR-R` (free relay, code 7).
    pub(crate) fn code(self) -> &'static str {
        match self {
            Stroke::Free => "FR",
            Stroke::Back => "BK",
            Stroke::Breast => "BR",
            Stroke::Fly => "FL",
            Stroke::Im => "IM",
            Stroke::MedleyRelay => "MED-R",
            Stroke::FreestyleRelay => "FR-R",
            Stroke::Diving => "DV",
        }
    }

    /// Numeric stroke code as it appears on the wire (1–7). Diving
    /// has no assigned number.
    #[allow(dead_code)]
    pub(crate) fn numeric(self) -> Option<u8> {
        Some(match self {
            Stroke::Free => 1,
            Stroke::Back => 2,
            Stroke::Breast => 3,
            Stroke::Fly => 4,
            Stroke::Im => 5,
            Stroke::MedleyRelay => 6,
            Stroke::FreestyleRelay => 7,
            Stroke::Diving => return None,
        })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct EventDef {
    pub(crate) distance: u16,
    pub(crate) gender: Gender,
    pub(crate) stroke: Stroke,
    /// Yards per split segment (typically 50; some meets use 25 for
    /// short-course / age-group races). Used purely to compute the
    /// total expected touch count per lane and to drive the scoreboard
    /// SPLIT/FINISH ARMED indicator. Ignored for diving.
    pub(crate) split_yards: u16,
}

impl EventDef {
    pub(crate) fn label(&self) -> String {
        if matches!(self.stroke, Stroke::Diving) {
            return "diving".to_string();
        }
        let g = match self.gender {
            Gender::Male => "M",
            Gender::Female => "F",
            Gender::Mixed => "X",
        };
        format!("{}{} {}", self.distance, g, self.stroke.code())
    }

    /// Number of touchpad presses expected per lane to complete the
    /// race (final touch = finish, earlier = cumulative splits).
    /// Diving and zero-distance events collapse to a single "touch".
    pub(crate) fn total_segments(&self) -> u16 {
        if matches!(self.stroke, Stroke::Diving) || self.distance == 0 || self.split_yards == 0 {
            return 1;
        }
        (self.distance / self.split_yards).max(1)
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct LaneTime {
    /// Cumulative touchpad times in milliseconds (race-relative), in
    /// touch order. The LAST entry is treated as the lane's finish
    /// (`primary_ms`); earlier entries are interim splits. May be
    /// empty if the lane was only DQ'd without any touches.
    pub(crate) touches_ms: Vec<u32>,
    /// True iff operator typed `/dq N` for this lane.
    pub(crate) dq: bool,
    /// Race-relative ms when this lane hit its final expected touch.
    /// Once set, further `/<lane>` touches are ignored (mirrors a real
    /// CTS6 lane disarming itself after the finish) and the scoreboard
    /// shows a FINAL marker that blinks 3 times then goes steady.
    pub(crate) finished_at: Option<u32>,
}

impl LaneTime {
    pub(crate) fn finish_ms(&self) -> u32 {
        self.touches_ms.last().copied().unwrap_or(0)
    }
    /// All cumulative splits BEFORE the final touch. Empty if 0 or 1
    /// touches were recorded.
    pub(crate) fn split_prefix(&self) -> &[u32] {
        if self.touches_ms.len() <= 1 {
            &[]
        } else {
            &self.touches_ms[..self.touches_ms.len() - 1]
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LaneResultRow {
    pub(crate) lane: LaneTime,
    /// 1..=N finishing place, 0 if no touches were recorded.
    pub(crate) place: u8,
}

#[derive(Debug, Clone)]
pub(crate) struct Race {
    pub(crate) event: u16,
    pub(crate) heat: u8,
    pub(crate) id: u16,
    pub(crate) lanes: [Option<LaneResultRow>; MAX_LANES],
    /// Number of cumulative split slots emitted in the on-wire frame.
    /// Equals `max(splits_per_lane)` across all lanes — zero-padded for
    /// lanes that touched fewer times.
    pub(crate) splits_count: u8,
}

#[derive(Debug)]
pub(crate) struct InProgress {
    pub(crate) event: u16,
    pub(crate) heat: u8,
    pub(crate) race_no: u16,
    pub(crate) started_at: Instant,
    /// Per-lane touches in arrival order. Each push records the
    /// elapsed-since-start in ms. Final entry per lane becomes the
    /// finish; earlier entries become interim splits. Indexed `0..MAX_LANES` but
    /// the active subset is constrained by `State::lane_spread`.
    pub(crate) lanes: [LaneTime; MAX_LANES],
}

// ─── State ─────────────────────────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct State {
    pub(crate) current_event: u16,
    pub(crate) current_heat: u8,
    /// Monotonically increasing race counter (mirrors CTS6 behaviour).
    pub(crate) next_race_no: u16,
    /// Configured event lineup. Index 0 = event 1.
    pub(crate) lineup: Vec<EventDef>,
    /// Currently-armed (mid-touch) race, if any.
    pub(crate) in_progress: Option<InProgress>,
    /// All finalized races, keyed by `race_no`.
    pub(crate) races: HashMap<u16, Race>,
    /// `(event, heat) -> race_no list` ordered oldest → newest.
    /// Used to seed SSBIE (latest) and walk SSBIL/SSBIN cursors.
    pub(crate) history: HashMap<(u16, u8), Vec<u16>>,
    /// Per (event, heat) navigation cursor. Tri-state so that the
    /// "off the end / off the start" position the real timer pauses at
    /// (responding with the 6-byte no-data frame) is itself navigable
    /// — the next press wraps, the back press returns to the boundary
    /// race. See [`Cursor`].
    pub(crate) cursor: HashMap<(u16, u8), Cursor>,
    /// Inclusive (low, high) lane range currently armed in the pool.
    /// Touch input outside this range is rejected; scoreboard only
    /// renders rows within it. Bounds are clamped to `1..=MAX_LANES`.
    pub(crate) lane_spread: (u8, u8),
    /// In-flight meet download (Is + It×N + Iu×N + Ir). Populated
    /// when an `Is` frame arrives and committed to `lineup` when the
    /// closing `Ir` finalize-slot frame is received.
    pub(crate) loading: Option<LoadingMeet>,
}

/// Accumulator for an in-progress meet download. The chronaris-mm
/// driver (see `src-tauri/src/timer/transport.rs` `drive_meet_download`)
/// streams the meet as: one `Is` (meet name), four sets of `It` label
/// frames (gender/age/stroke/round), a 500-frame `Iu` clear pass, the
/// populated `Iu` event records, then a single `Ir` finalize. We
/// buffer category labels + event records here and translate them into
/// the emulator's [`EventDef`] vec on finalize.
#[derive(Debug, Default, Clone)]
pub(crate) struct LoadingMeet {
    pub(crate) slot: u8,
    pub(crate) name: String,
    pub(crate) genders: Vec<String>,
    pub(crate) ages: Vec<String>,
    pub(crate) strokes: Vec<String>,
    pub(crate) rounds: Vec<String>,
    /// Populated `Iu` event records, in arrival order. Clear-pass
    /// frames (all-zero payload) are filtered out.
    pub(crate) events: Vec<LoadedEvent>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct LoadedEvent {
    pub(crate) event_num: u16,
    pub(crate) distance: u16,
    /// Raw `payload[3] - 1` from the Iu frame (0-based gender index).
    pub(crate) gender_idx: u8,
    /// Raw `payload[4] - 0x0A` from the Iu frame (0-based stroke index).
    pub(crate) stroke_idx: u8,
    pub(crate) is_relay: bool,
}

/// Navigation position within a `(event, heat)` history bucket.
///
/// Lifecycle (assume N races stored, indices 0..N-1):
///
/// * `SSBIE` (latest)  → `At(N-1)`, returns races[N-1].
/// * `SSBIN` while `At(i)`:
///     * `i+1 < N`   → `At(i+1)`, returns that race.
///     * `i+1 == N`  → `PastEnd`, returns nothing (no-data frame).
/// * `SSBIN` while `PastEnd`  → `At(0)`, returns races[0] (wraps).
/// * `SSBIN` while `PastStart` → `At(0)`, returns races[0].
/// * `SSBIL` while `At(i)`:
///     * `i > 0`     → `At(i-1)`, returns that race.
///     * `i == 0`    → `PastStart`, returns nothing.
/// * `SSBIL` while `PastStart` → `At(N-1)`, returns races[N-1] (wraps).
/// * `SSBIL` while `PastEnd`   → `At(N-1)`, returns races[N-1].
#[derive(Debug, Clone, Copy)]
pub(crate) enum Cursor {
    At(usize),
    PastEnd,
    PastStart,
}

impl State {
    pub(crate) fn new() -> Self {
        Self {
            current_event: 1,
            current_heat: 1,
            next_race_no: 1,
            lineup: Vec::new(),
            in_progress: None,
            races: HashMap::new(),
            history: HashMap::new(),
            cursor: HashMap::new(),
            lane_spread: (1, 8),
            loading: None,
        }
    }

    /// Commit the buffered [`LoadingMeet`] (built up from `Is`/`It`/`Iu`
    /// frames) into the active event lineup. Called when the closing
    /// `Ir` finalize-slot frame arrives.
    ///
    /// Sparse `event_num`s are padded with a placeholder `EventDef` so
    /// `self.lineup[event_num - 1]` lookups remain valid. Strokes and
    /// genders are resolved by string-matching the label tables sent by
    /// the client (the wire only carries indices); if the tables are
    /// missing or unrecognized, we fall back to `Free` / `Mixed`.
    pub(crate) fn apply_loading_meet(&mut self) {
        let Some(loading) = self.loading.take() else {
            return;
        };
        if loading.events.is_empty() {
            emu_log!(
                "[meet-load] slot {} \"{}\": finalize with no events; lineup left untouched",
                loading.slot,
                loading.name
            );
            return;
        }
        let max_evt = loading
            .events
            .iter()
            .map(|e| e.event_num)
            .max()
            .unwrap_or(0);
        let placeholder = EventDef {
            distance: 0,
            gender: Gender::Mixed,
            stroke: Stroke::Free,
            split_yards: 50,
        };
        let mut lineup = vec![placeholder.clone(); max_evt as usize];
        for e in &loading.events {
            if e.event_num == 0 {
                continue;
            }
            let gender = loading
                .genders
                .get(e.gender_idx as usize)
                .map(|s| label_to_gender(s))
                .unwrap_or(Gender::Mixed);
            let stroke = label_to_stroke(
                loading
                    .strokes
                    .get(e.stroke_idx as usize)
                    .map(String::as_str)
                    .unwrap_or(""),
                e.is_relay,
            );
            lineup[(e.event_num - 1) as usize] = EventDef {
                distance: e.distance,
                gender,
                stroke,
                split_yards: 50,
            };
        }
        emu_log!(
            "[meet-load] slot {} \"{}\": committed {} events (lineup size {})",
            loading.slot,
            loading.name,
            loading.events.len(),
            lineup.len()
        );
        self.lineup = lineup;
    }

    pub(crate) fn lookup_latest(&mut self, event: u16, heat: u8) -> Option<&Race> {
        let races = self.history.get(&(event, heat))?.clone();
        let last = *races.last()?;
        // Seed cursor at the newest entry so subsequent SSBIL walks
        // start from "one before latest".
        self.cursor
            .insert((event, heat), Cursor::At(races.len() - 1));
        self.races.get(&last)
    }

    pub(crate) fn lookup_previous(&mut self, event: u16, heat: u8) -> Option<&Race> {
        let races = self.history.get(&(event, heat))?.clone();
        if races.is_empty() {
            return None;
        }
        let cur = self.cursor.get(&(event, heat)).copied()?;
        let last_idx = races.len() - 1;
        let (next_cur, picked) = match cur {
            Cursor::At(0) => (Cursor::PastStart, None),
            Cursor::At(i) => (Cursor::At(i - 1), Some(i - 1)),
            // Coming back from the "off-the-end" sentinel returns to
            // the most recent race the user was just looking at.
            Cursor::PastEnd | Cursor::PastStart => (Cursor::At(last_idx), Some(last_idx)),
        };
        self.cursor.insert((event, heat), next_cur);
        picked.and_then(|i| self.races.get(&races[i]))
    }

    pub(crate) fn lookup_next(&mut self, event: u16, heat: u8) -> Option<&Race> {
        let races = self.history.get(&(event, heat))?.clone();
        if races.is_empty() {
            return None;
        }
        let cur = self.cursor.get(&(event, heat)).copied()?;
        let last_idx = races.len() - 1;
        let (next_cur, picked) = match cur {
            Cursor::At(i) if i >= last_idx => (Cursor::PastEnd, None),
            Cursor::At(i) => (Cursor::At(i + 1), Some(i + 1)),
            // Wrap: next-of-(off-the-end) loops back to the oldest.
            Cursor::PastEnd | Cursor::PastStart => (Cursor::At(0), Some(0)),
        };
        self.cursor.insert((event, heat), next_cur);
        picked.and_then(|i| self.races.get(&races[i]))
    }

    pub(crate) fn lookup_by_race(&self, race_no: u16) -> Option<&Race> {
        self.races.get(&race_no)
    }
}

/// Map a CTS6 gender-table label ("Mens", "Womens", "Boys", "Girls",
/// …) onto the emulator's [`Gender`] enum. Falls back to [`Gender::Mixed`].
pub(crate) fn label_to_gender(label: &str) -> Gender {
    let l = label.trim().to_ascii_lowercase();
    if l.starts_with('w') || l.starts_with('g') || l.starts_with('f') {
        Gender::Female
    } else if l.starts_with('m') || l.starts_with('b') {
        Gender::Male
    } else {
        Gender::Mixed
    }
}

/// Map a CTS6 stroke-table label ("Freestyle", "Backstroke", "Medley",
/// "Relay", …) onto [`Stroke`]. `is_relay` is taken from the Iu frame's
/// relay marker (`payload[5] == 0x10`) and biases the result toward a
/// `MedleyRelay` / `FreestyleRelay` variant.
pub(crate) fn label_to_stroke(label: &str, is_relay: bool) -> Stroke {
    let l = label.trim().to_ascii_lowercase();
    if is_relay {
        if l.contains("medley") {
            return Stroke::MedleyRelay;
        }
        return Stroke::FreestyleRelay;
    }
    if l.contains("fly") || l.contains("butter") {
        Stroke::Fly
    } else if l.contains("back") {
        Stroke::Back
    } else if l.contains("breast") {
        Stroke::Breast
    } else if l.contains("im") || l.contains("medley") {
        Stroke::Im
    } else if l.contains("div") {
        Stroke::Diving
    } else {
        Stroke::Free
    }
}
