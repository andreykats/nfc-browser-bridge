//! USB reset against a real reader (macOS). Ignored by default: plug in a reader, put a tag on it,
//! then run
//!
//! `cargo test -p bridge-core --test hardware -- --ignored --test-threads=1 --nocapture`
//!
//! The tests reset every smart-card reader on USB, so run them one at a time and not on a kiosk
//! that is in use.

#![cfg(target_os = "macos")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use bridge_core::backend::{PcscBackend, PcscContext, PcscError, ReaderWatch, state};
use bridge_core::real::RealPcsc;
use bridge_core::recovery::RecoveryConfig;
use bridge_core::usb::{self, UsbReader};
use bridge_core::usb_log::{self, UsbFaultLog};
use bridge_core::{Bridge, BridgeConfig};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// How long a reset reader may take to be back on USB and in the smart-card service.
const BACK_WITHIN: Duration = Duration::from_secs(20);
/// Once back, the reader must stay ready this long: no second drop-out.
const STABLE_FOR: Duration = Duration::from_secs(3);
/// Longest wait for a change event; USB is checked in between. The service drops a reset reader
/// for only ~20 ms, so the test waits on its events rather than polling the reader list.
const WAIT: Duration = Duration::from_millis(50);

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn usb_readers() -> Vec<UsbReader> {
    let readers = usb::system().readers().expect("macOS can see USB");
    assert!(
        !readers.is_empty(),
        "no smart-card reader on USB: plug one in"
    );
    readers
}

/// Usable: empty or holding a card, and neither unknown nor unavailable.
fn is_ready(s: u32) -> bool {
    s & (state::UNKNOWN | state::UNAVAILABLE) == 0 && s & (state::EMPTY | state::PRESENT) != 0
}

