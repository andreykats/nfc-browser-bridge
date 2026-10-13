//! Print a reader's state, try one SCardConnect, and watch the state for 3 s.
//! `cargo run -p bridge-core --example probe -- "<reader name>"`

use std::ffi::CString;
use std::time::{Duration, Instant};

use pcsc::{Context, Protocols, ReaderState, Scope, ShareMode, State};

fn main() {
    let name = CString::new(std::env::args().nth(1).expect("reader name")).unwrap();
    let ctx = Context::establish(Scope::User).unwrap();
    let mut rs = [ReaderState::new(name.clone(), State::UNAWARE)];
    ctx.get_status_change(Duration::from_millis(100), &mut rs)
        .ok();
    println!("state before: {:?}", rs[0].event_state());
    rs[0].sync_current_state();
    match ctx.connect(&name, ShareMode::Shared, Protocols::ANY) {
        Ok(_) => println!("connect: OK"),
        Err(e) => println!("connect: {e:?}"),
    }
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(3) {
        if ctx
            .get_status_change(Duration::from_millis(500), &mut rs)
            .is_ok()
        {
            println!(
                "{:>5} ms: {:?}",
                t.elapsed().as_millis(),
                rs[0].event_state()
            );
            rs[0].sync_current_state();
        }
    }
}
