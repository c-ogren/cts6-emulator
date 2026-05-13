//! Colorado Timing System emulator — in-memory, stdin-driven.
//!
//! Listens on `127.0.0.1:1337` (override with `--addr`) and speaks
//! the subset of the CTS6 wire protocol exercised by typical meet-
//! management clients (identify, commit, comm-test, slot list,
//! SSBIE/SSBIL/SSBIN/SSBIR event-result fetches). State lives only
//! in process memory and is wiped on restart — a real System 6
//! retains everything in NVRAM. Persistence (sqlite or similar) is
//! a future TODO; in-memory only is the current implementation, not
//! a design choice.
//!
//! Stdin REPL drives the meet:
//!
//!   /event N        select active event (1..=65535)
//!   /heat  N        select active heat  (1..=255)
//!   /lineup show    print configured event lineup
//!   /lineup add D G S
//!                   append event: distance(yd) gender(M/F/X) stroke(FR/BK/BR/FL/IM/MR/FR/DV)
//!   /dq L           mark lane L as DQ for the in-progress race
//!   /print  | /     finalize the in-progress race ("store/print")
//!   /races          list stored races
//!   /quit
//!
//! While a race is in progress:
//!   <enter>         start the race (timestamp 0.000)
//!   1..8            touchpad on lane N (records elapsed time, assigns place)
//!   1 3 5           batch — three touches in input order
//!   /               finalize and lock; bump heat by 1 for next race
//!
//! Frame format crib (full spec lives in this repo's docs/):
//!   identify req:  05 00 57 A3 FF
//!   identify rsp:  0A 00 31 2E 32 33 32 00 FF FE        ("1.232")
//!   commit  req:   06 00 52 72 35 FF
//!   commit  rsp:   05 00 07 F3 FF
//!   comm-test req: 05 00 57 8D FF
//!   comm-test rsp: 06 00 32 01 3B EB
//!   SSBIE  short:  0B 00 53 53 42 49 45 <heat:u8> <event:u8> <chk> FE
//!   SSBIE  long:   0D 00 53 53 42 49 45 <heat:le16> <event:le16> <chk> FD
//!   event response: 0x05 short frame, 219 B (49 hdr + 8×21 lanes + 2 trailer).

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;
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

/// Channel into the dedicated log-printer thread. Initialized in
/// `main()`; until then `emu_log!` falls back to plain stderr so
/// early-startup messages still surface. Once the TUI is up the
/// receiver lives in the main loop and drains lines into the log
/// pane each frame.
static LOG_TX: OnceLock<mpsc::Sender<String>> = OnceLock::new();

macro_rules! emu_log {
    ($($arg:tt)*) => {{
        let __line = format!($($arg)*);
        if let Some(tx) = $crate::LOG_TX.get() {
            let _ = tx.send(__line);
        } else {
            eprintln!("{}", __line);
        }
    }};
}

/// Shadow `println!` (and `print!`) inside this crate so the existing
/// command-handler code that writes to stdout transparently routes
/// into the TUI log pane instead of corrupting the alt-screen render.
/// Anywhere we genuinely want stdout (after TUI shutdown), use the
/// fully qualified `std::println!` / `std::print!`.
#[allow(unused_macros)]
macro_rules! println {
    ($($arg:tt)*) => { emu_log!($($arg)*) };
}
#[allow(unused_macros)]
macro_rules! print {
    ($($arg:tt)*) => { emu_log!($($arg)*) };
}

const FIRMWARE: &str = "1.232";
const DEFAULT_ADDR: &str = "127.0.0.1:1337";
const YEAR: u16 = 2026;

/// Maximum lane capacity tracked internally. Real CTS6 timers come in
/// 8- and 10-lane variants; we provision the larger one and let the
/// operator dial the *active* range via `/lanes A..B`. The on-wire
/// event response remains an 8-lane frame (the wire spec pins it at
/// 8), so lanes 9 and 10 are display-only.
const MAX_LANES: usize = 10;
/// Number of lanes serialized into the SSBIE event-result frame.
/// Pinned to 8 by the wire spec — do not change without also fixing
/// every consumer's lane-table parser.
const WIRE_LANES: usize = 8;

