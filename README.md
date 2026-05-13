

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