#[test]
#[ignore = "needs a USB smart-card reader"]
fn usb_reset_re_enumerates_the_reader_and_pcsc_drops_and_relists_it() {
    let usb = usb::system();
    let ctx = RealPcsc.establish().expect("establish a PC/SC context");
    for reader in usb_readers() {
        eprintln!("{} (id {:#x}): {}", reader.name, reader.id, reader.identity);
        let names = ctx.list_readers().expect("list PC/SC readers");
        let name = names.iter().find(|n| reader.matches(n)).unwrap_or_else(|| {
            panic!(
                "{} is on USB but the smart-card service does not list it: {names:?}",
                reader.name
            )
        });
        // UNAWARE: the first call returns the current state at once.
        let mut watch = [ReaderWatch::new(name.as_str())];
        ctx.get_status_change(Duration::ZERO, &mut watch)
            .expect("read the reader's state");
        watch[0].sync();
        eprintln!("  {name}: {:#06x}", watch[0].current_state & 0xFFFF);
        assert!(is_ready(watch[0].current_state), "{name} is not ready");

        let started = Instant::now();
        usb.reset(&reader).expect("USB reset");
        eprintln!("  reset returned after {:?}", started.elapsed());

        // USB is watched on its own thread: listing it takes long enough to miss the service's
        // ~20 ms drop-out if it ran between the waits below.
        let watcher = {
            let (usb, reader) = (usb.clone(), reader.clone());
            std::thread::spawn(move || {
                while started.elapsed() < BACK_WITHIN {
                    if let Some(r) = usb
                        .readers()
                        .unwrap_or_default()
                        .into_iter()
                        .find(|r| r.name == reader.name && r.id != reader.id)
                    {
                        eprintln!("  {:?}: on USB again as id {:#x}", started.elapsed(), r.id);
                        return Some(r.id);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                None
            })
        };
        let (mut left, mut back): (Option<Duration>, Option<Instant>) = (None, None);
        while started.elapsed() < BACK_WITHIN
            && !(watcher.is_finished() && back.is_some_and(|b| b.elapsed() >= STABLE_FOR))
        {
            match ctx.get_status_change(WAIT, &mut watch) {
                Ok(()) => {
                    let s = watch[0].event_state;
                    watch[0].sync();
                    eprintln!("  {:?}: {name}: {:#06x}", started.elapsed(), s & 0xFFFF);
                    if !is_ready(s) {
                        left.get_or_insert(started.elapsed());
                        back = None;
                    } else if left.is_some() {
                        back.get_or_insert_with(Instant::now);
                    }
                }
                Err(e) if e == PcscError::TIMEOUT => {}
                Err(e) => panic!("waiting for {name} failed: {e}"),
            }
        }
        let new_id = watcher.join().expect("USB watcher");
        assert!(
            new_id.is_some(),
            "{} did not re-enumerate on USB within {BACK_WITHIN:?}",
            reader.name
        );
        assert!(
            left.is_some(),
            "the smart-card service never dropped {name}: the reset did not reach it"
        );
        assert!(
            back.is_some_and(|b| b.elapsed() >= STABLE_FOR),
            "{name} is not ready again within {BACK_WITHIN:?}"
        );
    }
}

async fn connect(port: u16) -> Ws {
    let mut req = format!("ws://127.0.0.1:{port}/v1")
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("origin", HeaderValue::from_static("https://example.com"));
    tokio_tungstenite::connect_async(req).await.unwrap().0
}

/// Next JSON message, or panic once `deadline` passes.
async fn recv(ws: &mut Ws, deadline: Instant) -> Value {
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let msg = tokio::time::timeout(left, ws.next())
            .await
            .expect("timed out waiting for the bridge")
            .expect("socket closed")
            .expect("socket error");
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

async fn request(ws: &mut Ws, req: Value) -> Value {
    let id = req["id"].clone();
    ws.send(Message::text(req.to_string())).await.unwrap();
    let deadline = Instant::now() + BACK_WITHIN;
    loop {
        let v = recv(ws, deadline).await;
        if v["id"] == id {
            return v;
        }
    }
}

/// The reader in a `hello`/`readers` message with a tag on it, as (name, UID).
fn reader_with_card(v: &Value) -> Option<(String, String)> {
    v["readers"].as_array()?.iter().find_map(|r| {
        let uid = r["card"]["uid"].as_str()?;
        Some((r["name"].as_str()?.to_string(), uid.to_string()))
    })
}

async fn read_uid(ws: &mut Ws, id: u32, reader: &str) -> Value {
    request(
        ws,
        json!({"id": id, "type": "transmit", "reader": reader, "apdus": ["FFCA000000"]}),
    )
    .await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a USB smart-card reader with a tag on it"]
async fn reset_readers_brings_the_reader_back_with_its_tag_still_answering() {
    let config = BridgeConfig {
        port: 0,
        version: "9.9.9".into(),
        recovery: RecoveryConfig::default(),
    };
    let bridge = Bridge::start(RealPcsc, config, None).expect("start the bridge");
    let mut ws = connect(bridge.local_addr().port()).await;

    // The tag on the reader, from hello or the first readers events.
    let find_tag = async {
        loop {
            let v = recv(&mut ws, Instant::now() + BACK_WITHIN).await;
            if let Some(found) = reader_with_card(&v) {
                break found;
            }
        }
    };
    let (reader, uid) = tokio::time::timeout(Duration::from_secs(10), find_tag)
        .await
        .expect("no tag on the reader: put one on it");
    eprintln!("{reader}: tag {uid}");
    let before = read_uid(&mut ws, 1, &reader).await;
    assert_eq!(
        before["ok"], true,
        "the tag does not answer before the reset: {before}"
    );

    let started = Instant::now();
    ws.send(Message::text(
        json!({"id": 2, "type": "reset_readers"}).to_string(),
    ))
    .await
    .unwrap();
    // Until the reader is listed again with the tag on it. The tag never left, so it must not be
    // announced as a new tap.
    let deadline = started + BACK_WITHIN;
    let (mut response, mut reset_event, mut gone, mut back) = (None, false, false, false);
    while response.is_none() || !reset_event || !back {
        let v = recv(&mut ws, deadline).await;
        let has_card = reader_with_card(&v).is_some_and(|(r, _)| r == reader);
        match v["event"].as_str() {
            Some("card_present") => panic!("the tag was announced again as a new tap: {v}"),
            Some("recovery") if v["action"] == "usb_reset" => reset_event = true,
            Some("card_removed") => gone = true,
            Some("readers") if !has_card => gone = true,
            Some("readers") if gone && has_card => {
                eprintln!("  back with its tag after {:?}", started.elapsed());
                back = true;
            }
            _ if v["id"] == 2 => response = Some(v),
            _ => {}
        }
    }
    let response = response.unwrap();
    assert_eq!(response["ok"], true, "{response}");
    let reset: Vec<&str> = response["result"]["reset"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        reset.iter().any(|r| reader.starts_with(r)),
        "{reader} was not reset: {reset:?}"
    );

    let after = read_uid(&mut ws, 3, &reader).await;
    assert_eq!(
        after["ok"], true,
        "the tag does not answer after the reset: {after}"
    );
    assert_eq!(after["result"]["responses"], before["result"]["responses"]);
    let status = bridge.status();
    assert_eq!(
        serde_json::to_value(&status).unwrap()["recovery"],
        "idle",
        "recovery stepped in after a requested reset"
    );
    bridge.shutdown();
}

/// The macOS system log's lines when the USB driver stopped reading from the production reader
/// (2026-11-06), for `reader` instead.
fn usb_fault_lines(reader: &UsbReader) -> [String; 2] {
    let location = reader
        .identity
        .location
        .expect("macOS gives the reader's port");
    // The kernel names the device by its USB product string, without the vendor.
    let product = reader
        .name
        .split_once(' ')
        .map_or(reader.name.as_str(), |(_, p)| p);
    [
        format!(
            "2026-11-05 19:47:46.571 E  kernel[0:a2af] [com.apple.usb:all] AppleUSBXHCIRequest: AppleUSBIORequest::complete: device 1 ({product}@{location:08x}) endpoint 0x83: status 0xe00002ed (transaction error): 0 bytes transferred"
        ),
        "2026-11-05 19:47:46.576 E  usbsmartcardreaderd[232:bc6] (IOUSBHost) Error: Unable to send IO. with reason: (null)".into(),
    ]
}

/// What the production kiosk needed on 2026-11-06, short of the fault itself (which cannot be
/// caused on purpose): the driver's errors in the system log make the bridge reset that reader, and
/// the reader comes back with its tag still answering and later taps reported.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs a USB smart-card reader with a tag on it"]
async fn a_usb_fault_in_the_system_log_resets_the_reader() {
    let faults = Arc::new(UsbFaultLog::default());
    let config = BridgeConfig {
        port: 0,
        version: "9.9.9".into(),
        recovery: RecoveryConfig {
            faults: faults.clone(),
            ..RecoveryConfig::default()
        },
    };
    let bridge = Bridge::start(RealPcsc, config, None).expect("start the bridge");
    let mut ws = connect(bridge.local_addr().port()).await;
    let find_tag = async {
        loop {
            let v = recv(&mut ws, Instant::now() + BACK_WITHIN).await;
            if let Some(found) = reader_with_card(&v) {
                break found;
            }
        }
    };
    let (reader, uid) = tokio::time::timeout(Duration::from_secs(10), find_tag)
        .await
        .expect("no tag on the reader: put one on it");
    let usb_reader = usb_readers()
        .into_iter()
        .find(|u| u.matches(&reader))
        .expect("the reader with the tag is on USB");
    eprintln!("{reader}: tag {uid}; USB {}", usb_reader.identity);
    let before = read_uid(&mut ws, 1, &reader).await;
    assert_eq!(before["ok"], true, "the tag does not answer: {before}");

    let started = Instant::now();
    for line in usb_fault_lines(&usb_reader) {
        faults.feed(&line);
    }
    let deadline = started + BACK_WITHIN;
    let (mut reset, mut gone, mut back, mut recovered) = (false, false, false, false);
    while !(reset && back && recovered) {
        let v = recv(&mut ws, deadline).await;
        let has_card = reader_with_card(&v).is_some_and(|(r, _)| r == reader);
        match v["event"].as_str() {
            Some("card_present") => panic!("the tag was announced again as a new tap: {v}"),
            Some("recovery") if v["action"] == "usb_reset" => {
                eprintln!("  reset after {:?}: {}", started.elapsed(), v["reason"]);
                assert!(
                    v["readers"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|r| r == usb_reader.name.as_str()),
                    "{} was not reset: {v}",
                    usb_reader.name
                );
                reset = true;
            }
            Some("recovery") if v["action"] == "recovered" => recovered = true,
            Some("recovery") => panic!("unexpected recovery step: {v}"),
            Some("card_removed") => gone = true,
            Some("readers") if !has_card => gone = true,
            Some("readers") if gone && has_card => {
                eprintln!("  back with its tag after {:?}", started.elapsed());
                back = true;
            }
            _ => {}
        }
    }
    let after = read_uid(&mut ws, 2, &reader).await;
    assert_eq!(
        after["ok"], true,
        "the tag does not answer after the reset: {after}"
    );
    assert_eq!(after["result"]["responses"], before["result"]["responses"]);

    eprintln!("  lift the tag off the reader and put it back within 60 s");
    let deadline = Instant::now() + Duration::from_secs(60);
    let (mut lifted, mut tapped) = (false, false);
    while !tapped {
        let v = recv(&mut ws, deadline).await;
        lifted |= v["event"] == "card_removed" && v["reader"] == reader.as_str();
        tapped = lifted && v["event"] == "card_present" && v["reader"] == reader.as_str();
    }
    assert_eq!(
        serde_json::to_value(bridge.status()).unwrap()["recovery"],
        "idle"
    );
    bridge.shutdown();
}

/// Feeds the live system log into `faults` until dropped. Only the lines `usb_log` looks at.
struct LiveLog(std::process::Child);

impl LiveLog {
    fn start(faults: Arc<UsbFaultLog>) -> Self {
        use std::io::BufRead;
        let mut child = std::process::Command::new("/usr/bin/log")
            .args([
                "stream",
                "--style",
                "compact",
                "--level",
                "info",
                "--predicate",
                r#"(process == "kernel" AND senderImagePath CONTAINS "IOUSBHost") OR process == "usbsmartcardreaderd""#,
            ])
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("run log stream");
        let out = std::io::BufReader::new(child.stdout.take().unwrap());
        std::thread::spawn(move || {
            for line in out.lines().map_while(Result::ok) {
                // The lines that matter, for the test output.
                if ["AppleUSBIORequest", "Unable to send IO", "terminate"]
                    .iter()
                    .any(|k| line.contains(k))
                {
                    eprintln!("  log: {}", line.get(..200).unwrap_or(&line));
                }
                faults.feed(&line);
            }
        });
        // log stream misses what is logged before it is connected.
        std::thread::sleep(Duration::from_secs(1));
        Self(child)
    }
}

impl Drop for LiveLog {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A USB reset logs the same two errors as the fault (ACR1255U-J1: transaction error on endpoint
/// 0x83 and "Unable to send IO") along with the device's teardown; it must not count as a fault.
#[test]
#[ignore = "needs a USB smart-card reader"]
fn a_usb_reset_in_the_live_system_log_is_not_a_fault() {
    let usb = usb::system();
    let faults = Arc::new(UsbFaultLog::default());
    let _live = LiveLog::start(faults.clone());
    for reader in usb_readers() {
        eprintln!("{} (id {:#x}): {}", reader.name, reader.id, reader.identity);
        usb.reset(&reader).expect("USB reset");
    }
    std::thread::sleep(usb_log::TEARDOWN_WINDOW + Duration::from_secs(2));
    let found = faults.take();
    assert!(found.is_empty(), "a reset was taken for a fault: {found:?}");
}
