# How NFC Browser Bridge is tested

The bridge is tested at four levels:

1. **Automated tests:** they run on every push and before every release, against
   a simulated reader.
2. **Hardware tests:** they need a real reader on a Mac, and are required
   whenever reader code changes.
3. **The release checklist:** done by hand on real machines.
4. **A long-running test:** on a staging Mac mini, run on demand.

The macOS version is the tested one. The Windows version is built, and its
automated tests run on every push, but it **has not been tested with a real
reader** and won't be until a later release.

## Summary

| Level | What runs | Where | When |
| --- | --- | --- | --- |
| Automated, Rust | Format check, clippy, 74 tests incl. 26 end-to-end | GitHub Actions on macOS and Windows | Every push, and every release |
| Automated, TypeScript | Type check, 48 client library tests, 4 test-site tests | GitHub Actions on Linux | Every push, and every release |
| Hardware, Rust | 4 tests with a real reader | A Mac with a reader | When reader code changes |
| Hardware, client | Read/write/erase on a real tag | A Mac with a reader and a tag | When tag or protocol code changes |
| Release checklist | Install, replug, sleep, read/write, two tabs… | Real machines, by hand | Before major releases |
| Long-running | The latest release running for hours or days, read every minute | Staging Mac mini | On demand: `scripts/staging.sh start` / `stop` |

## 1. Automated tests

### Where they run

[`.github/workflows/ci.yml`](.github/workflows/ci.yml) runs on every push and
pull request:

- **Rust, on `macos-latest` and `windows-latest`:**
  - `cargo fmt --all --check`
  - `cargo clippy --workspace --all-targets -- -D warnings`: any warning
    fails the build.
  - `cargo test --workspace`
- **TypeScript, on Linux, for `client/` and `web/`:** `npm ci`,
  `npx tsc --noEmit` and `npm test` (vitest).

**Releases can't skip them.** [`release.yml`](.github/workflows/release.yml)
calls `ci.yml` as its `tests` job, on the exact commit being released. The
macOS and Windows installers build alongside it, but the `release` job that
signs and publishes needs `tests` to pass. A failing test means nothing is
published.

