//! Run the bridge headless against the real PC/SC service (no tray).
//!
//! `RUST_LOG=debug cargo run -p bridge-core --example serve`

use bridge_core::real::RealPcsc;
use bridge_core::{Bridge, BridgeConfig};

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let bridge = match Bridge::start(RealPcsc, BridgeConfig::default(), None) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot start: {e}");
            std::process::exit(1);
        }
    };
    println!(
        "listening on ws://{}/v1 — Enter to quit",
        bridge.local_addr()
    );
    let _ = std::io::stdin().read_line(&mut String::new());
    bridge.shutdown();
}
