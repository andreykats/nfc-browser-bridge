//! Procedurally drawn tray icons: a contactless card glyph in three states.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    NoReader,
    Ready,
    CardPresent,
}

/// Anti-aliased RGBA for `state`, `width`×`height` px. The glyph is drawn in a unit square,
/// centred and scaled as large as the width allows, up to the card filling [`MAX_CARD_FILL`] of the
/// height. A square canvas gets the unit square exactly. `template` draws black-on-transparent
/// (macOS template image, tinted by the system); otherwise the glyph is colored per state.
pub fn rgba(state: IconState, width: u32, height: u32, template: bool) -> Vec<u8> {
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
    let (w, h) = (width as f32, height as f32);
    let card_height = CARD.3 - CARD.1;
    let scale = (h * MAX_CARD_FILL / card_height).min(w);
    const SS: u32 = 4;
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for py in 0..height {
        for px in 0..width {
            let mut hits = 0;
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = 0.5 + (px as f32 + (sx as f32 + 0.5) / SS as f32 - w / 2.0) / scale;
                    let y = 0.5 + (py as f32 + (sy as f32 + 0.5) / SS as f32 - h / 2.0) / scale;
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
/// Largest card height as a fraction of the canvas height: matches the height of full-size macOS
/// menu-bar glyphs.
const MAX_CARD_FILL: f32 = 0.71;

fn covered(state: IconState, x: f32, y: f32) -> bool {
    let d = rounded_rect_sd(x, y);
    let card = match state {
        IconState::CardPresent => d <= 0.0,
        _ => (-STROKE..=0.0).contains(&d),
    };
    let waves = state != IconState::NoReader && waves(x, y);
    let cross = state == IconState::NoReader && cross(x, y);
    card || waves || cross
}

/// An X centred inside the card outline.
fn cross(x: f32, y: f32) -> bool {
    const HALF: f32 = 0.09;
    let (x0, y0, x1, y1) = CARD;
    let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    let a = segment_distance(x, y, (cx - HALF, cy - HALF), (cx + HALF, cy + HALF));
    let b = segment_distance(x, y, (cx - HALF, cy + HALF), (cx + HALF, cy - HALF));
    a.min(b) <= STROKE / 2.0
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
        let a = rgba(IconState::NoReader, 32, 32, false);
        let b = rgba(IconState::Ready, 32, 32, false);
        let c = rgba(IconState::CardPresent, 60, 44, true);
        assert_eq!(a.len(), 32 * 32 * 4);
        assert_eq!(c.len(), 60 * 44 * 4);
        assert_ne!(a, b);
        assert!(c.chunks(4).all(|p| p[0..3] == [0, 0, 0]));
        assert!(c.chunks(4).any(|p| p[3] == 255));
    }

    /// Rows of the canvas the glyph touches.
    fn ink_rows(px: &[u8], width: u32) -> (usize, usize) {
        let rows: Vec<usize> = px
            .chunks((width * 4) as usize)
            .enumerate()
            .filter(|(_, row)| row.chunks(4).any(|p| p[3] > 127))
            .map(|(i, _)| i)
            .collect();
        (rows[0], *rows.last().unwrap())
    }

    #[test]
    fn wide_canvas_fills_the_height_and_states_match() {
        let (w, h) = (60, 44);
        let (top, bottom) = ink_rows(&rgba(IconState::NoReader, w, h, false), w);
        let fill = (bottom - top + 1) as f32 / h as f32;
        assert!((fill - MAX_CARD_FILL).abs() < 0.05, "card fills {fill} of the height");
        for state in [IconState::Ready, IconState::CardPresent] {
            assert_eq!(ink_rows(&rgba(state, w, h, false), w), (top, bottom));
        }
    }
}
