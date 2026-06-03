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
#![warn(clippy::pedantic)]
#![warn(clippy::pedantic)]
use std::io;
use std::io::Write;
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread;

use ratatui::crossterm::{
    execute,
    terminal::{disable_raw_mode, LeaveAlternateScreen},
};

mod lineups;
mod protocol;
mod repl;
mod server;
mod state;
mod tui;

use server::run_server;
use state::State;
use tui::{run_tui, TuiApp};

/// Channel into the dedicated log-printer thread. Initialized in
/// `main()`; until then `emu_log!` falls back to plain stderr so
/// early-startup messages still surface.
pub(crate) static LOG_TX: OnceLock<mpsc::Sender<String>> = OnceLock::new();

/// Channel into the dedicated audio thread. Each `()` triggers one
/// playback of the embedded wall-touch beep.
pub(crate) static AUDIO_TX: OnceLock<mpsc::Sender<()>> = OnceLock::new();

/// Embedded WAV bytes for the wall-touch beep.
const BEEP_WAV: &[u8] = include_bytes!("cts-beep.wav");

/// Fire the wall-touch beep once. Non-blocking.
pub(crate) fn play_beep() {
    if let Some(tx) = AUDIO_TX.get() {
        let _ = tx.send(());
    }
}

#[macro_export]
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

const DEFAULT_ADDR: &str = "127.0.0.1:1337";

/// Current calendar year (UTC), used to stamp the year field in the
/// SSBIE event-result and meet-status frames.
pub(crate) fn current_year() -> u16 {
    use chrono::Datelike;
    u16::try_from(chrono::Utc::now().year()).unwrap_or(2000)
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn fmt_time(ms: u32, buf: &mut [u8]) -> &str {
    let mins = ms / 60_000;
    let rem = ms % 60_000;
    let secs = rem / 1000;
    let millis = rem % 1000;

    let mut cursor = std::io::Cursor::new(buf);

    if mins > 0 {
        let _ = write!(cursor, "{mins}:{secs:02}.{millis:03}");
    } else {
        let _ = write!(cursor, "{secs}.{millis:03}");
    }

    let len = usize::try_from(cursor.position()).unwrap_or(0);
    std::str::from_utf8(&cursor.into_inner()[..len]).unwrap()
}

/// Compact place label: "1st", "2nd", "3rd", "4th"…
pub(crate) fn place_label(place: u8) -> String {
    match place {
        1 => "1st".to_string(),
        2 => "2nd".to_string(),
        3 => "3rd".to_string(),
        n => format!("{n}th"),
    }
}

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

    let (log_tx, log_rx) = mpsc::channel::<String>();
    let _ = LOG_TX.set(log_tx);

    let (audio_tx, audio_rx) = mpsc::channel::<()>();
    let _ = AUDIO_TX.set(audio_tx);
    thread::spawn(move || match rodio::OutputStream::try_default() {
        Ok((_stream, handle)) => {
            while audio_rx.recv().is_ok() {
                let cursor = std::io::Cursor::new(BEEP_WAV);
                match rodio::Decoder::new(cursor) {
                    Ok(source) => {
                        if let Ok(sink) = rodio::Sink::try_new(&handle) {
                            sink.append(source);
                            sink.detach();
                        }
                    }
                    Err(e) => emu_log!("[audio] decode failed: {e}"),
                }
            }
        }
        Err(e) => {
            emu_log!("[audio] no output device ({e}) — beeps disabled");
            while audio_rx.recv().is_ok() {}
        }
    });

    {
        let s = Arc::clone(&state);
        let a = addr.clone();
        thread::spawn(move || {
            if let Err(e) = run_server(&a, &s) {
                emu_log!("[net] fatal: {e}");
            }
        });
    }

    emu_log!("cts6-emulator on {addr} — type /help for commands  ·  Esc / Ctrl-C to quit");

    let mut app = TuiApp::new(state, log_rx);

    if let Err(e) = run_tui(&mut app) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        eprintln!("[fatal] tui error: {e}");
    }
    println!("bye");
}