// ─── Domain ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gender {
    Male,
    Female,
    Mixed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stroke {
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
    fn code(self) -> &'static str {
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
    fn numeric(self) -> Option<u8> {
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
struct EventDef {
    distance: u16,
    gender: Gender,
    stroke: Stroke,
    /// Yards per split segment (typically 50; some meets use 25 for
    /// short-course / age-group races). Used purely to compute the
    /// total expected touch count per lane and to drive the scoreboard
    /// SPLIT/FINISH ARMED indicator. Ignored for diving.
    split_yards: u16,
}

impl EventDef {
    fn label(&self) -> String {
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
    fn total_segments(&self) -> u16 {
        if matches!(self.stroke, Stroke::Diving) || self.distance == 0 || self.split_yards == 0 {
            return 1;
        }
        (self.distance / self.split_yards).max(1)
    }
}

#[derive(Debug, Clone, Default)]
struct LaneTime {
    /// Cumulative touchpad times in milliseconds (race-relative), in
    /// touch order. The LAST entry is treated as the lane's finish
    /// (`primary_ms`); earlier entries are interim splits. May be
    /// empty if the lane was only DQ'd without any touches.
    touches_ms: Vec<u32>,
    /// True iff operator typed `/dq N` for this lane.
    dq: bool,
    /// Race-relative ms when this lane hit its final expected touch.
    /// Once set, further `/<lane>` touches are ignored (mirrors a real
    /// CTS6 lane disarming itself after the finish) and the scoreboard
    /// shows a FINAL marker that blinks 3 times then goes steady.
    finished_at: Option<u32>,
}

impl LaneTime {
    fn finish_ms(&self) -> u32 {
        self.touches_ms.last().copied().unwrap_or(0)
    }
    /// All cumulative splits BEFORE the final touch. Empty if 0 or 1
    /// touches were recorded.
    fn split_prefix(&self) -> &[u32] {
        if self.touches_ms.len() <= 1 {
            &[]
        } else {
            &self.touches_ms[..self.touches_ms.len() - 1]
        }
    }
}

#[derive(Debug, Clone)]
struct LaneResultRow {
    lane: LaneTime,
    /// 1..=N finishing place, 0 if no touches were recorded.
    place: u8,
}

#[derive(Debug, Clone)]
struct Race {
    event: u16,
    heat: u8,
    race_no: u16,
    lanes: [Option<LaneResultRow>; MAX_LANES],
    /// Number of cumulative split slots emitted in the on-wire frame.
    /// Equals max(splits_per_lane) across all lanes — zero-padded for
    /// lanes that touched fewer times.
    splits_count: u8,
}

#[derive(Debug)]
struct InProgress {
    event: u16,
    heat: u8,
    race_no: u16,
    started_at: Instant,
    /// Per-lane touches in arrival order. Each push records the
    /// elapsed-since-start in ms. Final entry per lane becomes the
    /// finish; earlier entries become interim splits. Indexed 0..MAX_LANES;
    /// the active subset is constrained by `State::lane_spread`.
    lanes: [LaneTime; MAX_LANES],
}

// ─── State ─────────────────────────────────────────────────────────────

#[derive(Debug)]
struct State {
    current_event: u16,
    current_heat: u8,
    /// Monotonically increasing race counter (mirrors CTS6 behaviour).
    next_race_no: u16,
    /// Configured event lineup. Index 0 = event 1.
    lineup: Vec<EventDef>,
    /// Currently-armed (mid-touch) race, if any.
    in_progress: Option<InProgress>,
    /// All finalized races, keyed by race_no.
    races: HashMap<u16, Race>,
    /// `(event, heat) -> race_no list` ordered oldest → newest.
    /// Used to seed SSBIE (latest) and walk SSBIL/SSBIN cursors.
    history: HashMap<(u16, u8), Vec<u16>>,
    /// Per (event, heat) navigation cursor. Tri-state so that the
    /// "off the end / off the start" position the real timer pauses at
    /// (responding with the 6-byte no-data frame) is itself navigable
    /// — the next press wraps, the back press returns to the boundary
    /// race. See [`Cursor`].
    cursor: HashMap<(u16, u8), Cursor>,
    /// Inclusive (low, high) lane range currently armed in the pool.
    /// Touch input outside this range is rejected; scoreboard only
    /// renders rows within it. Bounds are clamped to `1..=MAX_LANES`.
    lane_spread: (u8, u8),
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
enum Cursor {
    At(usize),
    PastEnd,
    PastStart,
}

impl State {
    fn new() -> Self {
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
        }
    }

    fn lookup_latest(&mut self, event: u16, heat: u8) -> Option<&Race> {
        let races = self.history.get(&(event, heat))?.clone();
        let last = *races.last()?;
        // Seed cursor at the newest entry so subsequent SSBIL walks
        // start from "one before latest".
        self.cursor
            .insert((event, heat), Cursor::At(races.len() - 1));
        self.races.get(&last)
    }

    fn lookup_previous(&mut self, event: u16, heat: u8) -> Option<&Race> {
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
            Cursor::PastEnd => (Cursor::At(last_idx), Some(last_idx)),
            // Wrap: prev-of-(off-the-start) loops to the newest race.
            Cursor::PastStart => (Cursor::At(last_idx), Some(last_idx)),
        };
        self.cursor.insert((event, heat), next_cur);
        picked.and_then(|i| self.races.get(&races[i]))
    }

    fn lookup_next(&mut self, event: u16, heat: u8) -> Option<&Race> {
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
            Cursor::PastEnd => (Cursor::At(0), Some(0)),
            // Coming back from "off-the-start" returns to the oldest
            // race (the boundary the user was just at).
            Cursor::PastStart => (Cursor::At(0), Some(0)),
        };
        self.cursor.insert((event, heat), next_cur);
        picked.and_then(|i| self.races.get(&races[i]))
    }

    fn lookup_by_race(&self, race_no: u16) -> Option<&Race> {
        self.races.get(&race_no)
    }
}

// ─── Frame helpers ─────────────────────────────────────────────────────

fn cts6_chk(bytes: &[u8]) -> u8 {
    let s: u32 = bytes.iter().map(|&b| b as u32).sum();
    (0xFFu32.wrapping_sub(s) & 0xFF) as u8
}

/// Build the SSBIE event-result response.
///
/// Two output shapes, picked by `race.splits_count`:
///
/// * `splits_count == 0` → 0x05 short frame (49 hdr + 8×21 lanes + 2
///   trailer = 219 B, no split slots). Matches
///   `parse_lane_table_short`. We leave `buf[16] = 0` so the long
///   dispatcher rejects it.
/// * `splits_count >= 1` → long-with-splits frame (49 hdr + 8×stride
///   + 2 trailer where stride = 21 + splits*4). `buf[16]` is set to
///   8 so the parser's `is_long_with_splits_frame` predicate accepts
///   the layout. Per-lane block layout matches
///   `parse_lane_table_long_with_splits`:
///       0:                place (or 0xFF for DQ)
///       1 .. 1+s*4:       splits[0..s] cumulative LE u32
///       1+s*4..+4:        primary (final touch) LE u32
///       +4..+4:           backup1 LE u32
///       +8 zeroes:        pad
///       +16..+4:          backup2 LE u32
fn build_event_response(race: &Race) -> Vec<u8> {
    let splits = race.splits_count as usize;
    let stride = 21 + splits * 4;
    let lane_count = WIRE_LANES;
    let total = 49 + lane_count * stride + 2;
    let mut buf = vec![0u8; total];
    buf[0] = 0x05; // class
    buf[1] = 0x01; // sub-address
    buf[2] = (race.event & 0xFF) as u8; // echo low byte (short-frame path)
    buf[3] = race.heat;
    // race counter at bytes 6-7 LE u16 (parser reads it from here).
    buf[6] = (race.race_no & 0xFF) as u8;
    buf[7] = (race.race_no >> 8) as u8;
    // Lane-count slot at byte 16: required by long-with-splits
    // dispatcher; harmless for short frames (parser ignores it there).
    if splits > 0 {
        buf[16] = lane_count as u8;
    }
    // Year LE u16 at offsets 40-41.
    buf[40] = (YEAR & 0xFF) as u8;
    buf[41] = (YEAR >> 8) as u8;
    // Event LE u16 at 44-45 (mirror — wide-frame path uses this).
    buf[44] = (race.event & 0xFF) as u8;
    buf[45] = (race.event >> 8) as u8;
    // Heat LE u16 at 46-47.
    buf[46] = race.heat;
    // Race LE u16 at 48-49 (legacy slot — also mirrored here).
    buf[48] = (race.race_no & 0xFF) as u8;

    for i in 0..lane_count {
        let base = 49 + i * stride;
        let row = match &race.lanes[i] {
            Some(r) => r,
            None => continue, // already zeroed
        };
        let l = &row.lane;
        buf[base] = if l.dq { 0xFF } else { row.place };
        // Splits region: write up to `splits` cumulative values; zero
        // the rest if this lane finished with fewer touches.
        let lane_splits = l.split_prefix();
        for s in 0..splits {
            let off = base + 1 + s * 4;
            let v = lane_splits.get(s).copied().unwrap_or(0);
            buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
        }
        let primary = l.finish_ms();
        let primary_off = base + 1 + splits * 4;
        buf[primary_off..primary_off + 4].copy_from_slice(&primary.to_le_bytes());
        // Synthesise plausible backup buttons ± a few ms.
        let backup1 = primary.saturating_add(3);
        let backup2 = primary
            .saturating_sub(2)
            .max(if primary > 0 { 1 } else { 0 });
        let b1_off = primary_off + 4;
        buf[b1_off..b1_off + 4].copy_from_slice(&backup1.to_le_bytes());
        // 8 pad bytes already zero.
        let b2_off = primary_off + 16;
        buf[b2_off..b2_off + 4].copy_from_slice(&backup2.to_le_bytes());
    }
    // Trailer — parser doesn't validate, just terminate sensibly.
    buf[total - 2] = 0x00;
    buf[total - 1] = 0xFA;
    buf
}

#[derive(Debug, Clone, Copy)]
enum Verb {
    Latest,   // 0x45 'E' — SSBIE
    Previous, // 0x4C 'L' — SSBIL
    Next,     // 0x4E 'N' — SSBIN
    ByRace,   // 0x52 'R' — SSBIR
}

#[derive(Debug, Clone)]
enum Request {
    Identify,
    Commit,
    CommTest,
    /// `Ws` — slot listing prefix; we just NACK.
    Slot(u8),
    /// `Ms` / `Ts` / `T` — meet status. We return a minimal 51-byte
    /// frame that satisfies `parse_selected_meet_reply`.
    MeetStatus,
    /// SSBIE/SSBIL/SSBIN: keyed by (event, heat).
    Fetch {
        verb: Verb,
        heat: u8,
        event: u16,
    },
    /// SSBIR: keyed by absolute race counter.
    FetchByRace {
        race: u16,
    },
    Unknown(Vec<u8>),
}

fn parse_request(buf: &[u8]) -> Request {
    // Fixed shapes first.
    if buf == [0x05, 0x00, 0x57, 0xA3, 0xFF] {
        return Request::Identify;
    }
    if buf == [0x05, 0x00, 0x57, 0x8D, 0xFF] {
        return Request::CommTest;
    }
    if buf == [0x06, 0x00, 0x52, 0x72, 0x35, 0xFF] {
        return Request::Commit;
    }
    if buf == [0x05, 0x00, 0x54, 0xA6, 0xFF] {
        // Bare `T` — meet selector enter.
        return Request::MeetStatus;
    }
    // 7-byte slot query: 07 00 52 73 <slot> <chk> FF
    if buf.len() == 7 && buf[0] == 0x07 && &buf[1..4] == [0x00, 0x52, 0x73] && buf[6] == 0xFF {
        return Request::Slot(buf[4]);
    }
    // SSBI{E,L,N,R} short: 11 bytes, ends 0xFE.
    if buf.len() == 11
        && buf[0] == 0x0B
        && buf[2] == 0x53
        && buf[3] == 0x53
        && buf[4] == 0x42
        && buf[5] == 0x49
        && buf[10] == 0xFE
    {
        let verb = match buf[6] {
            0x45 => Some(Verb::Latest),
            0x4C => Some(Verb::Previous),
            0x4E => Some(Verb::Next),
            0x52 => Some(Verb::ByRace),
            _ => None,
        };
        if let Some(v) = verb {
            if matches!(v, Verb::ByRace) {
                // SSBIR layout C: race in heat slot, event byte = 0.
                return Request::FetchByRace {
                    race: buf[7] as u16,
                };
            }
            return Request::Fetch {
                verb: v,
                heat: buf[7],
                event: buf[8] as u16,
            };
        }
    }
    // SSBI{E,L,N} long: 13 bytes, ends 0xFD.
    if buf.len() == 13
        && buf[0] == 0x0D
        && buf[2] == 0x53
        && buf[3] == 0x53
        && buf[4] == 0x42
        && buf[5] == 0x49
        && buf[12] == 0xFD
    {
        let verb = match buf[6] {
            0x45 => Some(Verb::Latest),
            0x4C => Some(Verb::Previous),
            0x4E => Some(Verb::Next),
            _ => None,
        };
        if let Some(v) = verb {
            let heat = buf[7]; // ignore high byte — protocol caps at u8
            let event = u16::from_le_bytes([buf[9], buf[10]]);
            return Request::Fetch {
                verb: v,
                heat,
                event,
            };
        }
    }
    Request::Unknown(buf.to_vec())
}

fn nack() -> Vec<u8> {
    no_data_reply()
}

/// 6-byte “no data / nothing to report” frame the real CTS6 returns
/// when an SSBIE/SSBIL/SSBIN/SSBIR query has no matching race. An
/// auto-polling client treats this as “device alive but bucket
/// empty”, which is essential to keep the polling loop healthy —
/// silence would otherwise look like a stalled connection.
fn no_data_reply() -> Vec<u8> {
    vec![0x06, 0x00, 0x32, 0x00, 0xC7, 0xFF]
}

fn identify_reply() -> Vec<u8> {
    // `<len> 00 <ascii…> 00 <chk> FE` — clients parse by reading
    // the first NUL after byte 2. The `0xFF` byte before `0xFE` is literal.
    let s = FIRMWARE.as_bytes();
    let mut buf = Vec::with_capacity(s.len() + 5);
    let total = (s.len() + 5) as u8; // len + 00 + ascii + 00 + FF + FE
    buf.push(total);
    buf.push(0x00);
    buf.extend_from_slice(s);
    buf.push(0x00);
    buf.push(0xFF);
    buf.push(0xFE);
    buf
}

fn commit_ack() -> Vec<u8> {
    vec![0x05, 0x00, 0x07, 0xF3, 0xFF]
}

fn comm_test_ack() -> Vec<u8> {
    vec![0x06, 0x00, 0x32, 0x01, 0x3B, 0xEB]
}

fn slot_reply(slot: u8) -> Vec<u8> {
    // 26 bytes: 1A 00 <slot> <20 ASCII NUL-pad name> <flag1> <flag2> <chk_hi> <chk_lo>
    let name = format!("Emulator Slot {slot}");
    let mut name_bytes = name.into_bytes();
    name_bytes.resize(20, 0);
    let mut buf = vec![0u8; 26];
    buf[0] = 0x1A;
    buf[1] = 0x00;
    buf[2] = slot;
    buf[3..23].copy_from_slice(&name_bytes);
    buf[23] = 0x00; // flag1
    buf[24] = 0x00; // flag2
    let chk = cts6_chk(&buf[..25]);
    buf[25] = chk;
    buf
}

fn meet_status_reply() -> Vec<u8> {
    // 51-byte frame parsed by `parse_selected_meet_reply`.
    // Fields: second/minute/hour/dow/month/day at bytes 8..14, year LE
    // at 40-41, length byte 0x33 at byte 0.
    let mut buf = vec![0u8; 51];
    buf[0] = 0x33; // length sentinel
    buf[8] = 0; // second
    buf[9] = 0; // minute
    buf[10] = 12; // hour
    buf[11] = 7; // dow
    buf[12] = 5; // month
    buf[13] = 9; // day
    buf[14] = (YEAR - 2000) as u8;
    buf[40] = (YEAR & 0xFF) as u8;
    buf[41] = (YEAR >> 8) as u8;
    buf[50] = 0xFB; // trailer marker
    buf
}

// ─── TCP server ────────────────────────────────────────────────────────

/// Read one CTS6 frame from the stream. Frames are length-prefixed:
/// byte 0 is the total frame length. Returns `Ok(None)` on clean EOF.
fn read_frame(stream: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
    let mut len_byte = [0u8; 1];
    match stream.read_exact(&mut len_byte) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let total = len_byte[0] as usize;
    if total == 0 || total > 256 {
        // Resync: drop the byte, log, and return Unknown later.
        return Ok(Some(vec![len_byte[0]]));
    }
    let mut rest = vec![0u8; total - 1];
    stream.read_exact(&mut rest)?;
    let mut buf = Vec::with_capacity(total);
    buf.push(len_byte[0]);
    buf.extend_from_slice(&rest);
    Ok(Some(buf))
}

fn handle_client(mut stream: TcpStream, state: Arc<Mutex<State>>) {
    let peer = stream
        .peer_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| "?".into());
    emu_log!("[net] client connected: {peer}");
    // Match real-timer behaviour — short read timeout so a stuck reader
    // doesn't pin the only connection slot forever.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));

    loop {
        let frame = match read_frame(&mut stream) {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => {
                emu_log!("[net] {peer} read error: {e}");
                break;
            }
        };
        let req = parse_request(&frame);
        let reply = match req {
            Request::Identify => Some(identify_reply()),
            Request::Commit => Some(commit_ack()),
            Request::CommTest => Some(comm_test_ack()),
            Request::Slot(s) => Some(slot_reply(s)),
            Request::MeetStatus => Some(meet_status_reply()),
            Request::Fetch { verb, heat, event } => {
                let mut s = state.lock().unwrap();
                let race = match verb {
                    Verb::Latest => s.lookup_latest(event, heat),
                    Verb::Previous => s.lookup_previous(event, heat),
                    Verb::Next => s.lookup_next(event, heat),
                    Verb::ByRace => None, // shouldn't happen here
                };
                race.map(build_event_response)
            }
            Request::FetchByRace { race } => {
                let s = state.lock().unwrap();
                s.lookup_by_race(race).map(build_event_response)
            }
            Request::Unknown(bytes) => {
                emu_log!(
                    "[net] {peer} unknown frame ({} B): {}",
                    bytes.len(),
                    hex(&bytes)
                );
                Some(nack())
            }
        };

        // Always reply: real CTS6 returns the 6-byte no-data frame for
        // empty buckets so the auto-poller stays alive. Silence here
        // would look like a dropped connection.
        let resp = reply.unwrap_or_else(|| {
            emu_log!(
                "[net] {peer} no-data reply (empty bucket for frame: {})",
                hex(&frame)
            );
            no_data_reply()
        });
        if let Err(e) = stream.write_all(&resp) {
            emu_log!("[net] {peer} write error: {e}");
            break;
        }
    }
    emu_log!("[net] client disconnected: {peer}");
}

