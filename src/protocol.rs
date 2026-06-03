//! CTS6 wire protocol: frame parsing, request decoding, and reply
//! construction.
//!
//! All pure functions — no `State` references. The TCP server in
//! [`crate::handle_client`] calls [`parse_request`] on each inbound
//! frame, dispatches on the resulting [`Request`] enum, and writes
//! back the appropriate `*_reply` / `*_ack` helper. The one
//! state-coupled builder is [`build_event_response`], which takes a
//! `&Race` and serializes it into the SSBIE on-wire layout.

use std::io::{self, Read};
use std::net::TcpStream;

use chrono::{Datelike, Timelike};

use crate::current_year;
use crate::state::Race;

/// Firmware version string reported in the identify reply.
pub(crate) const FIRMWARE: &str = "1.232";

/// Number of lanes serialized into the SSBIE event-result frame.
/// Pinned to 8 by the wire spec — do not change without also fixing
/// every consumer's lane-table parser.
pub(crate) const WIRE_LANES: usize = 8;

// ─── Frame helpers ─────────────────────────────────────────────────────

pub(crate) fn cts6_chk(bytes: &[u8]) -> u8 {
    let s: u32 = bytes.iter().map(|&b| u32::from(b)).sum();
    (0xFFu32.wrapping_sub(s) & 0xFF) as u8
}

