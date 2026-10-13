//! Protocol v1 wire types (see docs/PROTOCOL.md), hex helpers, request validation, write guard.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::backend::PcscError;

pub const PROTOCOL_VERSION: u32 = 1;
pub const APP_NAME: &str = "nfc-browser-bridge";
pub const DEFAULT_PORT: u16 = 38241;
pub const WS_PATH: &str = "/v1";

pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_APDUS: usize = 600;
pub const MIN_APDU_BYTES: usize = 4;
pub const MAX_APDU_BYTES: usize = 261;
pub const MAX_IN_FLIGHT: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReaderKind {
    Picc,
    Sam,
    Other,
}

impl ReaderKind {
    /// Classify by PC/SC name. ACS multi-slot readers say "PICC"/"SAM"; single-slot contactless
    /// models (e.g. "ACS ACR1255U-J1") only give the model.
    pub fn from_name(name: &str) -> Self {
        const CONTACTLESS_MODELS: [&str; 3] = ["ACR1255U", "ACR122", "ACR1251"];
        let upper = name.to_ascii_uppercase();
        if upper.contains("SAM") {
            Self::Sam
        } else if upper.contains("PICC")
            || upper.contains("CONTACTLESS")
            || CONTACTLESS_MODELS.iter().any(|m| upper.contains(m))
        {
            Self::Picc
        } else {
            Self::Other
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ServiceStatus {
    Ready,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardInfo {
    pub uid: Option<String>,
    pub atr: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reader {
    pub name: String,
    pub kind: ReaderKind,
    pub card: Option<CardInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Hello {
        protocol: u32,
        app: String,
        version: String,
        service: ServiceStatus,
        readers: Vec<Reader>,
    },
    Readers {
        service: ServiceStatus,
        readers: Vec<Reader>,
    },
    CardPresent {
        reader: String,
        uid: Option<String>,
        atr: String,
    },
    CardRemoved {
        reader: String,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestBody {
    Ping,
    ListReaders,
    Transmit {
        reader: String,
        apdus: Vec<String>,
        #[serde(rename = "stopOnError", default = "yes")]
        stop_on_error: bool,
        #[serde(rename = "unsafe", default)]
        unsafe_write: bool,
    },
    Reset {
        reader: String,
    },
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    ProtocolMismatch,
    NoSuchReader,
    NoCard,
    CardRemoved,
    SharingViolation,
    TransmitFailed,
    BlockedWrite,
    PcscUnavailable,
    Busy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BridgeError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "ser_pcsc")]
    pub pcsc: Option<PcscError>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<usize>,
}

fn ser_pcsc<S: serde::Serializer>(v: &Option<PcscError>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(e) => s.serialize_str(&e.to_string()),
        None => s.serialize_none(),
    }
}

impl BridgeError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            pcsc: None,
            completed: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, message)
    }

    /// Map a PC/SC failure while talking to `reader` to a protocol error.
    pub fn from_pcsc(e: PcscError) -> Self {
        let (code, message) = match e {
            PcscError::NO_SMARTCARD => (ErrorCode::NoCard, "No card on reader"),
            PcscError::REMOVED_CARD => (ErrorCode::CardRemoved, "Card was removed"),
            PcscError::UNKNOWN_READER
            | PcscError::READER_UNAVAILABLE
            | PcscError::NO_READERS_AVAILABLE => {
                (ErrorCode::NoSuchReader, "Reader is not available")
            }
            PcscError::SHARING_VIOLATION => (
                ErrorCode::SharingViolation,
                "Card is in use by another application",
            ),
            e if e.is_service_lost() => {
                (ErrorCode::PcscUnavailable, "Smart card service unavailable")
            }
            _ => (ErrorCode::TransmitFailed, "PC/SC command failed"),
        };
        Self {
            code,
            message: message.into(),
            pcsc: Some(e),
            completed: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Response {
    Ok {
        id: Value,
        ok: bool,
        result: Value,
    },
    Err {
        id: Value,
        ok: bool,
        error: BridgeError,
    },
}

impl Response {
    pub fn ok(id: Value, result: Value) -> Self {
        Self::Ok {
            id,
            ok: true,
            result,
        }
    }

    pub fn err(id: Value, error: BridgeError) -> Self {
        Self::Err {
            id,
            ok: false,
            error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApduResponse {
    pub data: String,
    pub sw: String,
}

impl ApduResponse {
    /// Split a raw response into body and status word. `None` if shorter than 2 bytes.
    pub fn from_raw(raw: &[u8]) -> Option<Self> {
        let split = raw.len().checked_sub(2)?;
        Some(Self {
            data: encode_hex(&raw[..split]),
            sw: encode_hex(&raw[split..]),
        })
    }

    pub fn is_success(&self) -> bool {
        self.sw == "9000"
    }
}

/// A parsed, not-yet-validated request.
#[derive(Debug)]
pub struct Request {
    pub id: Value,
    pub body: RequestBody,
}

/// Parse a text frame. On failure returns the id (or null) and the error to send back.
pub fn parse_request(text: &str) -> Result<Request, (Value, BridgeError)> {
    let value: Value = serde_json::from_str(text).map_err(|e| {
        (
            Value::Null,
            BridgeError::bad_request(format!("Invalid JSON: {e}")),
        )
    })?;
    let Value::Object(obj) = &value else {
        return Err((
            Value::Null,
            BridgeError::bad_request("Request must be a JSON object"),
        ));
    };
    let id = match obj.get("id") {
        Some(id @ (Value::String(_) | Value::Number(_))) => id.clone(),
        _ => {
            return Err((
                Value::Null,
                BridgeError::bad_request("Request needs an \"id\" (string or number)"),
            ));
        }
    };
    if let Some(p) = obj.get("protocol")
        && p.as_u64() != Some(u64::from(PROTOCOL_VERSION))
    {
        return Err((
            id,
            BridgeError::new(
                ErrorCode::ProtocolMismatch,
                format!("Bridge speaks protocol {PROTOCOL_VERSION}"),
            ),
        ));
    }
    let body = RequestBody::deserialize(&value)
        .map_err(|e| (id.clone(), BridgeError::bad_request(e.to_string())))?;
    Ok(Request { id, body })
}

/// Decode and check a transmit batch: count, hex, sizes, write guard.
pub fn validate_apdus(apdus: &[String], unsafe_write: bool) -> Result<Vec<Vec<u8>>, BridgeError> {
    if apdus.is_empty() {
        return Err(BridgeError::bad_request("\"apdus\" is empty"));
    }
    if apdus.len() > MAX_APDUS {
        return Err(BridgeError::bad_request(format!(
            "At most {MAX_APDUS} APDUs per batch"
        )));
    }
    let mut out = Vec::with_capacity(apdus.len());
    for (i, hex) in apdus.iter().enumerate() {
        let apdu = decode_hex(hex)
            .ok_or_else(|| BridgeError::bad_request(format!("APDU {i} is not valid hex")))?;
        if !(MIN_APDU_BYTES..=MAX_APDU_BYTES).contains(&apdu.len()) {
            return Err(BridgeError::bad_request(format!(
                "APDU {i} must be {MIN_APDU_BYTES} to {MAX_APDU_BYTES} bytes, got {}",
                apdu.len()
            )));
        }
        if !unsafe_write && is_guarded_write(&apdu) {
            return Err(BridgeError::new(
                ErrorCode::BlockedWrite,
                format!(
                    "APDU {i} writes to pages 0-3 (UID, lock bytes, CC); set \"unsafe\": true to allow"
                ),
            ));
        }
        out.push(apdu);
    }
    Ok(out)
}

const T2_WRITE: u8 = 0xA2;
const T2_COMP_WRITE: u8 = 0xA0;

fn is_t2_write_to_header(cmd: &[u8]) -> bool {
    matches!(cmd, [T2_WRITE | T2_COMP_WRITE, page, ..] if *page < 4)
}

/// True if the APDU writes NFC Forum Type 2 pages 0-3.
pub fn is_guarded_write(apdu: &[u8]) -> bool {
    match apdu {
        // UPDATE BINARY: address = P1P2; pages run contiguously from there.
        [0xFF, 0xD6, p1, p2, ..] => (u16::from(*p1) << 8 | u16::from(*p2)) < 4,
        // ACS transparent session exchange: look for a Transceive (0x95) data object.
        [0xFF, 0xC2, 0x00, 0x01, _, data @ ..] => {
            ber_tlvs(data).any(|(tag, value)| tag == 0x95 && is_t2_write_to_header(value))
        }
        // PN532 pass-through (ACR122-style): InDataExchange (D4 40 tg cmd…) / InCommunicateThru (D4 42 cmd…).
        [0xFF, 0x00, 0x00, 0x00, _, 0xD4, 0x40, _, cmd @ ..]
        | [0xFF, 0x00, 0x00, 0x00, _, 0xD4, 0x42, cmd @ ..] => is_t2_write_to_header(cmd),
        _ => false,
    }
}

/// Iterate simple BER-TLVs (1- or 2-byte tags, 1-byte or 0x81/0x82 lengths). Stops at malformed data.
fn ber_tlvs(mut data: &[u8]) -> impl Iterator<Item = (u16, &[u8])> {
    std::iter::from_fn(move || {
        let (&first, rest) = data.split_first()?;
        let (tag, rest) = if first & 0x1F == 0x1F {
            let (&second, rest) = rest.split_first()?;
            (u16::from(first) << 8 | u16::from(second), rest)
        } else {
            (u16::from(first), rest)
        };
        let (len, rest) = match rest {
            [0x81, l, rest @ ..] => (usize::from(*l), rest),
            [0x82, h, l, rest @ ..] => (usize::from(*h) << 8 | usize::from(*l), rest),
            [l, rest @ ..] if *l < 0x80 => (usize::from(*l), rest),
            _ => return None,
        };
        if rest.len() < len {
            return None;
        }
        let (value, rest) = rest.split_at(len);
        data = rest;
        Some((tag, value))
    })
}

pub fn encode_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(DIGITS[usize::from(b >> 4)]));
        s.push(char::from(DIGITS[usize::from(b & 0xF)]));
    }
    s
}

/// Decode hex (either case, no separators). `None` on odd length or a non-hex character.
pub fn decode_hex(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    let (pairs, rest) = s.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|pair| {
            let hi = (pair[0] as char).to_digit(16)?;
            let lo = (pair[1] as char).to_digit(16)?;
            Some((hi << 4 | lo) as u8)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hex_round_trip() {
        assert_eq!(encode_hex(&[0x04, 0xA2, 0xFF]), "04A2FF");
        assert_eq!(decode_hex("04a2Ff"), Some(vec![0x04, 0xA2, 0xFF]));
        assert_eq!(decode_hex("0"), None);
        assert_eq!(decode_hex("zz"), None);
        assert_eq!(decode_hex("+1"), None);
        assert_eq!(decode_hex(""), Some(vec![]));
    }

    #[test]
    fn reader_kinds() {
        assert_eq!(
            ReaderKind::from_name("ACS ACR1552 1S CL Reader PICC 0"),
            ReaderKind::Picc
        );
        assert_eq!(
            ReaderKind::from_name("ACS ACR1552 1S CL Reader SAM 0"),
            ReaderKind::Sam
        );
        assert_eq!(ReaderKind::from_name("ACS ACR1255U-J1"), ReaderKind::Picc);
        assert_eq!(
            ReaderKind::from_name("Generic Smart Card Reader"),
            ReaderKind::Other
        );
    }

    #[test]
    fn parse_requests() {
        let r = parse_request(r#"{"id":1,"type":"transmit","reader":"R","apdus":["FFCA000000"]}"#)
            .unwrap();
        assert_eq!(r.id, json!(1));
        match r.body {
            RequestBody::Transmit {
                stop_on_error,
                unsafe_write,
                ..
            } => assert!(stop_on_error && !unsafe_write),
            other => panic!("{other:?}"),
        }
        let (id, e) = parse_request(r#"{"id":"a","type":"nope"}"#).unwrap_err();
        assert_eq!((id, e.code), (json!("a"), ErrorCode::BadRequest));
        let (id, e) = parse_request("not json").unwrap_err();
        assert_eq!((id, e.code), (Value::Null, ErrorCode::BadRequest));
        let (id, e) = parse_request(r#"{"id":{},"type":"ping"}"#).unwrap_err();
        assert_eq!((id, e.code), (Value::Null, ErrorCode::BadRequest));
        let (_, e) = parse_request(r#"{"id":2,"type":"ping","protocol":2}"#).unwrap_err();
        assert_eq!(e.code, ErrorCode::ProtocolMismatch);
        assert!(parse_request(r#"{"id":2,"type":"ping","protocol":1}"#).is_ok());
    }

    #[test]
    fn apdu_validation() {
        assert!(validate_apdus(&[], false).is_err());
        assert_eq!(
            validate_apdus(&["FFCA00".into()], false).unwrap_err().code,
            ErrorCode::BadRequest
        );
        assert_eq!(
            validate_apdus(&["FFCA00000".into()], false)
                .unwrap_err()
                .code,
            ErrorCode::BadRequest
        );
        assert_eq!(
            validate_apdus(&["AA".repeat(262)], false).unwrap_err().code,
            ErrorCode::BadRequest
        );
        assert!(validate_apdus(&["AA".repeat(261)], false).is_ok());
        assert_eq!(
            validate_apdus(&vec!["FFCA000000".to_string(); 601], false)
                .unwrap_err()
                .code,
            ErrorCode::BadRequest
        );
        assert!(validate_apdus(&vec!["FFCA000000".to_string(); 600], false).is_ok());
    }

    #[test]
    fn write_guard() {
        let blocked = |hex: &str| is_guarded_write(&decode_hex(hex).unwrap());
        assert!(blocked("FFD6000304E1103E00"));
        assert!(blocked("FFD6000004AABBCCDD"));
        assert!(!blocked("FFD6000404030000FE"));
        assert!(!blocked("FFD6010004AABBCCDD")); // page 256
        assert!(!blocked("FFB0000310"));
        // Transparent session write to page 2 / page 4.
        assert!(blocked(
            "FFC200010895 06 A2 02 00000000".replace(' ', "").as_str()
        ));
        assert!(!blocked(
            "FFC200010895 06 A2 04 00000000".replace(' ', "").as_str()
        ));
        // With a leading timer data object (5F46).
        assert!(blocked(
            "FFC200010F5F460400000000 95 06 A0 03 00000000"
                .replace(' ', "")
                .as_str()
        ));
        // Start/end session are not writes.
        assert!(!blocked("FFC20000028100"));
        // PN532 pass-through.
        assert!(blocked("FF00000008D44001A20300000000"));
        assert!(blocked("FF00000007D442A20100000000"));
        assert!(!blocked("FF00000008D44001A20500000000"));
        assert!(validate_apdus(&["FFD6000304E1103E00".into()], true).is_ok());
        assert_eq!(
            validate_apdus(&["FFD6000304E1103E00".into()], false)
                .unwrap_err()
                .code,
            ErrorCode::BlockedWrite
        );
    }

    #[test]
    fn error_serialization() {
        let mut e = BridgeError::from_pcsc(PcscError::NO_SMARTCARD);
        e.completed = Some(2);
        let v = serde_json::to_value(Response::err(json!(7), e)).unwrap();
        assert_eq!(
            v,
            json!({"id":7,"ok":false,"error":{"code":"no_card","message":"No card on reader","pcsc":"0x8010000C","completed":2}})
        );
        let v = serde_json::to_value(Event::CardRemoved { reader: "R".into() }).unwrap();
        assert_eq!(v, json!({"event":"card_removed","reader":"R"}));
    }
}
