//! List the smart-card readers on USB, or reset them (the software equivalent of replugging).
//!
//! `cargo run -p bridge-core --example usb [reset]`

fn main() {
    let usb = bridge_core::usb::system();
    let Some(readers) = usb.readers() else {
        eprintln!("this platform cannot see USB");
        std::process::exit(1);
    };
    if readers.is_empty() {
        println!("no smart-card readers on USB");
    }
    let reset = std::env::args().nth(1).as_deref() == Some("reset");
    for r in &readers {
        println!("{} (id 0x{:x}): {}", r.name, r.id, r.identity);
        if reset {
            match usb.reset(r) {
                Ok(()) => println!("  reset"),
                Err(e) => println!("  reset failed: {e}"),
            }
        }
    }
}