To run the same checks locally:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
(cd client && npm ci && npx tsc --noEmit && npm test)
(cd web && npm ci && npx tsc --noEmit && npm test)
```

### The simulated reader and USB

Most of the bridge talks to the operating system through two interfaces: the
smart-card service (PC/SC) and USB. The tests swap both for scriptable fakes in
[`crates/bridge-core/src/fake.rs`](crates/bridge-core/src/fake.rs):

- **`FakePcsc`** simulates the smart-card service. Tests can plug and unplug
  readers, place and lift cards, script card answers and errors, make calls
  hang, and stop or restart the service.
- **`FakeUsb`** simulates readers on USB. Tests can make a reader disappear
  from PC/SC while it stays on USB, and record USB resets.

Because the fakes behave the same on every platform, the Mac-only reader
recovery logic is also exercised on the Windows runner. Only the real USB
access (`usb_macos.rs`) and the real system log are left for the hardware
tests.

### End-to-end tests (`crates/bridge-core/tests/bridge.rs`)

These 26 tests start the whole bridge (monitor, worker, recovery, WebSocket
server) on top of the fakes and talk to it with a real WebSocket client, the
way a web page does.

| Area | Tests |
| --- | --- |
| Readers and cards | Cards already present are listed on connect; hot-plug and card events; polling the reader list when the OS has no plug-and-play notifications; a vanished reader doesn't spin the monitor; one handle is held while a card is present; a UID read is retried after a glitch |
| Reading and writing | Reads and writes go in one batch; stop-on-error, and a reset recovers a wedged card; a PC/SC error resets the card and reports progress; sharing violations are retried three times; card errors map to the right error codes |
| Smart-card service | Recovers after the service stops; persistent errors re-establish the context; a hung card call is reported as a stall |
| Security and protocol | Request validation and the write guard (NTAG pages 0–3); oversized messages close the connection; the Host header and path are checked |
| Clients | Two clients share events, and their batches don't interleave; the port being in use is reported |
| USB recovery (macOS logic) | USB readers and recovery state are reported; a reader lost by PC/SC is reset over USB; recovery gives up after two resets and starts over on replug; an unresponsive card is reset over USB without a new tap; "reset readers" resets every USB reader; a reader that stopped reporting cards is reset; USB errors that aren't a reader fault are ignored |

`an_unresponsive_card_is_reset_over_usb_without_a_new_tap` depends on timing
and sometimes fails on the Windows runner. Rerun the failed job; don't skip it.

### Unit tests

**`bridge-core`** (the bridge itself):

| File | What's tested |
| --- | --- |
| `protocol.rs` | Hex encoding, reader kinds, parsing requests, APDU validation, the write guard, how errors and status are sent |
| `recovery.rs` | The recovery ladder for a missing reader; a short gap isn't treated as trouble; a silent reader or an unresponsive card goes straight to a USB reset |
| `usb_log.rs` | Parsing the kernel's USB transfer error; a fault needs both the kernel and driver lines, in either order, with no teardown nearby; a reader going away isn't a fault; matching the reader by port, else by name |
| `usb.rs` | Matching USB readers to PC/SC names; describing a reader's identity for the log |
| `real.rs` | Error codes and state bits match the `pcsc` crate |
| `health.rs` | Only long calls are reported as stalls |
| `server.rs` | How connecting websites are labelled |

**`bridge-app`** (the tray app):

| File | What's tested |
| --- | --- |
| `install.rs` | The release key parses; signatures verify while copying; a signature from another key, or one naming another version or file, is rejected; the Windows setup command; swapping app folders; finding the bundle |
| `update.rs` | Version comparison; finding the installer and its signature in a release |
| `upload.rs` | Log file names; the upload folder name; machine names; reading the hardware UUID; large files keep their last whole lines; only log files are listed |
| `menu.rs` | Menu contents for ready-with-card, no reader, port conflict, an update on offer, and Check for updates |
| `autostart.rs` | Start-at-login entries, and refusing temporary locations |
| `icons.rs` | Icon sizes and states; on the wide macOS canvas the card fills the target height and all states match |

Three tests here are ignored by default because they reach outside the
machine or change real files. Run them by hand when that code changes:

- `install.rs`, `replaces_a_bundle_from_a_disk_image`: macOS.
- `update.rs`, `reaches_github_over_os_tls`
- `upload.rs`, `uploads_to_the_logs_bucket`: writes to the real logs bucket.

### TypeScript tests

**`client/`**, the library websites use (48 tests):

| File | What's tested |
| --- | --- |
| `client.test.ts` | Cards present at connect aren't reported as taps; reconnecting with backoff without replaying a present card; request timeouts scaled to batch size; error mapping; refusing another protocol version; two clients sharing one bridge; USB reader and recovery state; resetting readers |
| `ndef.test.ts` | URI and Text records (every prefix, UTF-8 and UTF-16), every TNF, long and multi-record messages, rejecting malformed messages, choosing the record a kiosk matches on |
| `ntag.test.ts` | Recognising Type 2 tags; reading blank and written tags in at most two batches; writing up to an NTAG216's capacity and never outside user memory; read-only tags; the ACR1552 write fallback; **a card lifted mid-write leaves an empty or valid tag**; write order |
| `tlv.test.ts` | Finding the NDEF TLV past padding and control TLVs, both length forms, cut-off headers, overruns |

These run against `client/src/fakeBridge.ts`, a fake bridge in the browser.

**`web/`**, the test site (4 tests): building records from the form, input
validation, counting bytes against the tag's capacity, and parsing typed APDUs.

## 2. Hardware tests (real reader, macOS)

The automated tests can't show that the real smart-card service, the real USB
stack and the macOS system log behave the way the fakes assume. That's what the
hardware tests are for. They're marked `#[ignore]` (or need `BRIDGE_HW=1`), so
CI never runs them.

**Rust**, in [`crates/bridge-core/tests/hardware.rs`](crates/bridge-core/tests/hardware.rs):

```sh
cargo test -p bridge-core --test hardware -- --ignored --test-threads=1 --nocapture
```

| Test | Needs | Checks |
| --- | --- | --- |
| `usb_reset_re_enumerates_the_reader_and_pcsc_drops_and_relists_it` | Reader | A real USB reset re-enumerates the reader, and PC/SC drops and lists it again |
| `reset_readers_brings_the_reader_back_with_its_tag_still_answering` | Reader and tag | After a reset, the reader comes back with its tag still answering, without a new tap |
| `a_usb_fault_in_the_system_log_resets_the_reader` | Reader and tag; someone to lift the tag and put it back within 60 s | A fault in the live system log leads to a reset, and taps work afterwards |
| `a_usb_reset_in_the_live_system_log_is_not_a_fault` | Reader | The log lines from a real reset aren't mistaken for a fault |

**Client**, with a bridge running and a tag on the reader:

```sh
cd client && BRIDGE_HW=1 npx vitest run test/hardware.test.ts
```

This test reads the tag, writes every record type, fills it to capacity, erases
it, and restores what was on it.

**When they're required.** Any change to reader code must pass the hardware
tests before it's committed as tested or released. CLAUDE.md lists which files
count and which test each one needs, and agents working on the repo enforce
this. If no reader is available, the change stays uncommitted until one is.

