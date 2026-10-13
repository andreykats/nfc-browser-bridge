//! Regenerate packaging icons from the tray glyph:
//!   cargo run -p bridge-app --example gen_icons
//! Writes packaging/windows/app.ico and (macOS, via iconutil) packaging/macos/AppIcon.icns.

use std::path::Path;

use bridge_app::icons::{IconState, rgba};

fn png(size: u32) -> Vec<u8> {
    // Glyph on a white rounded tile so it reads on any background.
    let glyph = rgba(IconState::Ready, size, false);
    let mut out = Vec::with_capacity(glyph.len());
    let r = size as f32 * 0.18;
    for y in 0..size {
        for x in 0..size {
            let i = ((y * size + x) * 4) as usize;
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let (cx, cy) = (fx.clamp(r, size as f32 - r), fy.clamp(r, size as f32 - r));
            let inside = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt() <= r;
            let a = glyph[i + 3] as f32 / 255.0;
            let bg = if inside { 255.0 } else { 0.0 };
            let blend = |c: u8| (c as f32 * a + 255.0 * (1.0 - a)) as u8;
            if inside {
                out.extend_from_slice(&[
                    blend(glyph[i]),
                    blend(glyph[i + 1]),
                    blend(glyph[i + 2]),
                    255,
                ]);
            } else {
                out.extend_from_slice(&[
                    glyph[i],
                    glyph[i + 1],
                    glyph[i + 2],
                    (a * 255.0).max(bg) as u8,
                ]);
            }
        }
    }
    let mut buf = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut buf, size, size);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.write_header().unwrap().write_image_data(&out).unwrap();
    }
    buf
}

/// ICO with PNG-compressed entries (supported since Windows Vista).
fn ico(sizes: &[u32]) -> Vec<u8> {
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| png(s)).collect();
    let mut out = vec![0, 0, 1, 0];
    out.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (&s, img) in sizes.iter().zip(&images) {
        let dim = if s >= 256 { 0 } else { s as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0, 1, 0, 32, 0]);
        out.extend_from_slice(&(img.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        out.extend_from_slice(&img);
    }
    out
}

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging");
    std::fs::create_dir_all(root.join("windows")).unwrap();
    std::fs::write(
        root.join("windows/app.ico"),
        ico(&[16, 24, 32, 48, 64, 256]),
    )
    .unwrap();
    println!("wrote packaging/windows/app.ico");

    if cfg!(target_os = "macos") {
        let set = std::env::temp_dir().join("AppIcon.iconset");
        let _ = std::fs::remove_dir_all(&set);
        std::fs::create_dir_all(&set).unwrap();
        for s in [16, 32, 128, 256, 512] {
            std::fs::write(set.join(format!("icon_{s}x{s}.png")), png(s)).unwrap();
            std::fs::write(set.join(format!("icon_{s}x{s}@2x.png")), png(s * 2)).unwrap();
        }
        std::fs::create_dir_all(root.join("macos")).unwrap();
        let status = std::process::Command::new("iconutil")
            .args(["-c", "icns", "-o"])
            .arg(root.join("macos/AppIcon.icns"))
            .arg(&set)
            .status()
            .expect("iconutil");
        assert!(status.success());
        println!("wrote packaging/macos/AppIcon.icns");
    }
}
