```
  _____     __                 __      
 / ___/__  / /__  _______ ____/ /__    
/ /__/ _ \/ / _ \/ __/ _ `/ _  / _ \   
\___/\___/_/\___/_/  \_,_/\_,_/\___/   
         _______       _               
        /_  __(_)_ _  (_)__  ___ _     
         / / / /  ' \/ / _ \/ _ `/     
        /_/ /_/_/_/_/_/_//_/\_, /      
              ____         /___/       
             / __/_ _____ / /____ __ _ 
            _\ \/ // (_-</ __/ -_)  ' \
           /___/\_, /___/\__/\__/_/_/_/
               /___/
   ____                  __            
  / __/_ _  __ __/ /__ _/ /____  ____  
 / _//  ' \/ // / / _ `/ __/ _ \/ __/  
/___/_/_/_/\_,_/_/\_,_/\__/\___/_/     

Colorado Timing System Emulator
                                       
```
                                      

https://github.com/user-attachments/assets/be74ab5c-8ab0-4a37-8bfb-6a23b395cb57

# cts6-emulator

An independent, clean-room emulator of the wire protocol used by the
Colorado Time Systems CTS6 timing console. Useful for developing and
testing meet-management software (timing integrations, scoreboards,
display boards, results pipelines) without needing physical hardware
or a pool deck.

The emulator presents a small terminal UI (ratatui + crossterm) for
driving a synthetic meet — configure a lineup, start a heat, tap
lanes to record splits and finishes, mark DQs — while serving the
CTS6 wire protocol to clients over **TCP on `127.0.0.1:1337`**.

## Commands

The TUI has three panes — scoreboard + stored events tree on top, log
in the middle, command input at the bottom (`cts6>` prompt). Press
**F1** any time for the in-app help popup; **Tab** moves focus
between the input and the stored-events tree.

### Meet setup

| Command | What it does |
| --- | --- |
| `/event N` | Set the active event number. |
| `/heat N` | Set the active heat number. |
| `/race N` | Override the next race counter (resume mid-meet). Only valid when no race is in progress. |
| `/lanes A..B` | Set the active lane spread (within `1..=10`). E.g. `/lanes 1..10`, `/lanes 2..8`, `/lanes 1..1`. |
| `/lineup show` | List the configured event lineup. |
| `/lineup preset hs` | Load the standard NFHS high-school dual-meet lineup (12 events). |
| `/lineup add D G S [Y]` | Append an event: distance (yd), gender (`M`/`F`/`X`), stroke (see below), optional split-yards (default 50). |
| `/lineup clear` | Remove all events. |

**Stroke codes** (any form is accepted, case-insensitive):

| # | Code | Long form |
| --- | --- | --- |
| 1 | `FR` | `FREE`, `FREESTYLE` |
| 2 | `BK` | `BACK`, `BACKSTROKE` |
| 3 | `BR` | `BREAST`, `BREASTSTROKE` |
| 4 | `FL` | `FLY`, `BUTTERFLY` |
| 5 | `IM` | `MEDLEY` |
| 6 | `MED-R` | `MEDLEY-RELAY` |
| 7 | `FR-R` | `FREE-RELAY`, `FREESTYLE-RELAY` |
| – | `DV` | `DIVE`, `DIVING` |

> Bare `FR` always means **freestyle** (code 1). Free relay must be
> spelled `FR-R` (or `7`) so it's never confused with `FR`.

### Running a race

| Command | What it does |
| --- | --- |
| `<enter>` (empty input) | Start the race. Timestamp resets to `0.000`. |
| `N` | Touch lane `N` (within the active spread). Each touch records a cumulative split; the final touch is the lane's finish. |
| `1 3 5` | Batch — multiple lane touches in a single line. |
| `/dq L` | Mark lane `L` as DQ for the in-progress race. |
| `/` or `/print` | Finalize: places assigned by ascending finish time; lanes auto-bumped to next heat. |

While a race is running, lanes that have hit their expected total
touch count are marked **FINAL** on the scoreboard and ignore further
touches (mirrors a real CTS6 lane disarming itself). Place is shown
only once a lane goes FINAL.

### Inspection

| Command | What it does |
| --- | --- |
| `/races` | List all stored races. |
| `/status` | Print the current event/heat/race counters and lane spread. |
| `/help` or `/?` or `F1` | Open the help popup. |
| `/quit` or `/exit` | Quit the emulator. |

### Stored events tree

Press **Tab** to focus the right-hand stored-events pane:

| Key | What it does |
| --- | --- |
| `↑` / `↓` | Move selection. |
| `→` / `Enter` (on Event/Heat row) | Expand. |
| `←` (on Event/Heat row) | Collapse. |
| `←` (on Race row) | Collapse parent heat and jump up to it. |
| `Enter` (on Race row) | Open the per-lane results popup (place, finish time, every cumulative split). |
| `Home` / `End` | Jump to first / last visible row. |
| `Tab` / `Esc` | Return focus to the command input. |

Any key dismisses an open modal popup (help or results); `Ctrl-C` /
`Ctrl-D` quits the app from anywhere.

## Reverse-engineering disclaimer

This project is an independent reimplementation. It is **not affiliated
with, endorsed by, or sponsored by Colorado Time Systems**. "CTS",
"CTS6", and any related marks are the property of their respective
owners. Use of those names here is purely descriptive ("compatible
with the CTS6 protocol") and does not imply any relationship.

### How the protocol was obtained

The protocol was inferred entirely from **passive observation of
serial traffic** between unmodified Colorado Time Systems hardware
and downstream meet-management software. Specifically:

- A **DB9 breakout / pinout board** was placed inline on the RS-232
  serial link between the CTS6 console and the receiving PC.
- Bytes were captured with a logic analyzer / serial sniffer while
  real meets were run, then correlated against the visible scoreboard
  state (lane touches, splits, DQs, heat/event transitions).
- No firmware was decompiled, disassembled, or otherwise modified.
- No proprietary source code, schematics, or NDA-protected
  documentation was consulted. Only publicly distributed vendor
  manuals and observed wire bytes were used.

This kind of black-box, observation-based reverse engineering for the
purpose of **interoperability** is expressly permitted under
17 U.S.C. § 1201(f) (US) and Article 6 of the EU Software Directive
(2009/24/EC), and is supported by precedent including *Sega v.
Accolade* and *Sony Computer Entertainment v. Connectix*.

### Transport difference vs. the original hardware

The genuine CTS6 console speaks its protocol over an **RS-232 serial
link** (DB9). This emulator carries the **same byte-level protocol
over TCP** (`127.0.0.1:1337` by default) so that:

- Multiple software clients can connect simultaneously without
  needing physical or virtual COM ports.
- Tests can run on CI / containers / any host with a TCP stack.
- No serial drivers, USB-to-serial adapters, or null-modem cables
  are required during development.

If you need the wire format on an actual serial port, pipe the TCP
stream through `socat` (or equivalent), e.g.:

```sh
socat TCP:127.0.0.1:1337 PTY,link=/tmp/cts6,raw,echo=0
```

The protocol bytes are identical; only the framing transport
changes.

## License

Apache-2.0. See [LICENSE](LICENSE).
