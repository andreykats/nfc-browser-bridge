//! Hardware spike: list readers, wait for a card on each PICC slot, print ATR and UID.
//!
//! Run: `cargo run -p bridge-core --example spike`
//! Leave it running; tap/remove cards and plug/unplug readers. Ctrl-C to exit.

use std::ffi::CString;
use std::time::Duration;

use pcsc::{
    Context, Disposition, Error, PNP_NOTIFICATION, Protocols, ReaderState, Scope, ShareMode, State,
};

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

fn main() {
    let ctx = match Context::establish(Scope::User) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("establish failed: {e} (0x{:08X})", e as u32);
            std::process::exit(1);
        }
    };
    println!("PC/SC context established");

    let mut states: Vec<ReaderState> = vec![ReaderState::new(PNP_NOTIFICATION(), State::UNAWARE)];
    loop {
        // Re-list readers every pass; keep states for readers that still exist.
        let names: Vec<CString> = match ctx.list_readers_owned() {
            Ok(n) => n,
            Err(Error::NoReadersAvailable) => vec![],
            Err(e) => {
                eprintln!("list_readers: {e}");
                vec![]
            }
        };
        states.retain(|s| {
            s.name() == PNP_NOTIFICATION() || names.iter().any(|n| n.as_c_str() == s.name())
        });
        for n in &names {
            if !states.iter().any(|s| s.name() == n.as_c_str()) {
                println!("reader: {:?}", n);
                states.push(ReaderState::new(n.clone(), State::UNAWARE));
            }
        }

        match ctx.get_status_change(Duration::from_secs(1), &mut states) {
            Ok(()) => {}
            Err(Error::Timeout) => continue,
            Err(e) => {
                eprintln!("get_status_change: {e} (0x{:08X})", e as u32);
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        }

        for s in &mut states {
            let ev = s.event_state();
            if !ev.contains(State::CHANGED) {
                continue;
            }
            let name = s.name().to_owned();
            if name.as_c_str() == PNP_NOTIFICATION() {
                println!("PnP notification ({ev:?})");
            } else {
                println!("{:?}: {:?} atr={}", name, ev, hex(s.atr()));
                if ev.contains(State::PRESENT) && !ev.contains(State::MUTE) {
                    probe(&ctx, &name);
                }
            }
            s.sync_current_state();
        }
    }
}

fn probe(ctx: &Context, reader: &CString) {
    let card = match ctx.connect(reader, ShareMode::Shared, Protocols::ANY) {
        Ok(c) => c,
        Err(e) => {
            println!("  connect: {e} (0x{:08X})", e as u32);
            return;
        }
    };
    let mut buf = [0u8; pcsc::MAX_BUFFER_SIZE];
    for (label, apdu) in [
        (
            "GET UID   FF CA 00 00 00",
            &[0xFF, 0xCA, 0x00, 0x00, 0x00][..],
        ),
        (
            "READ p0   FF B0 00 00 10",
            &[0xFF, 0xB0, 0x00, 0x00, 0x10][..],
        ),
        (
            "READ p3   FF B0 00 03 04",
            &[0xFF, 0xB0, 0x00, 0x03, 0x04][..],
        ),
    ] {
        match card.transmit(apdu, &mut buf) {
            Ok(r) => println!("  {label} -> {}", hex(r)),
            Err(e) => println!("  {label} -> error {e} (0x{:08X})", e as u32),
        }
    }
    if let Err((_, e)) = card.disconnect(Disposition::LeaveCard) {
        println!("  disconnect: {e}");
    }
}
