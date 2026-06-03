use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::protocol::{
    build_event_response, comm_test_ack, commit_ack, finalize_ack, identify_reply, load_ack,
    meet_status_reply, nack, no_data_reply, parse_request, read_frame, slot_reply, Request, Verb,
};
use crate::state::{LoadedEvent, LoadingMeet, State, NUM_SLOTS};
use crate::{emu_log, hex};

pub(crate) fn handle_client(mut stream: TcpStream, state: &Arc<Mutex<State>>) {
    let peer = stream
        .peer_addr()
        .map_or_else(|_| "?".into(), |a| a.to_string());
    emu_log!("[net] client connected: {peer}");
    let _ = stream.set_read_timeout(Some(Duration::from_mins(1)));

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
            Request::CommTest => {
                emu_log!("[net] comm-test wake from {peer} -> ack");
                Some(comm_test_ack())
            }
            Request::Slot(s) => {
                let st = state.lock().unwrap();
                let name = if s >= 1 && (s as usize) <= NUM_SLOTS {
                    st.slots[(s - 1) as usize].name.clone()
                } else {
                    String::new()
                };
                emu_log!("[slot-list] Rs slot={s} -> \"{name}\"");
                Some(slot_reply(s, &name))
            }
            Request::MeetStatus => {
                let st = state.lock().unwrap();
                Some(meet_status_reply(&st.meet_date))
            }
            Request::Fetch { verb, heat, event } => {
                let mut s = state.lock().unwrap();
                let race = match verb {
                    Verb::Latest => s.lookup_latest(event, heat),
                    Verb::Previous => s.lookup_previous(event, heat),
                    Verb::Next => s.lookup_next(event, heat),
                    Verb::ByRace => None,
                };
                race.map(build_event_response)
            }
            Request::FetchByRace { race } => {
                let s = state.lock().unwrap();
                s.lookup_by_race(race).map(build_event_response)
            }
            Request::LoadMeetName { slot, name } => {
                let mut s = state.lock().unwrap();
                emu_log!("[meet-load] Is slot={slot} name=\"{name}\"");
                s.loading = Some(LoadingMeet {
                    slot,
                    name,
                    ..LoadingMeet::default()
                });
                Some(load_ack())
            }
            Request::LoadCategoryLabel {
                category,
                idx,
                label,
            } => {
                let mut s = state.lock().unwrap();
                if s.loading.is_none() {
                    s.loading = Some(LoadingMeet::default());
                }
                if let Some(l) = s.loading.as_mut() {
                    let table: Option<&mut Vec<String>> = match category {
                        0x00 => Some(&mut l.genders),
                        0x01 => Some(&mut l.ages),
                        0x02 => Some(&mut l.strokes),
                        0x03 => Some(&mut l.rounds),
                        _ => None,
                    };
                    if let Some(table) = table {
                        let i = idx as usize;
                        if table.len() <= i {
                            table.resize(i + 1, String::new());
                        }
                        table[i] = label;
                    } else {
                        emu_log!("[meet-load] It unknown category=0x{category:02X} idx={idx}");
                    }
                }
                Some(load_ack())
            }
            Request::LoadEventRecord {
                slot,
                event_num,
                distance,
                gender_idx,
                stroke_idx,
                is_relay,
                is_clear,
            } => {
                let mut s = state.lock().unwrap();
                if s.loading.is_none() {
                    s.loading = Some(LoadingMeet {
                        slot,
                        ..LoadingMeet::default()
                    });
                }
                if !is_clear {
                    if let Some(l) = s.loading.as_mut() {
                        l.events.push(LoadedEvent {
                            event_num,
                            distance,
                            gender_idx,
                            stroke_idx,
                            is_relay,
                        });
                    }
                }
                Some(load_ack())
            }
            Request::FinalizeSlot { slot } => {
                let mut s = state.lock().unwrap();
                emu_log!("[meet-load] Ir finalize slot={slot}");
                if let Some(l) = s.loading.as_ref() {
                    if l.slot != 0 && l.slot != slot {
                        emu_log!(
                            "[meet-load] Ir slot mismatch (loading slot={}, finalize slot={slot}); applying anyway",
                            l.slot
                        );
                    }
                }
                s.apply_loading_meet();
                Some(finalize_ack())
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

pub(crate) fn run_server(addr: &str, state: &Arc<Mutex<State>>) -> io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    emu_log!("[net] listening on {addr}");
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let s = Arc::clone(state);
                thread::spawn(move || handle_client(stream, &s));
            }
            Err(e) => emu_log!("[net] accept error: {e}"),
        }
    }
    Ok(())
}