**Caution:** the Rust hardware tests reset every reader on USB. Never run them
on the production kiosk while it's in use.

The last full run (2026-11-06, macOS, ACR1255U-J1 and ACR1552U, NTAG215)
passed. [docs/HARDWARE.md](docs/HARDWARE.md) keeps the record of results.

## 3. Release checklist (by hand)

[docs/HARDWARE.md](docs/HARDWARE.md#release-checklist) has the manual
checklist. It covers:

- installing from the release files
- start at login
- unplugging and replugging the reader, and sleep and wake
- reading and writing every record type, including a full-capacity NTAG216
- lifting the card in the middle of a write
- using the bridge from two browser tabs
- quitting and relaunching while a page is open
- a reader left idle for 30 minutes
- the "Apps on device" permission in Chrome

Each check is marked done only after it has been seen working on a real
machine. The Windows column stays empty until Windows testing with real
readers begins.

## 4. Long-running test (staging Mac mini, on demand)

Some failures only show up after hours or days of running. The production
kiosk's reader once stopped reporting tags and stayed that way for 17.5 hours.
A staging run puts the bridge through the same conditions as the kiosk for as
long as you leave it on. **It runs only when asked**, and releases don't wait
for it.

- **Machine:** the Mac mini `Mac-mini.local` (macOS user `User`, Apple
  silicon), which uploads as `Mac-mini-F6E28B59`. It isn't the production
  kiosk. Its address comes from DHCP (192.168.1.141 on 2026-11-08). It has an
  ACR1255U-J1, the kiosk's model, with a tag left on it.
- **What a run tests:** the bridge installed there, which is the latest
  release (it updates itself like the kiosk), with the kiosk app running in
  Chrome against it as in production.
- **The probe:** [`staging-probe`](crates/bridge-app/src/bin/staging-probe.rs)
  runs during a run, under a LaunchAgent, so it survives restarts. It stays
  connected to the bridge like the kiosk, and every minute it reads the tag
  (its UID and pages 4–7). It logs every result and every bridge event
  (recoveries, the tag coming and going, lost connections) to
  `~/Library/Logs/NFCBrowserBridgeStaging/run.log`.
- **The summary:** stopping a run summarises it. The summary and the run's log
  are uploaded, with the bridge's own logs for those days, to the logs bucket
  under `nfc-browser-bridge/Mac-mini-F6E28B59/`. The verdict is:
  - **FAIL:** a read failed, the bridge was unreachable, recovery gave up on
    the reader, or no reads happened.
  - **WARN:** every read worked, but the bridge had to recover the reader, the
    connection dropped, or the tag came and went.
  - **PASS:** none of that.

  It also gives the read count (and how many were expected), failures by
  reason, read times and events.
- **What it can't catch directly:** with the tag resting on the reader there
  are no new taps, so a reader that stops reporting *new* taps (the 2026-11-06
  fault) doesn't fail a read. The bridge spots that fault in the macOS system
  log and resets the reader, and the summary shows it as recovery events
  (WARN).

**Running it**, from a developer Mac with SSH key access to the Mac mini:

```sh
scripts/staging.sh start      # start a run; it keeps going until stopped
scripts/staging.sh status     # is a run on, reads so far, latest entries
scripts/staging.sh stop       # stop, upload, and print the summary
scripts/staging.sh results    # the latest summary from the bucket (gcloud)
```

`STAGING_HOST` picks the Mac mini if `user@Mac-mini.local` doesn't reach it.

**Setting it up** (once, or after changing the probe):

1. On the Mac mini, install the bridge from the latest release (README), with
   **Start at login** on. Put the reader on USB with a tag on it, and set up
   the kiosk app in Chrome as on the production kiosk.
2. Give your Mac SSH key access to the Mac mini. Run this in Terminal, because
   macOS's Local Network privacy can block other apps, and use the IP address:

   ```sh
   ssh-copy-id -i ~/.ssh/id_rsa.pub user@192.168.1.141
   ```

3. Build and copy the probe (it restarts a run in progress):

   ```sh
   scripts/staging.sh install
   ```

The bucket's rule that lets the probe add `probe.<date>.log` and
`summary.<date>.txt` lives in the logs-bucket repo.

## What isn't tested

- **Windows with real readers:** not until a later release. On Windows only the
  automated tests run, against the simulated reader.
- **How the menu bar icon and menu look on screen:** the tests check what's
  drawn and listed, not the rendered result. Look at them in the real menu bar
  after changing them.
- **Readers other than the ACR1255U-J1 and the ACR1552U, and drivers other than
  the one built into macOS.**
