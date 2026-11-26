//! Pixel maths with no OS calls for the performance overlay: its canvas (premultiplied RGBA) and
//! hardware-name shortening. Adapted from the Neptune overlay (dudegio/Neptune-GDeflate) as
//! carried by soldsawn/victory-nvidia-8gb (commit 7c55a8c1), both GPL-3.0.

/// A premultiplied RGBA bitmap, top-down.
pub(crate) struct Bitmap {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

impl Bitmap {
    pub fn new(w: usize, h: usize) -> Self {
        Bitmap { w, h, px: vec![0; w * h * 4] }
    }

    /// Lays `rgb` (straight colour) over pixel (x, y) with opacity `a` (0..=255).
    pub fn over(&mut self, x: usize, y: usize, rgb: [u8; 3], a: u32) {
        if x >= self.w || y >= self.h || a == 0 {
            return;
        }
        let p = &mut self.px[(y * self.w + x) * 4..][..4];
        let keep = 255 - a;
        for i in 0..3 {
            p[i] = ((u32::from(rgb[i]) * a + u32::from(p[i]) * keep + 127) / 255) as u8;
        }
        p[3] = (a + (u32::from(p[3]) * keep + 127) / 255) as u8;
    }

    /// A rounded rectangle with anti-aliased edges, `rgb` at opacity `a`.
    pub fn fill_round(&mut self, (x0, y0, w, h): (usize, usize, usize, usize), radius: f32, rgb: [u8; 3], a: u32) {
        for y in 0..h {
            for x in 0..w {
                let c = round_coverage(x as f32 + 0.5, y as f32 + 0.5, w as f32, h as f32, radius);
                self.over(x0 + x, y0 + y, rgb, (c * a as f32 + 0.5) as u32);
            }
        }
    }
}

/// Coverage (0..1) of pixel centre (x, y) by a `w` x `h` rectangle with corner radius `r`.
pub(crate) fn round_coverage(x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let (qx, qy) = ((x - w / 2.0).abs() - (w / 2.0 - r), (y - h / 2.0).abs() - (h / 2.0 - r));
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let d = outside + qx.max(qy).min(0.0) - r;
    (0.5 - d).clamp(0.0, 1.0)
}

/// "AMD Ryzen 7 9800X3D 8-Core Processor" -> "Ryzen 7 9800X3D".
pub(crate) fn short_cpu(name: &str) -> String {
    let cleaned = name.replace("(R)", "").replace("(TM)", "").replace("(tm)", "");
    let cleaned = cleaned.split('@').next().unwrap_or("").trim();
    let words: Vec<&str> = cleaned
        .split_whitespace()
        .filter(|w| !["AMD", "Intel", "CPU", "Processor"].contains(w) && !w.ends_with("-Core"))
        .collect();
    if words.is_empty() { name.trim().to_string() } else { words.join(" ") }
}

/// "NVIDIA GeForce RTX 5070 Ti" -> "RTX 5070 Ti".
pub(crate) fn short_gpu(name: &str) -> String {
    let words: Vec<&str> = name
        .split_whitespace()
        .filter(|w| !["NVIDIA", "GeForce", "AMD", "(TM)", "(R)"].contains(w))
        .collect();
    if words.is_empty() { name.trim().to_string() } else { words.join(" ") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_blends_premultiplied() {
        let mut bm = Bitmap::new(2, 1);
        bm.over(0, 0, [255, 255, 255], 255);
        bm.over(1, 0, [0, 0, 0], 128);
        assert_eq!(&bm.px[..4], [255, 255, 255, 255]);
        assert_eq!(&bm.px[4..], [0, 0, 0, 128]);
        // Off the bitmap: ignored, not a panic.
        bm.over(5, 5, [1, 2, 3], 255);
    }

    #[test]
    fn rounded_corners() {
        assert_eq!(round_coverage(50.0, 20.0, 100.0, 40.0, 8.0), 1.0);
        assert_eq!(round_coverage(0.5, 0.5, 100.0, 40.0, 8.0), 0.0);
        assert_eq!(round_coverage(50.0, 0.5, 100.0, 40.0, 8.0), 1.0);
    }

    #[test]
    fn names_are_shortened() {
        assert_eq!(short_cpu("AMD Ryzen 7 9800X3D 8-Core Processor"), "Ryzen 7 9800X3D");
        assert_eq!(short_cpu("Intel(R) Core(TM) i9-14900K @ 3.20GHz"), "Core i9-14900K");
        assert_eq!(short_gpu("NVIDIA GeForce RTX 3050"), "RTX 3050");
        assert_eq!(short_gpu("AMD Radeon RX 7900 XTX"), "Radeon RX 7900 XTX");
    }
}
