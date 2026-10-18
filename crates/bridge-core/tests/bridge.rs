//! End-to-end tests: FakePcsc -> monitor/worker -> WebSocket server -> a real WebSocket client.

use std::time::Duration;

use bridge_core::backend::{Disposition, PcscError};
use bridge_core::fake::{ATR_ULTRALIGHT, FakeCard, FakePcsc, FaultOp, NtagType};
use bridge_core::{Bridge, BridgeConfig};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const R1: &str = "ACS ACR1552 1S CL Reader PICC 0";
const R2: &str = "ACS ACR1255U-J1";
const UID: [u8; 7] = [0x04, 0x69, 0x9E, 0x22, 0xC0, 0x1C, 0x90];
const UID_HEX: &str = "04699E22C01C90";

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

fn start(fake: &FakePcsc) -> Bridge<FakePcsc> {
    Bridge::start(
        fake.clone(),
        BridgeConfig {
            port: 0,
            version: "9.9.9".into(),
        },
        None,
    )
    .unwrap()
}

async fn connect(bridge: &Bridge<FakePcsc>) -> Ws {
    connect_with(bridge, None, "/v1").await.unwrap()
}

async fn connect_with(
    bridge: &Bridge<FakePcsc>,
    host: Option<&str>,
    path: &str,
) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
    let port = bridge.local_addr().port();
    let mut req = format!("ws://127.0.0.1:{port}{path}")
        .into_client_request()
        .unwrap();
    if let Some(h) = host {
        req.headers_mut()
            .insert("host", HeaderValue::from_str(h).unwrap());
    }
    req.headers_mut()
        .insert("origin", HeaderValue::from_static("https://example.com"));
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
}

