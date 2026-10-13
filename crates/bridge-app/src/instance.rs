//! Is the process on our port another NFC Browser Bridge? Connect and read its `hello`.

use std::io::ErrorKind;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::time::Duration;

use tungstenite::Message;

pub fn is_our_bridge(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(1)) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let Ok((mut ws, _)) = tungstenite::client(format!("ws://127.0.0.1:{port}/v1"), stream) else {
        return false;
    };
    let ours = loop {
        match ws.read() {
            Ok(Message::Text(text)) => {
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                break v["event"] == "hello" && v["app"] == bridge_core::protocol::APP_NAME;
            }
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e)) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => break false,
        }
    };
    let _ = ws.close(None);
    ours
}