/// Decode a fixed-width, NULL-padded ASCII string from a CTS6 frame.
/// Non-printable bytes are dropped; trailing nulls (and anything after
/// the first null) are stripped.
pub(crate) fn ascii_from_null_padded(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    bytes[..end]
        .iter()
        .filter(|&&b| (0x20..=0x7E).contains(&b))
        .map(|&b| b as char)
        .collect()
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
///     8 so the parser's `is_long_with_splits_frame` predicate accepts
///     the layout. Per-lane block layout matches
///     `parse_lane_table_long_with_splits`:
///     0:                place (or 0xFF for DQ)
///     1 .. 1+s*4:       splits[0..s] cumulative LE u32
///     1+s*4..+4:        primary (final touch) LE u32
///     +4..+4:           backup1 LE u32
///     +8 zeroes:        pad
///     +16..+4:          backup2 LE u32
pub(crate) fn build_event_response(race: &Race) -> Vec<u8> {
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
    buf[6] = (race.id & 0xFF) as u8;
    buf[7] = (race.id >> 8) as u8;
    // Lane-count slot at byte 16: required by long-with-splits
    // dispatcher; harmless for short frames (parser ignores it there).
    if splits > 0 {
        buf[16] = u8::try_from(lane_count).unwrap_or(0xFF);
    }
    // Year LE u16 at offsets 40-41.
    let year = current_year();
    buf[40] = (year & 0xFF) as u8;
    buf[41] = (year >> 8) as u8;
    // Event LE u16 at 44-45 (mirror — wide-frame path uses this).
    buf[44] = (race.event & 0xFF) as u8;
    buf[45] = (race.event >> 8) as u8;
    // Heat LE u16 at 46-47.
    buf[46] = race.heat;
    // Race LE u16 at 48-49 (legacy slot — also mirrored here).
    buf[48] = (race.id & 0xFF) as u8;

    for i in 0..lane_count {
        let base = 49 + i * stride;
        let Some(row) = &race.lanes[i] else { continue };
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
        let backup2 = primary.saturating_sub(2).max(u32::from(primary > 0));
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
pub(crate) enum Verb {
    Latest,   // 0x45 'E' — SSBIE
    Previous, // 0x4C 'L' — SSBIL
    Next,     // 0x4E 'N' — SSBIN
    ByRace,   // 0x52 'R' — SSBIR
}

#[derive(Debug, Clone)]
pub(crate) enum Request {
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
    /// `Is` — begin meet download (28-byte frame, slot + 21-byte name).
    LoadMeetName {
        slot: u8,
        name: String,
    },
    /// `It` — write one category-label slot (24-byte frame).
    LoadCategoryLabel {
        category: u8,
        idx: u8,
        label: String,
    },
    /// `Iu` — write or clear one event-record slot (18-byte frame).
    /// A frame with all-zero distance/gender/stroke bytes is treated as
    /// a clear; everything else is a populated event record.
    LoadEventRecord {
        slot: u8,
        event_num: u16,
        distance: u16,
        gender_idx: u8,
        stroke_idx: u8,
        is_relay: bool,
        is_clear: bool,
    },
    /// `Ir` — finalize slot (7-byte frame). Commits the buffered
    /// meet download into [`crate::State::lineup`].
    FinalizeSlot {
        slot: u8,
    },
    Unknown(Vec<u8>),
}

pub(crate) fn parse_request(buf: &[u8]) -> Request {
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
    if buf.len() == 7 && buf[0] == 0x07 && buf[1..4] == [0x00, 0x52, 0x73] && buf[6] == 0xFF {
        return Request::Slot(buf[4]);
    }
    // 7-byte Ir finalize-slot: 07 00 49 72 <slot> <chk_lo> <chk_hi>.
    // The chk_hi byte for slot 7 happens to be 0xFF (same as the Rs
    // trailer), so we must discriminate on the `Ir` opcode rather than
    // the trailer.
    if buf.len() == 7 && buf[0] == 0x07 && buf[1..4] == [0x00, 0x49, 0x72] {
        return Request::FinalizeSlot { slot: buf[4] };
    }
    // 28-byte Is meet-name: 1C 00 49 73 <slot> <21B name> <chk_lo> <chk_hi>
    if buf.len() == 28 && buf[0] == 0x1C && buf[1..4] == [0x00, 0x49, 0x73] {
        let name = ascii_from_null_padded(&buf[5..26]);
        return Request::LoadMeetName { slot: buf[4], name };
    }
    // 24-byte It category-label: 18 00 49 74 <cat> <idx> <16B label> <chk_lo> <chk_hi>
    if buf.len() == 24 && buf[0] == 0x18 && buf[1..4] == [0x00, 0x49, 0x74] {
        let label = ascii_from_null_padded(&buf[6..22]);
        return Request::LoadCategoryLabel {
            category: buf[4],
            idx: buf[5],
            label,
        };
    }
    // 18-byte Iu event-record / event-clear:
    //   12 00 49 75 <slot> <evt_lo> | <evt_hi> <dist_lo> <dist_hi>
    //   <gender+1> <stroke+0x0A> <age|0x10> <round|age> <0|round>
    //   00 00 <chk_lo> <chk_hi>
    if buf.len() == 18 && buf[0] == 0x12 && buf[1..4] == [0x00, 0x49, 0x75] {
        let slot = buf[4];
        let evt_lo = buf[5];
        let evt_hi = buf[6];
        let event_num = u16::from_le_bytes([evt_lo, evt_hi]);
        let distance = u16::from_le_bytes([buf[7], buf[8]]);
        let gender_byte = buf[9];
        let stroke_byte = buf[10];
        let is_relay = buf[11] == 0x10;
        let gender_idx = gender_byte.saturating_sub(1);
        let stroke_idx = stroke_byte.saturating_sub(0x0A);
        // Clear frames carry an event_num (so the timer knows the slot)
        // but zero everywhere else in the payload.
        let is_clear = distance == 0 && gender_byte == 0 && stroke_byte == 0 && !is_relay;
        return Request::LoadEventRecord {
            slot,
            event_num,
            distance,
            gender_idx,
            stroke_idx,
            is_relay,
            is_clear,
        };
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
                    race: u16::from(buf[7]),
                };
            }
            return Request::Fetch {
                verb: v,
                heat: buf[7],
                event: u16::from(buf[8]),
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

// ─── Reply / ACK helpers ───────────────────────────────────────────────

pub(crate) fn nack() -> Vec<u8> {
    no_data_reply()
}

/// Generic 6-byte ACK the timer returns for accepted `Is` / `It` / `Iu`
/// write frames. Chronaris-mm's `is_cts6_ack` accepts any well-formed
/// `06 00 .. .. .. FF`; we send the canonical "generic ack" shape.
pub(crate) fn load_ack() -> Vec<u8> {
    vec![0x06, 0x00, 0x06, 0x00, 0xF3, 0xFF]
}

/// 6-byte "slot committed" ack the timer returns for an `Ir` finalize.
pub(crate) fn finalize_ack() -> Vec<u8> {
    vec![0x06, 0x00, 0x64, 0x00, 0x95, 0xFF]
}

/// 6-byte “no data / nothing to report” frame the real CTS6 returns
/// when an SSBIE/SSBIL/SSBIN/SSBIR query has no matching race. An
/// auto-polling client treats this as “device alive but bucket
/// empty”, which is essential to keep the polling loop healthy —
/// silence would otherwise look like a stalled connection.
pub(crate) fn no_data_reply() -> Vec<u8> {
    vec![0x06, 0x00, 0x32, 0x00, 0xC7, 0xFF]
}

pub(crate) fn identify_reply() -> Vec<u8> {
    // `<len> 00 <ascii…> 00 <chk> FE` — clients parse by reading
    // the first NUL after byte 2. The `0xFF` byte before `0xFE` is literal.
    let s = FIRMWARE.as_bytes();
    let mut buf = Vec::with_capacity(s.len() + 5);
    let total = u8::try_from(s.len() + 5).unwrap_or(0); // len + 00 + ascii + 00 + FF + FE
    buf.push(total);
    buf.push(0x00);
    buf.extend_from_slice(s);
    buf.push(0x00);
    buf.push(0xFF);
    buf.push(0xFE);
    buf
}

pub(crate) fn commit_ack() -> Vec<u8> {
    vec![0x05, 0x00, 0x07, 0xF3, 0xFF]
}

pub(crate) fn comm_test_ack() -> Vec<u8> {
    vec![0x06, 0x00, 0x32, 0x01, 0x3B, 0xEB]
}

pub(crate) fn slot_reply(slot: u8, name: &str) -> Vec<u8> {
    // 26 bytes: 1A 00 <slot> <20 ASCII NUL-pad name> <flag1> <flag2> <chk_hi> <chk_lo>
    let mut name_bytes = name.as_bytes().to_vec();
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

pub(crate) fn meet_status_reply(time: &chrono::DateTime<chrono::Local>) -> Vec<u8> {
    // 51-byte frame parsed by `parse_selected_meet_reply`.
    // Fields: second/minute/hour/dow/month/day at bytes 8..14, year LE
    // at 40-41, length byte 0x33 at byte 0.
    let mut buf = vec![0u8; 51];
    buf[0] = 0x33; // length sentinel
    buf[8] = u8::try_from(time.second()).unwrap_or(0);
    buf[9] = u8::try_from(time.minute()).unwrap_or(0);
    buf[10] = u8::try_from(time.hour()).unwrap_or(0);
    buf[11] = u8::try_from(time.weekday().num_days_from_sunday() + 1).unwrap_or(0); // dow 1=Sun..7=Sat
    buf[12] = u8::try_from(time.month()).unwrap_or(0); // month
    buf[13] = u8::try_from(time.day()).unwrap_or(0); // day
    buf[14] = (u8::try_from(time.year() - 2000)).unwrap_or(0); // year offset from 2000, clamped to fit in a byte
    let year = time.year();
    buf[40] = (year & 0xFF) as u8;
    buf[41] = (year >> 8) as u8;
    buf[50] = 0xFB; // trailer marker
    buf
}

// ─── Wire I/O ──────────────────────────────────────────────────────────

/// Read one CTS6 frame from the stream. Frames are length-prefixed:
/// byte 0 is the total frame length. Returns `Ok(None)` on clean EOF.
pub(crate) fn read_frame(stream: &mut TcpStream) -> io::Result<Option<Vec<u8>>> {
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