/// Next JSON message (skipping control frames), or panic after 5 s.
async fn recv(ws: &mut Ws) -> Value {
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("timed out waiting for a message")
            .expect("socket closed")
            .expect("socket error");
        if let Message::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

/// Skip messages until one matches.
async fn recv_until(ws: &mut Ws, pred: impl Fn(&Value) -> bool) -> Value {
    loop {
        let v = recv(ws).await;
        if pred(&v) {
            return v;
        }
    }
}

async fn request(ws: &mut Ws, req: Value) -> Value {
    let id = req["id"].clone();
    ws.send(Message::text(req.to_string())).await.unwrap();
    recv_until(ws, |v| v.get("id") == Some(&id)).await
}

/// Connect and wait until `hello` (or a later `readers` event) satisfies `ready`.
async fn connect_ready(bridge: &Bridge<FakePcsc>, ready: impl Fn(&Value) -> bool) -> Ws {
    let mut ws = connect(bridge).await;
    let hello = recv(&mut ws).await;
    assert_eq!(hello["event"], "hello");
    if !ready(&hello) {
        recv_until(&mut ws, |v| v["event"] == "readers" && ready(v)).await;
    }
    ws
}

fn reader_has_card(v: &Value, reader: &str) -> bool {
    v["readers"].as_array().is_some_and(|rs| {
        rs.iter()
            .any(|r| r["name"] == reader && !r["card"].is_null())
    })
}

fn has_reader(v: &Value, reader: &str) -> bool {
    v["readers"]
        .as_array()
        .is_some_and(|rs| rs.iter().any(|r| r["name"] == reader))
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_lists_existing_cards_without_a_tap() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag215, UID));
    let bridge = start(&fake);
    // Wait until the bridge has identified the card.
    let mut first = connect_ready(&bridge, |v| reader_has_card(v, R1)).await;
    drop(first.close(None).await);

    let mut ws = connect(&bridge).await;
    let hello = recv(&mut ws).await;
    assert_eq!(hello["event"], "hello");
    assert_eq!(hello["protocol"], 1);
    assert_eq!(hello["app"], "nfc-browser-bridge");
    assert_eq!(hello["version"], "9.9.9");
    assert_eq!(hello["service"], "ready");
    assert_eq!(
        hello["readers"],
        json!([{"name": R1, "kind": "picc", "card": {"uid": UID_HEX, "atr": ATR_ULTRALIGHT}}])
    );
    // No card_present follows for a card that was already there.
    let pong = request(&mut ws, json!({"id": 1, "type": "ping"})).await;
    assert_eq!(pong, json!({"id": 1, "ok": true, "result": {}}));
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn hot_plug_and_card_events() {
    let fake = FakePcsc::new();
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| v["service"] == "ready").await;

    fake.add_reader(R2);
    recv_until(&mut ws, |v| v["event"] == "readers" && has_reader(v, R2)).await;

    fake.insert_card(R2, FakeCard::ntag(NtagType::Ntag213, UID));
    let present = recv_until(&mut ws, |v| v["event"] != "readers").await;
    assert_eq!(
        present,
        json!({"event": "card_present", "reader": R2, "uid": UID_HEX, "atr": ATR_ULTRALIGHT})
    );
    let readers = recv(&mut ws).await;
    assert!(reader_has_card(&readers, R2), "{readers}");

    fake.remove_card(R2);
    assert_eq!(
        recv(&mut ws).await,
        json!({"event": "card_removed", "reader": R2})
    );
    let readers = recv(&mut ws).await;
    assert!(!reader_has_card(&readers, R2));

    // Unplugging a reader with a card on it reports the removal.
    fake.insert_card(R2, FakeCard::ntag(NtagType::Ntag213, UID));
    recv_until(&mut ws, |v| v["event"] == "card_present").await;
    recv_until(&mut ws, |v| v["event"] == "readers").await;
    fake.remove_reader(R2);
    assert_eq!(
        recv(&mut ws).await,
        json!({"event": "card_removed", "reader": R2})
    );
    recv_until(&mut ws, |v| {
        v["event"] == "readers" && v["readers"] == json!([])
    })
    .await;
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn polls_reader_list_without_pnp() {
    let fake = FakePcsc::new();
    fake.set_pnp_supported(false);
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| v["service"] == "ready").await;
    fake.add_reader(R2);
    recv_until(&mut ws, |v| v["event"] == "readers" && has_reader(v, R2)).await;
    fake.insert_card(R2, FakeCard::ntag(NtagType::Ntag216, UID));
    recv_until(&mut ws, |v| v["event"] == "card_present").await;
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn recovers_after_service_stops() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag215, UID));
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| reader_has_card(v, R1)).await;

    // Windows stops the service when the last reader goes away.
    fake.set_service(false);
    assert_eq!(
        recv(&mut ws).await,
        json!({"event": "card_removed", "reader": R1})
    );
    assert_eq!(
        recv(&mut ws).await,
        json!({"event": "readers", "service": "unavailable", "readers": []})
    );
    let r = request(
        &mut ws,
        json!({"id": 1, "type": "transmit", "reader": R1, "apdus": ["FFCA000000"]}),
    )
    .await;
    assert_eq!(r["error"]["code"], "pcsc_unavailable");

    fake.set_service(true);
    recv_until(&mut ws, |v| {
        v["event"] == "readers" && v["service"] == "ready"
    })
    .await;
    recv_until(&mut ws, |v| v["event"] == "card_present").await;
    // The worker re-establishes its own context too.
    let r = request(
        &mut ws,
        json!({"id": 2, "type": "transmit", "reader": R1, "apdus": ["FFCA000000"]}),
    )
    .await;
    assert_eq!(
        r["result"]["responses"],
        json!([{"data": UID_HEX, "sw": "9000"}])
    );
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn transmit_reads_and_writes_in_one_batch() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag213, UID));
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| reader_has_card(v, R1)).await;

    let connects = fake.connects();
    let r = request(
        &mut ws,
        json!({"id": "w", "type": "transmit", "reader": R1, "apdus": ["ffd6000404030bd101", "FFB0000410", "FFB0000304"]}),
    )
    .await;
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(
        r["result"]["responses"],
        json!([
            {"data": "", "sw": "9000"},
            {"data": "030BD101000000000000000000000000", "sw": "9000"},
            {"data": "E1101200", "sw": "9000"},
        ])
    );
    assert_eq!(fake.connects(), connects + 1, "one connection per batch");
    assert_eq!(fake.disconnects().last(), Some(&Disposition::LeaveCard));
    assert_eq!(
        &fake.card(R1).unwrap().memory[16..20],
        &[0x03, 0x0B, 0xD1, 0x01]
    );
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_on_error_and_reset_recovers_a_wedged_card() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag213, UID));
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| reader_has_card(v, R1)).await;

    // Out-of-range read wedges the card; the batch stops there.
    let r = request(
        &mut ws,
        json!({"id": 1, "type": "transmit", "reader": R1, "apdus": ["FFB0006410", "FFCA000000"]}),
    )
    .await;
    assert_eq!(
        r["result"]["responses"],
        json!([{"data": "", "sw": "6300"}])
    );
    // With stopOnError false the rest still runs (and fails, the card is wedged).
    let r = request(
        &mut ws,
        json!({"id": 2, "type": "transmit", "reader": R1, "apdus": ["FFCA000000", "FFCA000000"], "stopOnError": false}),
    )
    .await;
    assert_eq!(
        r["result"]["responses"],
        json!([{"data": "", "sw": "6F00"}, {"data": "", "sw": "6F00"}])
    );

    let r = request(&mut ws, json!({"id": 3, "type": "reset", "reader": R1})).await;
    assert_eq!(
        r,
        json!({"id": 3, "ok": true, "result": {"atr": ATR_ULTRALIGHT}})
    );
    assert_eq!(fake.resets(), 1);
    let r = request(
        &mut ws,
        json!({"id": 4, "type": "transmit", "reader": R1, "apdus": ["FFCA000000"]}),
    )
    .await;
    assert_eq!(r["result"]["responses"][0]["sw"], "9000");
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn pcsc_error_resets_card_and_reports_progress() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag213, UID));
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| reader_has_card(v, R1)).await;

    // First APDU succeeds, second hits a PC/SC error.
    fake.fail_after(FaultOp::Transmit, 1, PcscError::COMM_ERROR);
    let r = request(&mut ws, json!({"id": 1, "type": "transmit", "reader": R1, "apdus": ["FFCA000000", "FFCA000000", "FFCA000000"]})).await;
    assert_eq!(r["ok"], false, "{r}");
    assert_eq!(r["error"]["code"], "transmit_failed");
    assert_eq!(r["error"]["pcsc"], "0x80100013");
    assert_eq!(r["error"]["completed"], 1);
    assert_eq!(fake.disconnects().last(), Some(&Disposition::ResetCard));
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn sharing_violation_is_retried_three_times() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag213, UID));
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| reader_has_card(v, R1)).await;

    fake.hold_exclusive(R1, 3);
    let r = request(
        &mut ws,
        json!({"id": 1, "type": "transmit", "reader": R1, "apdus": ["FFCA000000"]}),
    )
    .await;
    assert_eq!(r["ok"], true, "{r}");

    fake.hold_exclusive(R1, 4);
    let r = request(
        &mut ws,
        json!({"id": 2, "type": "transmit", "reader": R1, "apdus": ["FFCA000000"]}),
    )
    .await;
    assert_eq!(r["error"]["code"], "sharing_violation");
    assert_eq!(r["error"]["pcsc"], "0x8010000B");
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn card_errors_map_to_codes() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| has_reader(v, R1)).await;

    let r = request(
        &mut ws,
        json!({"id": 1, "type": "transmit", "reader": R1, "apdus": ["FFCA000000"]}),
    )
    .await;
    assert_eq!(r["error"]["code"], "no_card");
    let r = request(
        &mut ws,
        json!({"id": 2, "type": "transmit", "reader": "nope", "apdus": ["FFCA000000"]}),
    )
    .await;
    assert_eq!(r["error"]["code"], "no_such_reader");
    let r = request(&mut ws, json!({"id": 3, "type": "reset", "reader": "nope"})).await;
    assert_eq!(r["error"]["code"], "no_such_reader");
    let r = request(&mut ws, json!({"id": 4, "type": "list_readers"})).await;
    assert_eq!(
        r["result"],
        json!({"service": "ready", "readers": [{"name": R1, "kind": "picc", "card": null}]})
    );
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn request_validation_and_write_guard() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag213, UID));
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| reader_has_card(v, R1)).await;

    ws.send(Message::text("{oops")).await.unwrap();
    let r = recv_until(&mut ws, |v| v.get("ok").is_some()).await;
    assert_eq!(
        (r["id"].clone(), r["error"]["code"].clone()),
        (Value::Null, json!("bad_request"))
    );

    ws.send(Message::binary(vec![1, 2, 3])).await.unwrap();
    let r = recv_until(&mut ws, |v| v.get("ok").is_some()).await;
    assert_eq!(r["error"]["code"], "bad_request");

    let r = request(&mut ws, json!({"id": 1, "type": "ping", "protocol": 2})).await;
    assert_eq!(r["error"]["code"], "protocol_mismatch");

    let too_many = vec!["FFCA000000"; 601];
    let r = request(
        &mut ws,
        json!({"id": 2, "type": "transmit", "reader": R1, "apdus": too_many}),
    )
    .await;
    assert_eq!(r["error"]["code"], "bad_request");
    let r = request(
        &mut ws,
        json!({"id": 3, "type": "transmit", "reader": R1, "apdus": ["FFCAZZ0000"]}),
    )
    .await;
    assert_eq!(r["error"]["code"], "bad_request");

    let before = fake.card(R1).unwrap().memory;
    let r = request(
        &mut ws,
        json!({"id": 4, "type": "transmit", "reader": R1, "apdus": ["FFD6000304E1100000"]}),
    )
    .await;
    assert_eq!(r["error"]["code"], "blocked_write");
    assert_eq!(fake.card(R1).unwrap().memory, before);
    let r = request(&mut ws, json!({"id": 5, "type": "transmit", "reader": R1, "apdus": ["FFD6000304E1101200"], "unsafe": true})).await;
    assert_eq!(r["ok"], true);

    // A full NTAG216-sized batch is accepted.
    let reads = vec!["FFB0000410"; 600];
    let r = request(
        &mut ws,
        json!({"id": 6, "type": "transmit", "reader": R1, "apdus": reads}),
    )
    .await;
    assert_eq!(r["result"]["responses"].as_array().unwrap().len(), 600);
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_message_closes_the_connection() {
    let fake = FakePcsc::new();
    let bridge = start(&fake);
    let mut ws = connect(&bridge).await;
    recv(&mut ws).await;
    let huge = format!(
        r#"{{"id":1,"type":"ping","pad":"{}"}}"#,
        "x".repeat(1024 * 1024 + 1)
    );
    let _ = ws.send(Message::text(huge)).await;
    let end = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match ws.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(_)) => {}
            }
        }
    })
    .await;
    assert!(end.is_ok(), "connection stayed open");
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn host_header_and_path_are_checked() {
    let fake = FakePcsc::new();
    let bridge = start(&fake);
    let port = bridge.local_addr().port();

    let err = connect_with(&bridge, Some(&format!("evil.example:{port}")), "/v1")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 403),
        "{err:?}"
    );
    let err = connect_with(&bridge, Some(&format!("127.0.0.1:{}", port + 1)), "/v1")
        .await
        .unwrap_err();
    assert!(
        matches!(&err, tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 403),
        "{err:?}"
    );
    let err = connect_with(&bridge, None, "/v2").await.unwrap_err();
    assert!(
        matches!(&err, tokio_tungstenite::tungstenite::Error::Http(r) if r.status() == 404),
        "{err:?}"
    );

    assert!(
        connect_with(&bridge, Some(&format!("localhost:{port}")), "/v1")
            .await
            .is_ok()
    );
    assert!(
        connect_with(&bridge, Some(&format!("127.0.0.1:{port}")), "/v1")
            .await
            .is_ok()
    );
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn two_clients_share_events_and_batches_do_not_interleave() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    let bridge = start(&fake);
    let mut a = connect_ready(&bridge, |v| has_reader(v, R1)).await;
    let mut b = connect_ready(&bridge, |v| has_reader(v, R1)).await;
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag216, UID));
    recv_until(&mut a, |v| v["event"] == "card_present").await;
    recv_until(&mut b, |v| v["event"] == "card_present").await;

    let batch_a: Vec<String> = (0..50).map(|_| "FFB0000404".to_string()).collect();
    let batch_b: Vec<String> = (0..50).map(|_| "FFCA000000".to_string()).collect();
    a.send(Message::text(
        json!({"id": "a", "type": "transmit", "reader": R1, "apdus": batch_a}).to_string(),
    ))
    .await
    .unwrap();
    b.send(Message::text(
        json!({"id": "b", "type": "transmit", "reader": R1, "apdus": batch_b}).to_string(),
    ))
    .await
    .unwrap();
    recv_until(&mut a, |v| v["id"] == "a").await;
    recv_until(&mut b, |v| v["id"] == "b").await;

    // Each batch's APDUs are contiguous in the log.
    let log: Vec<u8> = fake.apdu_log().iter().map(|(_, apdu)| apdu[1]).collect();
    let switches = log.windows(2).filter(|w| w[0] != w[1]).count();
    assert!(switches <= 2, "batches interleaved: {log:?}");
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn port_in_use_is_reported() {
    let fake = FakePcsc::new();
    let bridge = start(&fake);
    let port = bridge.local_addr().port();
    let second = Bridge::start(
        fake.clone(),
        BridgeConfig {
            port,
            version: "x".into(),
        },
        None,
    );
    assert!(matches!(second, Err(bridge_core::StartError::PortInUse)));
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn holds_one_handle_while_a_card_is_present() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| has_reader(v, R1)).await;
    assert_eq!(fake.open_handles(R1), 0);

    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag213, UID));
    recv_until(&mut ws, |v| v["event"] == "card_present").await;
    assert_eq!(
        fake.open_handles(R1),
        1,
        "idle handle keeps the card powered"
    );
    // Batches use their own connection and close it.
    request(
        &mut ws,
        json!({"id": 1, "type": "transmit", "reader": R1, "apdus": ["FFCA000000"]}),
    )
    .await;
    assert_eq!(fake.open_handles(R1), 1);

    fake.remove_card(R1);
    recv_until(&mut ws, |v| v["event"] == "card_removed").await;
    let released = tokio::time::timeout(Duration::from_secs(2), async {
        while fake.open_handles(R1) != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(released.is_ok(), "handle released after removal");

    // Unplugging the reader also releases it.
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag213, UID));
    recv_until(&mut ws, |v| v["event"] == "card_present").await;
    fake.remove_reader(R1);
    recv_until(&mut ws, |v| {
        v["event"] == "readers" && v["readers"] == json!([])
    })
    .await;
    let released = tokio::time::timeout(Duration::from_secs(2), async {
        while fake.open_handles(R1) != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(released.is_ok(), "handle released after reader removal");
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn uid_read_is_retried_after_a_glitch() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| has_reader(v, R1)).await;

    // A card swiped past the reader fails its first command.
    fake.fail_next(FaultOp::Transmit, PcscError::COMM_ERROR);
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag215, UID));
    let ev = recv_until(&mut ws, |v| v["event"] == "card_present").await;
    assert_eq!(ev["uid"], UID_HEX);
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn persistent_errors_re_establish_the_context() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| has_reader(v, R1)).await;

    // An error that is not "service lost" but never goes away.
    for _ in 0..5 {
        fake.fail_next(FaultOp::GetStatusChange, PcscError::INTERNAL_ERROR);
    }
    fake.add_reader(R2);
    recv_until(&mut ws, |v| {
        v["event"] == "readers" && v["service"] == "unavailable"
    })
    .await;
    recv_until(&mut ws, |v| {
        v["event"] == "readers" && v["service"] == "ready" && has_reader(v, R2)
    })
    .await;
    // The new context works.
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag215, UID));
    let ev = recv_until(&mut ws, |v| v["event"] == "card_present").await;
    assert_eq!(ev["uid"], UID_HEX);
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_vanished_reader_does_not_spin_the_monitor() {
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| has_reader(v, R1)).await;

    // The reader keeps being listed but cannot be watched: the monitor re-lists at once, then
    // waits between attempts and finally starts over with a new context.
    for _ in 0..4 {
        fake.fail_next(FaultOp::GetStatusChange, PcscError::UNKNOWN_READER);
    }
    let started = std::time::Instant::now();
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag215, UID));
    recv_until(&mut ws, |v| v["event"] == "card_present").await;
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "waited between failed attempts"
    );
    bridge.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hung_card_call_is_reported_as_a_stall() {
    use bridge_core::health::Part;
    let fake = FakePcsc::new();
    fake.add_reader(R1);
    let bridge = start(&fake);
    let mut ws = connect_ready(&bridge, |v| has_reader(v, R1)).await;
    let health = bridge.health();
    // Above the monitor's 1 s status wait, which is a normal call.
    let limit = Duration::from_millis(1500);
    assert_eq!(health.stalled(limit), None);

    // Idle waiting (monitor between taps, worker without jobs) is not a stall.
    for _ in 0..10 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(health.stalled(limit), None);
    }

    fake.delay_next_transmit(Duration::from_secs(3));
    fake.insert_card(R1, FakeCard::ntag(NtagType::Ntag215, UID));
    tokio::time::sleep(Duration::from_secs(2)).await;
    let stall = health.stalled(limit).expect("worker reported as stuck");
    assert_eq!(stall.part, Part::Worker);

    // It recovers on its own once the call returns.
    recv_until(&mut ws, |v| v["event"] == "card_present").await;
    assert_eq!(health.stalled(limit), None);
    bridge.shutdown();
}
