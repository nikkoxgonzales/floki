//! Floki's icon, computed (no image files): a blue rounded tile with a white
//! magnifying glass over three file-name lines. One source for the tray, the
//! window title bar and the `floki.exe` resource (`build.rs` includes this
//! file), so all three always match.

/// Straight-alpha RGBA pixels of the icon at `size`×`size`, anti-aliased
/// with 4×4 supersampling.
#[must_use]
pub fn render(size: u32) -> Vec<u8> {
    const SS: u32 = 4;
    let s = size as f32;
    // Strokes never thinner than ~1.5 px, so 16 px stays legible.
    let ring = 0.085f32.max(1.6 / s);
    let handle = 0.07f32.max(1.4 / s);
    // The lines inside the lens are noise below 32 px.
    let lines = size >= 32;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0f32; 4];
            for sy in 0..SS {
                for sx in 0..SS {
                    let u = (px as f32 + (sx as f32 + 0.5) / SS as f32) / s;
                    let v = (py as f32 + (sy as f32 + 0.5) / SS as f32) / s;
                    if let Some([r, g, b]) = sample(u, v, ring, handle, lines) {
                        acc[0] += r;
                        acc[1] += g;
                        acc[2] += b;
                        acc[3] += 1.0;
                    }
                }
            }
            if acc[3] == 0.0 {
                out.extend([0, 0, 0, 0]);
            } else {
                let to_u8 = |c: f32| (c * 255.0).round().clamp(0.0, 255.0) as u8;
                out.extend([
                    to_u8(acc[0] / acc[3]),
                    to_u8(acc[1] / acc[3]),
                    to_u8(acc[2] / acc[3]),
                    to_u8(acc[3] / (SS * SS) as f32),
                ]);
            }
        }
    }
    out
}

/// Colour at unit coordinates `(u, v)`, or `None` outside the tile.
fn sample(u: f32, v: f32, ring: f32, handle: f32, lines: bool) -> Option<[f32; 3]> {
    const WHITE: [f32; 3] = [1.0, 1.0, 1.0];
    const TOP: [f32; 3] = [0.36, 0.62, 1.0];
    const BOTTOM: [f32; 3] = [0.15, 0.32, 0.77];
    const LENS: (f32, f32, f32) = (0.43, 0.43, 0.255);

    // Rounded square: half-size 0.47, corner radius 0.2.
    let qx = ((u - 0.5).abs() - 0.27).max(0.0);
    let qy = ((v - 0.5).abs() - 0.27).max(0.0);
    if qx.hypot(qy) > 0.2 {
        return None;
    }
    let base = mix(TOP, BOTTOM, v);
    let (cx, cy, r_out) = LENS;
    let d = (u - cx).hypot(v - cy);
    if d <= r_out && d >= r_out - ring {
        return Some(WHITE);
    }
    if segment_dist(u, v, (0.6, 0.6), (0.79, 0.79)) <= handle {
        return Some(WHITE);
    }
    if d < r_out - ring {
        let line = |y: f32, x1: f32| segment_dist(u, v, (0.33, y), (x1, y)) <= 0.021;
        if lines && (line(0.36, 0.53) || line(0.43, 0.5) || line(0.5, 0.45)) {
            return Some(mix(base, WHITE, 0.9));
        }
        // Glass: a faint white tint.
        return Some(mix(base, WHITE, 0.16));
    }
    Some(base)
}

fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t)
}

/// Distance from `(u, v)` to the segment `a`–`b`.
fn segment_dist(u: f32, v: f32, a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let t = (((u - a.0) * dx + (v - a.1) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
    (u - (a.0 + t * dx)).hypot(v - (a.1 + t * dy))
}

#[cfg(test)]
mod tests {
    use super::render;

    fn px(rgba: &[u8], size: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    #[test]
    fn every_size_has_the_right_length() {
        for size in [16, 24, 32, 48, 64, 256] {
            assert_eq!(render(size).len(), (size * size * 4) as usize);
        }
    }

    #[test]
    fn corners_are_transparent_and_tile_is_opaque() {
        let rgba = render(32);
        assert_eq!(px(&rgba, 32, 0, 0)[3], 0);
        assert_eq!(px(&rgba, 32, 31, 31)[3], 0);
        assert_eq!(px(&rgba, 32, 28, 4)[3], 255);
    }

    #[test]
    fn lens_ring_is_white_and_background_is_blue() {
        let rgba = render(256);
        // Left edge of the ring: x = (0.43 - 0.255 + 0.04) * 256 ≈ 55.
        let ring = px(&rgba, 256, 55, 110);
        assert!(ring[..3].iter().all(|&c| c > 240), "{ring:?}");
        // Bottom-left background: blue dominates.
        let bg = px(&rgba, 256, 40, 220);
        assert!(bg[2] > bg[0] + 60, "{bg:?}");
    }
}