fn run_server(addr: &str, state: Arc<Mutex<State>>) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    emu_log!("[net] listening on {addr}");
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let s = Arc::clone(&state);
                thread::spawn(move || handle_client(stream, s));
            }
            Err(e) => emu_log!("[net] accept error: {e}"),
        }
    }
    Ok(())
}

// ─── REPL ──────────────────────────────────────────────────────────────

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{:02X}", b))
        .collect::<Vec<_>>()
        .join(" ")
}

fn fmt_time(ms: u32) -> String {
    let mins = ms / 60_000;
    let rem = ms % 60_000;
    let secs = rem / 1000;
    let millis = rem % 1000;
    if mins > 0 {
        format!("{mins}:{secs:02}.{millis:03}")
    } else {
        format!("{secs}.{millis:03}")
    }
}

/// Compact place label: "1st", "2nd", "3rd", "4th"\u2026 Up through 10
/// lanes (the pool max) the irregular suffixes are explicit; anything
/// beyond falls back to the "th" rule.
fn place_label(place: u8) -> String {
    match place {
        1 => "1st".to_string(),
        2 => "2nd".to_string(),
        3 => "3rd".to_string(),
        n => format!("{n}th"),
    }
}

/// Maximum number of log lines retained in the TUI's scrollback ring.
/// Older lines are dropped to keep memory bounded during long meets.
const MAX_LOG: usize = 2000;

