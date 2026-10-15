//! Times drawn on contact sheets: a 5×7 pixel font of the characters a time
//! needs, since Homebrew's ffmpeg has no text filter (no freetype).

/// Each glyph is 7 rows of 5 bits, the leftmost pixel the highest bit.
fn glyph(c: char) -> [u8; 7] {
    match c {
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        _ => [0; 7],
    }
}

/// An RGB picture, 3 bytes a pixel, row after row.
pub struct Picture {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u8>,
}

impl Picture {
    pub fn new(width: usize, height: usize, rgb: [u8; 3]) -> Picture {
        Picture {
            width,
            height,
            pixels: rgb
                .iter()
                .copied()
                .cycle()
                .take(width * height * 3)
                .collect(),
        }
    }

    pub fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, rgb: [u8; 3]) {
        for row in y..(y + h).min(self.height) {
            for col in x..(x + w).min(self.width) {
                let at = (row * self.width + col) * 3;
                self.pixels[at..at + 3].copy_from_slice(&rgb);
            }
        }
    }

    /// Copies `rgb` (`w`×`h`, 3 bytes a pixel) in with its top left at x, y.
    pub fn paste(&mut self, x: usize, y: usize, w: usize, h: usize, rgb: &[u8]) {
        for row in 0..h.min(self.height.saturating_sub(y)) {
            let n = w.min(self.width.saturating_sub(x)) * 3;
            let from = row * w * 3;
            let to = ((y + row) * self.width + x) * 3;
            if from + n <= rgb.len() {
                self.pixels[to..to + n].copy_from_slice(&rgb[from..from + n]);
            }
        }
    }

    /// `text` in white on a black box, its top left at x, y, each font
    /// pixel `scale` pixels. Returns the box's width and height.
    pub fn label(&mut self, x: usize, y: usize, text: &str, scale: usize) -> (usize, usize) {
        let pad = scale * 2;
        let advance = 6 * scale;
        let (w, h) = (
            text.chars().count() * advance - scale + 2 * pad,
            7 * scale + 2 * pad,
        );
        self.fill(x, y, w, h, [0, 0, 0]);
        for (i, c) in text.chars().enumerate() {
            for (r, bits) in glyph(c).iter().enumerate() {
                for b in 0..5 {
                    if bits & (0x10 >> b) != 0 {
                        self.fill(
                            x + pad + i * advance + b * scale,
                            y + pad + r * scale,
                            scale,
                            scale,
                            [255, 255, 255],
                        );
                    }
                }
            }
        }
        (w, h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn draws_inside() {
        let mut p = Picture::new(40, 20, [9, 9, 9]);
        let (w, h) = p.label(1, 1, "1:2", 1);
        assert_eq!((w, h), (21, 11));
        // The box's corner is black, the "1"'s top white.
        assert_eq!(&p.pixels[(40 + 1) * 3..(40 + 1) * 3 + 3], &[0, 0, 0]);
        let at = ((1 + 2) * 40 + 1 + 2 + 2) * 3;
        assert_eq!(&p.pixels[at..at + 3], &[255, 255, 255]);
        // Past the edge: cut, not a panic.
        p.label(35, 15, "12:34:56", 2);
        p.paste(30, 18, 20, 5, &[1; 20 * 5 * 3]);
        assert_eq!(p.pixels.len(), 40 * 20 * 3);
    }
}
