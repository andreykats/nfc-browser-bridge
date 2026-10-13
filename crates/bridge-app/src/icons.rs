//! Procedurally drawn tray icons: a contactless card glyph in three states.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    NoReader,
    Ready,
    CardPresent,
}

/// Anti-aliased RGBA for `state`, `size` px square. `template` draws black-on-transparent (macOS
/// template image, tinted by the system); otherwise the glyph is colored per state.
pub fn rgba(state: IconState, size: u32, template: bool) -> Vec<u8> {
    let color: [u8; 3] = if template {
        [0, 0, 0]
    } else {
        match state {
            IconState::NoReader => [0x80, 0x80, 0x86],
            IconState::Ready => [0xC8, 0x10, 0x2E],
            IconState::CardPresent => [0x1F, 0x9D, 0x4D],
        }
    };
    let dim = if state == IconState::NoReader && template {
        0.55
    } else {
        1.0
    };
    const SS: u32 = 4;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (px as f32 + (sx as f32 + 0.5) / SS as f32) / size as f32;
                    let y = (py as f32 + (sy as f32 + 0.5) / SS as f32) / size as f32;
                    if covered(state, x, y) {
                        hits += 1;
                    }
                }
            }
            let a = hits as f32 / (SS * SS) as f32 * dim;
            out.extend_from_slice(&[color[0], color[1], color[2], (a * 255.0).round() as u8]);
        }
    }
    out
}

/// Card rectangle and wave geometry in unit coordinates.
const CARD: (f32, f32, f32, f32) = (0.06, 0.24, 0.62, 0.76);
const RADIUS: f32 = 0.08;
const STROKE: f32 = 0.075;

fn covered(state: IconState, x: f32, y: f32) -> bool {
    let d = rounded_rect_sd(x, y);
    let card = match state {
        IconState::CardPresent => d <= 0.0,
        _ => (-STROKE..=0.0).contains(&d),
    };
    let waves = state != IconState::NoReader && waves(x, y);
    let slash = state == IconState::NoReader
        && segment_distance(x, y, (0.08, 0.92), (0.92, 0.08)) <= STROKE / 2.0;
    card || waves || slash
}

fn rounded_rect_sd(x: f32, y: f32) -> f32 {
    let (x0, y0, x1, y1) = CARD;
    let cx = (x0 + x1) / 2.0;
    let cy = (y0 + y1) / 2.0;
    let hx = (x1 - x0) / 2.0 - RADIUS;
    let hy = (y1 - y0) / 2.0 - RADIUS;
    let qx = (x - cx).abs() - hx;
    let qy = (y - cy).abs() - hy;
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    outside + qx.max(qy).min(0.0) - RADIUS
}

fn waves(x: f32, y: f32) -> bool {
    let (cx, cy) = (0.66, 0.5);
    let dx = x - cx;
    let dy = y - cy;
    if dx <= 0.0 || dy.abs() > dx * 1.1 {
        return false;
    }
    let r = (dx * dx + dy * dy).sqrt();
    [0.12, 0.22].iter().any(|&w| (r - w).abs() <= STROKE / 2.0)
}

fn segment_distance(x: f32, y: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (vx, vy) = (b.0 - a.0, b.1 - a.1);
    let t = (((x - a.0) * vx + (y - a.1) * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
    let (px, py) = (a.0 + t * vx - x, a.1 + t * vy - y);
    (px * px + py * py).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_have_the_right_size_and_differ() {
        let a = rgba(IconState::NoReader, 32, false);
        let b = rgba(IconState::Ready, 32, false);
        let c = rgba(IconState::CardPresent, 32, true);
        assert_eq!(a.len(), 32 * 32 * 4);
        assert_ne!(a, b);
        assert!(c.chunks(4).all(|p| p[0..3] == [0, 0, 0]));
        assert!(c.chunks(4).any(|p| p[3] == 255));
    }
}
