# Hardware notes and checklist

Results of the Step 0 spike and the release checklist. Fill in the blanks as
each check is run; mark a check done only after it has been seen working on the
real machine.

Tools:

- `cargo run -p bridge-core --example spike`: lists readers, waits for cards,
  prints the ATR and the answers to `FF CA`, `FF B0 00 00 10`, `FF B0 00 03 04`.
- `cargo run -p bridge-core --example idle -- "<reader name>"`: measures how
  long a card may sit idle before `SCardConnect` fails (`hold` as a second
  argument keeps a handle open while measuring).
- `cargo run -p bridge-core --example serve`: the bridge without the tray.
- `cd client && BRIDGE_HW=1 npx vitest run test/hardware.test.ts`: read, write,
  full-capacity write, erase and restore on a real tag through a running bridge.

## Spike results

### macOS (Mac, macOS 27, Apple silicon), 2026-10-13

| Question | ACR1255U-J1 (USB) | ACR1552U |
| --- | --- | --- |
| PC/SC reader name(s) | `ACS ACR1255U-J1` (one slot, no PICC/SAM suffix) | _not yet tested_ |
| Driver | Apple's bundled CCID driver (`ifd-ccid.bundle`) works; no ACS driver needed | _not yet tested_ |
| HID + CCID together | n/a | _not yet tested_ |
| UID format | `FF CA` → `04699E22C01C90` (7 bytes, 14 hex chars) | _not yet tested_ |
| Idle behaviour | see "Idle card power-down" below | _not yet tested_ |

NTAG215 seen: ATR `3B8F8001804F0CA0000003060300030000000068` (PC/SC Part 3
card name `00 03`, so `isType2` accepts it), CC `E1 10 3E 00`.

Other observations:

- **No PnP notifications on macOS.** `\\?PnP?\Notification` comes back as
  `SCARD_STATE_UNKNOWN`. The monitor drops it and relies on re-listing readers
  every second (the 1 s `SCardGetStatusChange` timeout).
- **Idle card power-down.** With no PC/SC handle open, macOS powers the card
  down about 8 s after the last disconnect (measured: connect works after 3, 5
  and 7 s idle, fails after 9 and 12 s). The next `SCardConnect` then fails with
  `SCARD_E_NO_SMARTCARD`, the reader reports the card removed, and about 0.5 s
  later present again: a false "re-tap". With one shared handle held open, a
  second connect still works after 12 s and 20 s. The bridge's worker therefore
  holds one idle shared handle per present card from the UID read until the
  card leaves. Batches still use their own connection.
- Timings through the bridge (NTAG215, ACR1255U-J1, debug build): multi-record
  write 20 pages ≈ 160 ms; full-capacity write (492-byte message, 125 pages)
  ≈ 950 ms; full read ≈ 140 ms.

### Windows

_Not yet run. Please run the spike on Windows with both readers and record:_

| Question | ACR1255U-J1 | ACR1552U |
| --- | --- | --- |
| PC/SC reader name(s), incl. PICC and SAM slots | | |
| Built-in `usbccid` driver enough, or ACS driver needed? | | |
| HID + CCID mode: keyboard output and PC/SC both work? | | |
| `FF CA` UID identical to what keyboard mode types? | | |
| Smart Card Plug and Play pop-ups or sharing violations? | | |
| ACR1255U-J1: sleeps or powers off when idle on USB? Prefers USB over Bluetooth? | | |
| Does an idle card stay powered (see macOS power-down above)? | | |

If Smart Card Plug and Play causes "installing driver" pop-ups or sharing
violations, turn it off with
`HKLM\SOFTWARE\Policies\Microsoft\Windows\ScPnP\EnableScPnP = 0` (DWORD) and
document it in the README.

## Release checklist

Run on macOS and Windows, with the ACR1552U and the ACR1255U-J1, and with
NTAG213, NTAG215 and NTAG216 cards.

| # | Check | macOS | Windows |
| --- | --- | --- | --- |
| 1 | Install from the Release files; "Start at login" works after signing out and in | | |
| 2 | Unplug and replug the reader while the bridge runs; sleep and wake the machine | | |
| 3 | Read, write and verify every record type, incl. a full-capacity NTAG216 write | | |
| 4 | Lift the card in the middle of a write: the tag still reads as empty or valid | | |
| 5 | Use the bridge from two browser tabs at once | | |
| 6 | Quit and relaunch the bridge with the page open: the client reconnects and does not replay "card present" | | |
| 7 | Leave the ACR1255U-J1 idle for 30 minutes, then tap | | |
| 8 | GitHub Pages site in Chrome, incl. the "Apps on device" permission prompt | | |

Partial results so far (macOS, ACR1255U-J1, NTAG215 only, built from source):
read/write/full-capacity/erase verified with the hardware test; tray menu,
icon states and the "already running" check seen working.
