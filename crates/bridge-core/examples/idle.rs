//! Measure how long a card can sit idle on a reader before SCardConnect fails.
//! `cargo run -p bridge-core --example idle -- "<reader name>"`

use std::ffi::CString;
use std::time::{Duration, Instant};

use pcsc::{Context, Disposition, Protocols, ReaderState, Scope, ShareMode, State};

fn wait_present(ctx: &Context, reader: &CString) {
    let mut rs = [ReaderState::new(reader.clone(), State::UNAWARE)];
    loop {
        ctx.get_status_change(Duration::from_secs(5), &mut rs).ok();
        if rs[0].event_state().contains(State::PRESENT) {
            return;
        }
        rs[0].sync_current_state();
    }
}

fn main() {
    let name = CString::new(std::env::args().nth(1).expect("reader name")).unwrap();
    let ctx = Context::establish(Scope::User).unwrap();
    if std::env::args().nth(2).as_deref() == Some("hold") {
        // Keep one shared handle open; connect separately from a second context after idling.
        wait_present(&ctx, &name);
        let holder = loop {
            match ctx.connect(&name, ShareMode::Shared, Protocols::ANY) {
                Ok(c) => break c,
                Err(e) => {
                    println!("holder connect: {e:?}, retrying");
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        };
        println!("holding a handle");
        let ctx2 = Context::establish(Scope::User).unwrap();
        for delay in [12u64, 20] {
            std::thread::sleep(Duration::from_secs(delay));
            match ctx2.connect(&name, ShareMode::Shared, Protocols::ANY) {
                Ok(card) => {
                    let mut buf = [0u8; 64];
                    let uid = card
                        .transmit(&[0xFF, 0xCA, 0, 0, 0], &mut buf)
                        .map(|b| b.to_vec());
                    println!("hold, idle {delay}s: second connect OK, FF CA -> {uid:02X?}");
                    let _ = card.disconnect(Disposition::LeaveCard);
                }
                Err(e) => println!("hold, idle {delay}s: second connect failed {e:?}"),
            }
        }
        drop(holder);
        return;
    }
    for delay in [1u64, 3, 5, 7, 9, 12] {
        wait_present(&ctx, &name);
        let t = Instant::now();
        std::thread::sleep(Duration::from_secs(delay));
        let r = ctx.connect(&name, ShareMode::Shared, Protocols::ANY);
        match r {
            Ok(card) => {
                let mut buf = [0u8; 64];
                let uid = card
                    .transmit(&[0xFF, 0xCA, 0, 0, 0], &mut buf)
                    .map(|b| b.to_vec());
                println!("idle {delay:>2}s: connect OK, FF CA -> {uid:02X?}");
                let _ = card.disconnect(Disposition::LeaveCard);
            }
            Err(e) => println!(
                "idle {delay:>2}s: connect failed {e:?} after {:?}",
                t.elapsed()
            ),
        }
    }
}