/// Visible TUI state. The shared [`State`] mutex stays the source of
/// truth for race data; this struct only holds presentation state
/// (input buffer, log ring, history cursor).
struct TuiApp {
    state: Arc<Mutex<State>>,
    log_rx: mpsc::Receiver<String>,
    log: VecDeque<String>,
    input: String,
    cursor: usize,
    history: Vec<String>,
    history_idx: Option<usize>,
    scratch: String,
    quit: bool,
    /// When true, draw the help text as a centered modal overlay
    /// instead of dumping it into the log pane. Toggled by `/help`,
    /// `/?`, or F1; dismissed by Esc / Enter / `q` / `?`.
    show_help: bool,
    /// When `Some(race_no)`, draw a centered modal showing that
    /// race's per-lane results (place, finish time, every recorded
    /// split). Set by pressing Enter on a Race row in the stored
    /// events tree; cleared by Esc / Enter / `q`.
    show_results: Option<u16>,
    /// Which pane currently owns keyboard focus. Tab toggles between
    /// the command input (default) and the stored-events tree.
    focus: Focus,
    /// Events expanded in the stored-events tree (event numbers).
    tree_expanded_events: HashSet<u16>,
    /// (event, heat) tuples expanded in the stored-events tree.
    tree_expanded_heats: HashSet<(u16, u8)>,
    /// Selection cursor into the flattened tree row list. Clamped
    /// against the current row count each frame.
    tree_selected: usize,
    /// Top row of the tree viewport (for scrolling). Adjusted to
    /// keep `tree_selected` visible.
    tree_scroll: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
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

fn run_tui(app: &mut TuiApp) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let res = (|| -> io::Result<()> {
        while !app.quit {
            // Drain anything the network thread (or command handlers)
            // pushed since the last frame.
            while let Ok(line) = app.log_rx.try_recv() {
                if app.log.len() >= MAX_LOG {
                    app.log.pop_front();
                }
                app.log.push_back(line);
            }
            terminal.draw(|f| draw(f, app))?;
            // 100ms poll = 10 fps redraw cap, which is plenty for a
            // human-readable running clock and keeps CPU near zero.
            if event::poll(Duration::from_millis(100))? {
                handle_event(app, event::read()?);
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
    // Scoreboard height grows with the active lane spread (header +
    // separator + N lanes + 1 of padding + borders). Per-lane arm
    // tags render inline on the lane row so no extra rows needed.
    let lane_rows = {
        let s = app.state.lock().unwrap();
        let (lo, hi) = s.lane_spread;
        hi.saturating_sub(lo).saturating_add(1) as u16
    };
    let scoreboard_h = (lane_rows + 5).max(7);
    let chunks = Layout::vertical([
        Constraint::Length(scoreboard_h),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .split(area);
    // Top row: scoreboard on the left, stored-events tree on the
    // right. Tree gets a fixed 42-col slice — wide enough for
    // "  ▸ Event 12: 200X RL  (3 heats, 8 races)" without wrapping.
    let top = Layout::horizontal([Constraint::Min(40), Constraint::Length(42)]).split(chunks[0]);
    draw_scoreboard(f, top[0], &app.state);
    draw_stored_events(f, top[1], app);
    draw_log(f, chunks[1], &app.log);
    draw_input(f, chunks[2], app);
    if app.show_help {
        draw_help_popup(f, area);
    }
    if let Some(race_no) = app.show_results {
        draw_results_popup(f, area, &app.state, race_no);
    }
}

fn draw_scoreboard(f: &mut Frame, area: Rect, state: &Arc<Mutex<State>>) {
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
            let elapsed_ms = ip.started_at.elapsed().as_millis().min(u32::MAX as u128) as u32;
            let event_def = s.lineup.get(ip.event as usize - 1);
            let label = event_def
                .map(|e| e.label())
                .unwrap_or_else(|| format!("event {}", ip.event));
            // Total expected touches per lane (final = finish, earlier
            // = splits). Default to 1 if no matching EventDef \u2014 a free
            // race with no lineup acts like a single-segment sprint.
            let total_segments = event_def.map(|e| e.total_segments()).unwrap_or(1);
            lines.push(Line::from(vec![
                Span::raw("  race "),
                Span::styled(
                    format!("{}", ip.race_no),
                    Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
                ),
                Span::raw("    running time  "),
                Span::styled(
                    fmt_time(elapsed_ms),
                    Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("    {label}  heat {}", ip.heat)),
            ]));
            lines.push(Line::from(Span::styled(
                "  ─────────────────────────────────────────────────────────────",
                Style::new().fg(Color::DarkGray),
            )));
            // Place is awarded only once a lane has gone FINAL (hit
            // its expected total touches). Until then the place column
            // stays blank — ranking mid-race off touch counts is
            // misleading. Among finished lanes, sort by finish time
            // ascending. Map is lane_idx -> 1-indexed place.
            let places: HashMap<usize, u8> = {
                let mut ranked: Vec<(usize, u32)> = ip
                    .lanes
                    .iter()
                    .enumerate()
                    .filter(|(i, lt)| {
                        let ln = (*i as u8) + 1;
                        ln >= lo && ln <= hi && !lt.dq && lt.finished_at.is_some()
                    })
                    .map(|(i, lt)| (i, lt.finished_at.unwrap()))
                    .collect();
                ranked.sort_by_key(|(_, t)| *t);
                ranked
                    .into_iter()
                    .enumerate()
                    .map(|(rank, (idx, _))| (idx, (rank + 1) as u8))
                    .collect()
            };
            for (i, lt) in ip.lanes.iter().enumerate() {
                let lane_no = (i + 1) as u8;
                if lane_no < lo || lane_no > hi {
                    continue;
                }
                let mut spans: Vec<Span> = vec![Span::styled(
                    format!("  Lane{lane_no:>2}  "),
                    Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                )];
                // Place tag sits between the lane identifier and the
                // time. Width is fixed (5 cols incl. trailing spaces)
                // so all rows line up regardless of who's placed.
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
                    // Once a lane goes FINAL, blink the time 3 times
                    // (250ms phases \u00d7 6 = 1500ms total) then hold
                    // steady. Phase 0/2/4 = visible bright, 1/3/5 =
                    // dim/hidden; phase \u22656 = steady bright.
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
                    spans.push(Span::styled(format!("{:>10}", fmt_time(t)), time_style));
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
                    // Cumulative split history is still recorded in
                    // `touches_ms` (and emitted on-wire / in /races),
                    // but the scoreboard intentionally shows only the
                    // most-recent touch time so long races (500 free
                    // = 9 splits, 400 relay = 7) don't run off the
                    // pane width.
                } else {
                    spans.push(Span::styled("        --", Style::new().fg(Color::DarkGray)));
                }
                // Per-lane arm indicator. Each lane independently
                // tracks how many touches it owes; once the next
                // expected touch IS the finish, that lane blinks
                // FINISH ARMED until the swimmer hits the pad. Lanes
                // that have already gone final or been DQ'd show no
                // arm tag. With no lineup entry (total_segments==0)
                // we can't infer arm state, so suppress.
                if !lt.dq && lt.finished_at.is_none() && total_segments > 0 {
                    let next_touch = (lt.touches_ms.len() as u16).saturating_add(1);
                    if next_touch == total_segments {
                        // FINISH ARMED — 2Hz blink off race-elapsed
                        // ms (independent of redraw cadence).
                        let on = (elapsed_ms / 500) % 2 == 0;
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
    // Bright cyan border + bold title to make the scoreboard the
    // visual anchor of the TUI; default-fg text inside reads as the
    // primary content.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::Cyan))
        .title(Span::styled(
            " scoreboard ",
            Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

/// Flatten the stored race history into the visible tree row list,
/// honouring expand/collapse state. Events sort numerically; heats
/// and races sort numerically within their parent. Always rebuilt
/// each frame — cheap, and keeps the tree in sync with new races
/// without a separate notification path.
fn build_tree_rows(state: &State, app: &TuiApp) -> Vec<TreeRow> {
    // Group history by event for the top-level rows.
    let mut by_event: HashMap<u16, Vec<(u8, &Vec<u16>)>> = HashMap::new();
    for ((event_no, heat), races) in state.history.iter() {
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
    let s = app.state.lock().unwrap();
    let rows = build_tree_rows(&s, app);
    let focused = app.focus == Focus::Tree;
    let border_color = if focused {
        Color::Magenta
    } else {
        Color::DarkGray
    };
    let title_style = Style::new().fg(border_color).add_modifier(Modifier::BOLD);
    // Viewport: account for top + bottom borders.
    let view_h = area.height.saturating_sub(2) as usize;
    // Clamp selection + scroll against current row count. (We don't
    // mutate app here — that happens in handle_event before the next
    // draw — so we just compute a clamped local view.)
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
                        .map(|e| e.label())
                        .unwrap_or_else(|| format!("event {event_no}"));
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
                    // Look up the race for a quick "winner" summary.
                    let summary = s.races.get(race_no).and_then(|r| {
                        r.lanes
                            .iter()
                            .filter_map(|opt| opt.as_ref())
                            .filter(|row| row.place == 1)
                            .map(|row| {
                                let t = row.lane.finish_ms();
                                if t > 0 {
                                    fmt_time(t)
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
    // Anchor to the tail: only render lines that fit in the visible
    // area so we avoid scroll-state bookkeeping. Newest at the bottom.
    // A single log entry may contain embedded '\n' (e.g. /help output),
    // so expand into one ratatui Line per physical line — otherwise the
    // newlines render as glyphs/spaces and the formatting collapses.
    let inner_h = area.height.saturating_sub(2) as usize; // borders
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
    // When focus is on the input pane, brighten everything: cyan
    // border, bold cyan prompt, white input. When focus is on the
    // tree, dim the prompt and de-emphasize the border so it's
    // visually obvious where keystrokes are going.
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
    // Only show the on-screen cursor when the input pane actually
    // owns the keyboard — otherwise it looks like keystrokes will
    // land here when they're really going to the tree.
    if focused {
        let cx = area.x + 1 + PROMPT.len() as u16 + app.cursor as u16;
        let cy = area.y + 1;
        let max_x = area.x + area.width.saturating_sub(2);
        f.set_cursor_position((cx.min(max_x), cy));
    }
}

/// Centered modal that displays `HELP_TEXT` over the underlying TUI.
/// Sized to the help content (with sensible caps), clamped to the
/// terminal area, and `Clear`ed first so the scoreboard/log don't
/// bleed through.
fn draw_help_popup(f: &mut Frame, area: Rect) {
    let lines: Vec<Line> = HELP_TEXT.split('\n').map(Line::from).collect();
    let content_h = lines.len() as u16;
    let content_w = HELP_TEXT
        .split('\n')
        .map(|l| l.chars().count() as u16)
        .max()
        .unwrap_or(0);
    // +2 for borders, +2 for a 1-col inner padding on each side.
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
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" help — Esc/Enter to close ")
        .style(Style::new().fg(Color::White).bg(Color::Black));
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

/// Centered overlay showing per-lane results for a single finalized
/// race: place, finish time, and every cumulative split. Triggered
/// by pressing Enter on a Race row in the stored-events tree;
/// dismissed by any key (Esc / Enter / q / etc.).
fn draw_results_popup(f: &mut Frame, area: Rect, state: &Arc<Mutex<State>>, race_no: u16) {
    let s = state.lock().unwrap();
    let race = match s.lookup_by_race(race_no) {
        Some(r) => r,
        None => return,
    };
    let event_label = s
        .lineup
        .get((race.event as usize).saturating_sub(1))
        .map(|e| e.label())
        .unwrap_or_else(|| format!("event {}", race.event));

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(
            format!("  Race {}  ", race.race_no),
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

    // Sort lanes for display: real placers first (by ascending place),
    // then DQs, then untouched / empty lanes. Within the trailing
    // groups, fall back to lane number for stability.
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
        let lane_no = (*i as u8) + 1;
        let place_str = if row.place > 0 {
            place_label(row.place)
        } else {
            "—".to_string()
        };
        let mut spans: Vec<Span> = vec![
            Span::styled(
                format!("  {:>3}  ", place_str),
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
                format!("{:>10}  ", fmt_time(row.lane.finish_ms())),
                Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            ));
        }
        // Cumulative splits = every touch BEFORE the final one.
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
                spans.push(Span::styled(fmt_time(*ms), Style::new().fg(Color::Gray)));
            }
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Esc/Enter to close",
        Style::new().fg(Color::DarkGray),
    )));

    // Size the popup to its longest visible line, then clamp to the
    // available area. Account for borders + 1-col inner padding.
    let content_w = lines.iter().map(|l| l.width() as u16).max().unwrap_or(0);
    let content_h = lines.len() as u16;
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
    let title = format!(" race {} results — Esc/Enter to close ", race.race_no);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .style(Style::new().fg(Color::White).bg(Color::Black));
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        popup,
    );
}

fn handle_event(app: &mut TuiApp, ev: Event) {
    let key = match ev {
        Event::Key(k) if k.kind == KeyEventKind::Press => k,
        _ => return,
    };
    // While the help popup is up, swallow all input — any of
    // Esc/Enter/q/?/F1 dismisses it; everything else is ignored so
    // it can't accidentally edit the input buffer underneath.
    if app.show_help {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
            | (KeyCode::Char('d'), KeyModifiers::CONTROL) => app.quit = true,
            _ => app.show_help = false,
        }
        return;
    }
    // Results popup behaves the same way as the help modal: any key
    // dismisses, Ctrl-C/D still quits the whole app.
    if app.show_results.is_some() {
        match (key.code, key.modifiers) {
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
            | (KeyCode::Char('d'), KeyModifiers::CONTROL) => app.quit = true,
            _ => app.show_results = None,
        }
        return;
    }
    // Tree pane keybindings. Active only when focused, so the
    // command input keeps full control of the keyboard otherwise
    // (typing 'q' in input mode must NOT quit, etc.).
    if app.focus == Focus::Tree {
        // Quit shortcuts still work from any focus.
        if matches!(
            (key.code, key.modifiers),
            (KeyCode::Char('c'), KeyModifiers::CONTROL)
                | (KeyCode::Char('d'), KeyModifiers::CONTROL)
        ) {
            app.quit = true;
            return;
        }
        // Snapshot the visible row list so navigation operates on
        // exactly what the user sees.
        let rows = {
            let s = app.state.lock().unwrap();
            build_tree_rows(&s, app)
        };
        let last = rows.len().saturating_sub(1);
        match (key.code, key.modifiers) {
            (KeyCode::Tab, _) | (KeyCode::Esc, _) => app.focus = Focus::Input,
            (KeyCode::Up, _) => {
                app.tree_selected = app.tree_selected.saturating_sub(1);
            }
            (KeyCode::Down, _) => {
                if app.tree_selected < last {
                    app.tree_selected += 1;
                }
            }
            (KeyCode::Home, _) => app.tree_selected = 0,
            (KeyCode::End, _) => app.tree_selected = last,
            (KeyCode::Right, _) | (KeyCode::Enter, _) => {
                if let Some(row) = rows.get(app.tree_selected) {
                    match row {
                        TreeRow::Event { event_no, .. } => {
                            app.tree_expanded_events.insert(*event_no);
                        }
                        TreeRow::Heat { event_no, heat, .. } => {
                            app.tree_expanded_heats.insert((*event_no, *heat));
                        }
                        TreeRow::Race { race_no, .. } => {
                            // Pop the per-heat results modal — same
                            // overlay UX as the help screen.
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
                            // Jump back up to the parent heat row so a
                            // second Left collapses it — matches the
                            // common file-tree UX.
                            app.tree_expanded_heats.remove(&(*event_no, *heat));
                            // Find the parent's index and select it.
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
        (KeyCode::Char('c'), KeyModifiers::CONTROL)
        | (KeyCode::Char('d'), KeyModifiers::CONTROL)
        | (KeyCode::Esc, _) => {
            app.quit = true;
        }
        (KeyCode::F(1), _) => {
            app.show_help = true;
        }
        (KeyCode::Tab, _) => {
            app.focus = Focus::Tree;
            // Reset selection if it's stale relative to current rows.
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
                // Avoid consecutive duplicates in history.
                if app.history.last().map(|h| h.as_str()) != Some(line.as_str()) {
                    app.history.push(line.clone());
                }
            }
            app.history_idx = None;
            app.scratch.clear();
            // Intercept `/help` / `/?` before it ever reaches
            // handle_command — show the modal instead of spamming
            // the log pane with the multi-line help text.
            let trimmed = line.trim();
            if trimmed == "/help" || trimmed == "/?" {
                app.show_help = true;
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
        (KeyCode::Backspace, _) => {
            if app.cursor > 0 {
                app.cursor -= 1;
                app.input.remove(app.cursor);
            }
        }
        (KeyCode::Delete, _) => {
            if app.cursor < app.input.len() {
                app.input.remove(app.cursor);
            }
        }
        (KeyCode::Left, _) => {
            if app.cursor > 0 {
                app.cursor -= 1;
            }
        }
        (KeyCode::Right, _) => {
            if app.cursor < app.input.len() {
                app.cursor += 1;
            }
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
            // Skip control chord characters (Ctrl-X, Alt-X) so they
            // don't get inserted as literal input.
            if m.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                return;
            }
            app.input.insert(app.cursor, c);
            app.cursor += 1;
        }
        _ => {}
    }
}

fn parse_gender(s: &str) -> Option<Gender> {
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
fn parse_stroke(s: &str) -> Option<Stroke> {
    let up = s.trim().to_ascii_uppercase();
    // Strip internal hyphens/spaces so MED-R, MED R, MEDR all match,
    // but compare the original (uppercased) string first so the
    // hyphenated canonical forms remain unambiguous.
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
/// inclusive `(low, high)` pair. Both endpoints must lie in
/// `1..=MAX_LANES` and satisfy `low <= high`. Returns `None` on any
/// malformed input so the caller can print a usage hint.
fn parse_lane_spread(s: &str) -> Option<(u8, u8)> {
    let (a, b) = s.split_once("..")?;
    let lo: u8 = a.trim().parse().ok()?;
    let hi: u8 = b.trim().parse().ok()?;
    if lo < 1 || hi < 1 || lo > MAX_LANES as u8 || hi > MAX_LANES as u8 || lo > hi {
        return None;
    }
    Some((lo, hi))
}

/// Start a new in-progress race for the current (event, heat).
fn start_race(state: &mut State) {
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
        .map(|e| e.label())
        .unwrap_or_else(|| format!("event {}", state.current_event));
    println!(
        "race {race_no} started — {label} heat {heat} (touch lanes 1..8 — each touch is a split, / to finalize)",
        heat = state.current_heat,
    );
}

/// Touch a lane during the in-progress race. Each call appends a
/// cumulative split for that lane; the LAST touch before `/` becomes
/// the lane's finish (`primary_ms`).
///
/// If the current event has a known total-segment count (from the
/// lineup) and this lane has already accumulated that many touches,
/// the call is a no-op — the lane is "final" and further pad hits
/// are ignored, matching CTS6 behaviour where a finished lane stops
/// reading splits.
fn touch_lane(state: &mut State, lane: u8) {
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
    // Look up segment cap BEFORE borrowing in_progress mutably so we
    // can also touch state.lineup. 0 means "no lineup entry, don't cap".
    let total_segments: u16 = state
        .in_progress
        .as_ref()
        .and_then(|ip| state.lineup.get(ip.event as usize - 1))
        .map(|d| d.total_segments())
        .unwrap_or(0);
    let ip = match state.in_progress.as_mut() {
        Some(i) => i,
        None => {
            println!("(no race in progress — press <enter> to start)");
            return;
        }
    };
    let elapsed = ip.started_at.elapsed().as_millis().min(u32::MAX as u128) as u32;
    let cum = elapsed.max(1);
    let entry = &mut ip.lanes[lane_idx];
    if entry.finished_at.is_some() {
        println!("  lane {lane} already FINAL — touch ignored");
        return;
    }
    // Defensive: keep splits monotonic. A re-touch at an earlier
    // wall-clock time (shouldn't happen, but ¯paste lag¯) gets
    // bumped to last+1ms so downstream split filters don't drop it.
    let cum = match entry.touches_ms.last().copied() {
        Some(prev) if cum <= prev => prev.saturating_add(1),
        _ => cum,
    };
    entry.touches_ms.push(cum);
    let n = entry.touches_ms.len() as u16;
    if total_segments > 0 && n >= total_segments {
        entry.finished_at = Some(cum);
        println!(
            "  lane {lane} → FINISH @ {} ({}/{} touches)",
            fmt_time(cum),
            n,
            total_segments,
        );
    } else {
        println!("  lane {lane} → split {} @ {}", n, fmt_time(cum),);
    }
}

fn dq_lane(state: &mut State, lane: u8) {
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
    let ip = match state.in_progress.as_mut() {
        Some(i) => i,
        None => {
            println!("(no race in progress)");
            return;
        }
    };
    ip.lanes[lane_idx].dq = true;
    println!("  lane {lane} → DQ");
}

/// Finalize the in-progress race (timer "store/print").
///
/// Per-lane processing:
///   * 0 touches & not DQ → empty lane (skipped in frame).
///   * 0 touches & DQ      → lane row with place=0, dq=true.
///   * ≥1 touches          → last touch = finish; earlier touches =
///                            cumulative splits.
///
/// Place is assigned by sorting non-DQ lanes by finish time ascending.
/// DQ lanes get place=0 (the parser surfaces them via the 0xFF
/// sentinel → disqualified flag).
fn finalize_race(state: &mut State) {
    let ip = match state.in_progress.take() {
        Some(i) => i,
        None => {
            println!("(no race in progress)");
            return;
        }
    };

    // Build LaneResultRows + collect (lane_idx, finish_ms) for non-DQ
    // lanes so we can assign places.
    let mut rows: [Option<LaneResultRow>; MAX_LANES] = Default::default();
    let mut finishers: Vec<(usize, u32)> = Vec::new();
    let mut splits_max: u8 = 0;
    for (i, lt) in ip.lanes.into_iter().enumerate() {
        let touched = !lt.touches_ms.is_empty();
        if !touched && !lt.dq {
            continue;
        }
        let split_count = lt.split_prefix().len() as u8;
        if split_count > splits_max {
            splits_max = split_count;
        }
        if touched && !lt.dq {
            finishers.push((i, lt.finish_ms()));
        }
        rows[i] = Some(LaneResultRow { lane: lt, place: 0 });
    }
    // Assign places by ascending finish time.
    finishers.sort_by_key(|&(_, ms)| ms);
    for (place_minus_1, (lane_idx, _)) in finishers.iter().enumerate() {
        if let Some(r) = rows[*lane_idx].as_mut() {
            r.place = (place_minus_1 + 1) as u8;
        }
    }

    let lane_count = rows.iter().filter(|r| r.is_some()).count();
    let race = Race {
        event: ip.event,
        heat: ip.heat,
        race_no: ip.race_no,
        lanes: rows,
        splits_count: splits_max,
    };
    let key = (race.event, race.heat);
    state.races.insert(race.race_no, race);
    state.history.entry(key).or_default().push(ip.race_no);
    // Fresh SSBIE will reseed; clear the cursor so a stale walk doesn't
    // skip the new latest race.
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

fn print_lineup(state: &State) {
    if state.lineup.is_empty() {
        println!("(no lineup configured)");
        return;
    }
    for (i, e) in state.lineup.iter().enumerate() {
        let marker = if (i + 1) as u16 == state.current_event {
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

/// Standard NFHS high-school dual-meet event order (12 events).
/// All gendered as Mixed for emulator simplicity \u2014 in a real coed
/// dual the same 12 are run twice (girls then boys). Splits default
/// to 50 yd, which matches CTS6 split-arm behaviour for these races.
fn high_school_lineup() -> Vec<EventDef> {
    use Gender::Mixed;
    use Stroke::*;
    let mk = |distance: u16, stroke: Stroke| EventDef {
        distance,
        gender: Mixed,
        stroke,
        split_yards: 50,
    };
    vec![
        mk(200, MedleyRelay), //  1. 200 medley relay   (BK/BR/FL/FR, 50 each)
        mk(200, Free),        //  2. 200 free
        mk(200, Im),          //  3. 200 IM             (FL/BK/BR/FR, 50 each)
        mk(50, Free),         //  4. 50 free
        EventDef {
            //  5. diving
            distance: 0,
            gender: Mixed,
            stroke: Diving,
            split_yards: 0,
        },
        mk(100, Fly),            //  6. 100 fly
        mk(100, Free),           //  7. 100 free
        mk(500, Free),           //  8. 500 free
        mk(200, FreestyleRelay), //  9. 200 free relay     (4\u00d750 free)
        mk(100, Back),           // 10. 100 back
        mk(100, Breast),         // 11. 100 breast
        mk(400, FreestyleRelay), // 12. 400 free relay     (4\u00d7100 free, splits every 50)
    ]
}

fn print_races(state: &State) {
    if state.races.is_empty() {
        println!("(no races stored)");
        return;
    }
    let mut all: Vec<&Race> = state.races.values().collect();
    all.sort_by_key(|r| r.race_no);
    for r in all {
        let lanes = r.lanes.iter().filter(|l| l.is_some()).count();
        println!(
            "  race {:>3}: event {} heat {} ({lanes} lanes, {} splits)",
            r.race_no, r.event, r.heat, r.splits_count,
        );
    }
}

fn handle_command(state: &mut State, line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        // Empty enter → start a race.
        start_race(state);
        return true;
    }
    // Slash-commands.
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
                // Override the race counter so the operator can resume
                // mid-meet (e.g. after a restart) without redoing
                // races 1..N-1. Only valid when no race is in flight;
                // otherwise the in-progress race would inherit a stale
                // number on /print.
                if state.in_progress.is_some() {
                    println!("cannot change /race while a race is in progress (finalize with / first)");
                } else if let Some(n) =
                    parts.next().and_then(|s| s.parse::<u16>().ok())
                {
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
                    // Optional 4th arg: split distance in yards (25 or 50).
                    // Defaults to 50 — the standard CTS6 split arming.
                    let split_yards = parts
                        .next()
                        .and_then(|s| s.parse::<u16>().ok())
                        .unwrap_or(50);
                    match (dist, gender, stroke) {
                        (Some(d), Some(g), Some(s)) => {
                            state.lineup.push(EventDef {
                                distance: d,
                                gender: g,
                                stroke: s,
                                split_yards,
                            });
                            println!(
                                "added event {}: {} (splits every {split_yards} yd)",
                                state.lineup.len(),
                                state.lineup.last().unwrap().label()
                            );
                        }
                        _ => println!(
                            "usage: /lineup add <distance> <M|F|X> <stroke> [split_yards]\n  stroke: 1/FR  2/BK  3/BR  4/FL  5/IM  6/MED-R  7/FR-R  DV"
                        ),
                    }
                }
                "preset" => match parts.next().unwrap_or("") {
                    "hs" | "highschool" | "high-school" => {
                        state.lineup = high_school_lineup();
                        println!(
                            "loaded high-school dual-meet lineup ({} events)",
                            state.lineup.len()
                        );
                    }
                    other => println!(
                        "unknown preset: {other:?} (try /lineup preset hs)"
                    ),
                },
                "clear" => {
                    state.lineup.clear();
                    println!("lineup cleared");
                }
                other => println!("unknown lineup subcommand: {other}"),
            },
            "races" => print_races(state),
            "lanes" => match parts.next() {
                None => {
                    let (lo, hi) = state.lane_spread;
                    println!("lane spread = {lo}..{hi}");
                }
                Some(spec) => match parse_lane_spread(spec) {
                    Some((lo, hi)) => {
                        if state.in_progress.is_some() {
                            println!("cannot change /lanes while a race is in progress (finalize with / first)");
                        } else {
                            state.lane_spread = (lo, hi);
                            println!("lane spread = {lo}..{hi}");
                        }
                    }
                    None => println!(
                        "usage: /lanes A..B  (1..={MAX_LANES}, e.g. /lanes 1..10, /lanes 2..8, /lanes 1..1)"
                    ),
                },
            },
            "status" => {
                let (lo, hi) = state.lane_spread;
                println!(
                    "event={} heat={} next_race_no={} lanes={lo}..{hi} in_progress={} stored={}",
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
    // Bare numbers — touch lanes (one or more, whitespace separated).
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

fn print_help() {
    println!("{HELP_TEXT}");
}

/// Help text shared between the legacy `println!` path (e.g. when
/// the TUI isn't running) and the in-TUI modal popup.
const HELP_TEXT: &str = "cts6 emulator commands:\n\
     \n\
     meet setup:\n\
       /event N                  set active event\n\
       /heat  N                  set active heat\n\
       /race  N                  set next race number (resume mid-meet)\n\
       /lanes A..B               set active lane spread (1..=10,\n\
                                   e.g. /lanes 1..10, 2..8, 1..1)\n\
       /lineup show              list configured events\n\
       /lineup preset hs         load NFHS high-school dual-meet lineup\n\
       /lineup add D G S [Y]     add event (distance, M|F|X,\n\
                                   stroke 1/FR 2/BK 3/BR 4/FL 5/IM\n\
                                   6/MED-R 7/FR-R DV, optional split-yds)\n\
       /lineup clear             remove all events\n\
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

fn parse_args() -> String {
    let mut args = std::env::args().skip(1);
    let mut addr = DEFAULT_ADDR.to_string();
    while let Some(a) = args.next() {
        match a.as_str() {
            "--addr" | "-a" => {
                if let Some(v) = args.next() {
                    addr = v;
                }
            }
            "-h" | "--help" => {
                eprintln!("usage: cts6-emulator [--addr 127.0.0.1:1337]");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown flag: {other}");
                std::process::exit(2);
            }
        }
    }
    addr
}

fn main() {
    let addr = parse_args();
    let state = Arc::new(Mutex::new(State::new()));

    // Wire up the log channel BEFORE the network thread spawns so its
    // first lines (e.g. "[net] listening on …") flow into the TUI
    // log pane instead of the now-suppressed stdout.
    let (log_tx, log_rx) = mpsc::channel::<String>();
    let _ = LOG_TX.set(log_tx);

    {
        let s = Arc::clone(&state);
        let a = addr.clone();
        thread::spawn(move || {
            if let Err(e) = run_server(&a, s) {
                emu_log!("[net] fatal: {e}");
            }
        });
    }

    emu_log!("cts6-emulator on {addr} — type /help for commands  ·  Esc / Ctrl-C to quit");

    let mut app = TuiApp {
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
        show_results: None,
        focus: Focus::Input,
        tree_expanded_events: HashSet::new(),
        tree_expanded_heats: HashSet::new(),
        tree_selected: 0,
        tree_scroll: 0,
    };

    if let Err(e) = run_tui(&mut app) {
        // Best-effort cleanup in case run_tui bailed before its own
        // restore ran.
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        std::eprintln!("[fatal] tui error: {e}");
    }
    std::println!("bye");
}
