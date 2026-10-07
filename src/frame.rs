//! BGRX frames and the sprites drawn into them or found in them (the pointer,
//! our own windows). Shared by every capture backend.

/// Screen rectangle, half-open: (x0, y0, x1, y1).
pub type Rect = (i32, i32, i32, i32);

/// Half-open row range [y0, y1).
pub type Rows = (i32, i32);

/// An image at a screen position, premultiplied ARGB: the cursor or one of our windows.
pub struct Sprite {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub argb: Vec<u32>,
}

impl Sprite {
    /// (pixel, screen x, screen y) of every non-transparent pixel, drawn at (x, y).
    pub fn pixels_at(&self, x: i32, y: i32) -> impl Iterator<Item = (u32, i32, i32)> + '_ {
        (0..self.h)
            .flat_map(move |sy| (0..self.w).map(move |sx| (self.argb[(sy * self.w + sx) as usize], x + sx, y + sy)))
            .filter(|(p, ..)| p >> 24 != 0)
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // X11 un-blending
    pub fn covers(&self, x: i32, y: i32) -> bool {
        let (x, y) = (x - self.x, y - self.y);
        x >= 0 && y >= 0 && x < self.w && y < self.h && self.argb[(y * self.w + x) as usize] >> 24 != 0
    }
}

/// Where a BGRX frame sits on screen: w×h pixels with top-left at (x0, y0).
#[derive(Clone, Copy)]
pub struct View {
    pub w: usize,
    pub h: usize,
    pub x0: i32,
    pub y0: i32,
}

impl View {
    /// Byte offset of screen pixel (x, y), if it is in the frame.
    pub fn at(&self, x: i32, y: i32) -> Option<usize> {
        let (x, y) = (x - self.x0, y - self.y0);
        (x >= 0 && y >= 0 && (x as usize) < self.w && (y as usize) < self.h).then(|| (y as usize * self.w + x as usize) * 4)
    }

    /// Screen rows this frame covers.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))] // X11 damage tracking
    pub fn rows(&self) -> Rows {
        (self.y0, self.y0 + self.h as i32)
    }
}

/// Whether most opaque pixels of `s` are in the frame, verbatim.
pub fn shows(f: &[u8], v: View, s: &Sprite) -> bool {
    let (mut n, mut hit) = (0, 0);
    for (p, x, y) in s.pixels_at(s.x, s.y).filter(|(p, ..)| p >> 24 == 255) {
        if let Some(i) = v.at(x, y) {
            n += 1;
            hit += (f[i..i + 3] == p.to_le_bytes()[..3]) as usize;
        }
    }
    n > 0 && hit * 2 >= n
}

/// Alpha-blend `s` over the frame.
pub fn draw(f: &mut [u8], v: View, s: &Sprite) {
    for (p, x, y) in s.pixels_at(s.x, s.y) {
        if let Some(i) = v.at(x, y) {
            let a = p >> 24;
            for (k, d) in f[i..i + 3].iter_mut().enumerate() {
                *d = ((p >> (8 * k) & 255) + (*d as u32 * (255 - a) + 127) / 255) as u8;
            }
        }
    }
}

/// The rows where two frames (rows of `row_bytes`) differ, for backends that
/// get no damage events and compare each capture with the last one.
#[cfg_attr(target_os = "linux", allow(dead_code))] // X11 has DAMAGE
pub fn diff_rows(old: &[u8], new: &[u8], row_bytes: usize) -> Option<Rows> {
    let rows = || old.chunks_exact(row_bytes).zip(new.chunks_exact(row_bytes));
    let y0 = rows().position(|(a, b)| a != b)?;
    let y1 = rows().rposition(|(a, b)| a != b)? + 1;
    Some((y0 as i32, y1 as i32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_rows_finds_the_changed_band() {
        let old = vec![0u8; 6 * 32]; // 6 rows of 8 BGRX pixels
        let mut new = old.clone();
        assert_eq!(diff_rows(&old, &new, 32), None);
        new[2 * 32 + 5] = 1;
        assert_eq!(diff_rows(&old, &new, 32), Some((2, 3)));
        new[4 * 32] = 9;
        assert_eq!(diff_rows(&old, &new, 32), Some((2, 5)));
    }
}
