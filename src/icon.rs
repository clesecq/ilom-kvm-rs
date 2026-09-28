//! Application icon, drawn in code so no image asset has to ship.
//!
//! A monitor on a blue rounded tile, with a green prompt cursor on its
//! screen. Shapes are rounded rectangles rendered with a signed distance
//! function, which gives anti-aliased edges without an image library.

use eframe::egui::IconData;

pub const SIZE: u32 = 128;

/// Rounded rectangle centred on (`cx`, `cy`) with half extents `hw`/`hh`.
struct RoundRect {
    cx: f32,
    cy: f32,
    hw: f32,
    hh: f32,
    radius: f32,
    color: [u8; 3],
}

impl RoundRect {
    /// Coverage of pixel centre (`x`, `y`), 0.0 outside to 1.0 inside.
    fn coverage(&self, x: f32, y: f32) -> f32 {
        let dx = (x - self.cx).abs() - (self.hw - self.radius);
        let dy = (y - self.cy).abs() - (self.hh - self.radius);
        let outside = dx.max(0.0).hypot(dy.max(0.0));
        let distance = outside + dx.max(dy).min(0.0) - self.radius;
        (0.5 - distance).clamp(0.0, 1.0)
    }
}

pub fn rgba() -> Vec<u8> {
    let s = SIZE as f32 / 128.0;
    let shape = |cx: f32, cy: f32, hw: f32, hh: f32, radius: f32, color| RoundRect {
        cx: cx * s,
        cy: cy * s,
        hw: hw * s,
        hh: hh * s,
        radius: radius * s,
        color,
    };
    // Painted back to front.
    let shapes = [
        shape(64.0, 64.0, 60.0, 60.0, 26.0, [37, 99, 235]), // tile
        shape(64.0, 96.0, 7.0, 10.0, 2.0, [203, 213, 225]), // stand neck
        shape(64.0, 104.0, 22.0, 4.0, 4.0, [203, 213, 225]), // stand foot
        shape(64.0, 56.0, 42.0, 32.0, 7.0, [226, 232, 240]), // bezel
        shape(64.0, 56.0, 36.0, 26.0, 3.0, [15, 23, 42]),   // screen
        shape(44.0, 54.0, 8.0, 2.5, 1.0, [74, 222, 128]),   // prompt "_"
        shape(60.0, 50.0, 4.5, 6.5, 1.0, [74, 222, 128]),   // cursor block
    ];
    let mut pixels = vec![0_u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let mut color = [0.0_f32; 3];
            let mut alpha = 0.0_f32;
            for shape in &shapes {
                let a = shape.coverage(px, py);
                if a == 0.0 {
                    continue;
                }
                // "Over" compositing in straight alpha.
                let out_alpha = a + alpha * (1.0 - a);
                for (channel, &value) in color.iter_mut().zip(&shape.color) {
                    *channel =
                        (value as f32 * a + *channel * alpha * (1.0 - a)) / out_alpha.max(1e-6);
                }
                alpha = out_alpha;
            }
            let out = &mut pixels[((y * SIZE + x) * 4) as usize..][..4];
            for (target, channel) in out.iter_mut().zip(color) {
                *target = channel.round() as u8;
            }
            out[3] = (alpha * 255.0).round() as u8;
        }
    }
    pixels
}

pub fn icon_data() -> IconData {
    IconData {
        rgba: rgba(),
        width: SIZE,
        height: SIZE,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corners_are_transparent_and_centre_is_opaque() {
        let pixels = rgba();
        let at = |x: u32, y: u32| &pixels[((y * SIZE + x) * 4) as usize..][..4];
        assert_eq!(at(0, 0)[3], 0);
        assert_eq!(at(SIZE / 2, SIZE / 2)[3], 255);
    }
}
