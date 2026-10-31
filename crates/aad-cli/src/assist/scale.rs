//! The screen scaled down, redoing only what changed.
//!
//! A 4K desktop is nine million pixels; scaling all of them for every frame
//! costs more than encoding the result. The scaled picture is kept and only
//! the regions a change touched are averaged again from the full one.
use super::damage::{Damage, Rect};
use super::{PixelFormat, Raw};
use std::sync::Arc;

/// For each output column (or row), the input columns it averages.
fn spans(from: u32, to: u32) -> Vec<(u32, u32)> {
    (0..to)
        .map(|o| {
            let start = (u64::from(o) * u64::from(from) / u64::from(to)) as u32;
            let end =
                ((u64::from(o + 1) * u64::from(from)).div_ceil(u64::from(to)) as u32).min(from);
            (start, end.max(start + 1))
        })
        .collect()
}

#[derive(Default)]
pub struct Downscale {
    shape: Option<(u32, u32, PixelFormat, u32, u32)>,
    columns: Vec<(u32, u32)>,
    rows: Vec<(u32, u32)>,
    pixels: Option<Arc<Vec<u8>>>,
}

impl Downscale {
    /// `raw` at `size`. Outside `damage` the last result is kept, so
    /// `damage` must be what changed since the picture given last time;
    /// `None` scales everything.
    pub fn apply(&mut self, raw: &Raw, damage: Option<&Damage>, size: (u32, u32)) -> Raw {
        let shape = (raw.width, raw.height, raw.format, size.0, size.1);
        let mut damage = damage;
        if self.shape != Some(shape) {
            self.shape = Some(shape);
            self.columns = spans(raw.width, size.0);
            self.rows = spans(raw.height, size.1);
            self.pixels = None;
            damage = None;
        }
        let whole = [Rect {
            x: 0,
            y: 0,
            w: raw.width,
            h: raw.height,
        }];
        let rects: Vec<Rect> = match damage {
            Some(d) => d
                .dirty
                .iter()
                .chain(d.moves.iter().map(|m| &m.to))
                .copied()
                .collect(),
            None => whole.to_vec(),
        };
        let pixels = Arc::make_mut(
            self.pixels
                .get_or_insert_with(|| Arc::new(vec![0u8; size.0 as usize * size.1 as usize * 4])),
        );
        let stride = raw.width as usize * 4;
        for rect in rects.iter().filter_map(|r| r.clip(raw.width, raw.height)) {
            // The output pixels whose spans reach into the rectangle.
            let left = (u64::from(rect.x) * u64::from(size.0) / u64::from(raw.width)) as usize;
            let right = ((u64::from(rect.x + rect.w) * u64::from(size.0))
                .div_ceil(u64::from(raw.width)) as usize)
                .min(size.0 as usize);
            let top = (u64::from(rect.y) * u64::from(size.1) / u64::from(raw.height)) as usize;
            let bottom = ((u64::from(rect.y + rect.h) * u64::from(size.1))
                .div_ceil(u64::from(raw.height)) as usize)
                .min(size.1 as usize);
            for oy in top..bottom {
                let (y0, y1) = self.rows[oy];
                for ox in left..right {
                    let (x0, x1) = self.columns[ox];
                    let mut sum = [0u32; 4];
                    for y in y0..y1 {
                        let line = &raw.pixels[y as usize * stride..];
                        for x in x0..x1 {
                            let p = &line[x as usize * 4..x as usize * 4 + 4];
                            sum[0] += u32::from(p[0]);
                            sum[1] += u32::from(p[1]);
                            sum[2] += u32::from(p[2]);
                            sum[3] += u32::from(p[3]);
                        }
                    }
                    let count = (y1 - y0) * (x1 - x0);
                    let at = (oy * size.0 as usize + ox) * 4;
                    for (channel, total) in sum.iter().enumerate() {
                        pixels[at + channel] = (total / count) as u8;
                    }
                }
            }
        }
        Raw {
            pixels: self.pixels.clone().unwrap_or_default(),
            width: size.0,
            height: size.1,
            format: raw.format,
            damage: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(width: u32, height: u32, value: impl Fn(u32, u32) -> u8) -> Raw {
        let mut pixels = Vec::new();
        for y in 0..height {
            for x in 0..width {
                let v = value(x, y);
                pixels.extend_from_slice(&[v, v, v, 255]);
            }
        }
        Raw {
            pixels: Arc::new(pixels),
            width,
            height,
            format: PixelFormat::Bgra,
            damage: None,
        }
    }

    #[test]
    fn redoing_only_the_change_gives_the_same_as_scaling_everything() {
        let before = picture(97, 61, |x, y| ((x * 7 + y * 3) % 256) as u8);
        let after = picture(97, 61, |x, y| {
            if (40..50).contains(&x) && (20..30).contains(&y) {
                255
            } else {
                ((x * 7 + y * 3) % 256) as u8
            }
        });
        let mut incremental = Downscale::default();
        incremental.apply(&before, None, (40, 25));
        let change = Damage {
            moves: Vec::new(),
            dirty: vec![Rect {
                x: 40,
                y: 20,
                w: 10,
                h: 10,
            }],
        };
        let partial = incremental.apply(&after, Some(&change), (40, 25));
        let full = Downscale::default().apply(&after, None, (40, 25));
        assert_eq!(partial.pixels, full.pixels);
        assert_eq!((partial.width, partial.height), (40, 25));
    }

    #[test]
    fn every_input_pixel_is_averaged_into_some_output() {
        let spans = spans(10, 4);
        assert_eq!(spans.first().unwrap().0, 0);
        assert_eq!(spans.last().unwrap().1, 10);
        assert!(spans.windows(2).all(|w| w[0].1 >= w[1].0));
        let flat = picture(10, 10, |_, _| 77);
        let scaled = Downscale::default().apply(&flat, None, (4, 4));
        assert!(scaled
            .pixels
            .chunks_exact(4)
            .all(|p| p == [77, 77, 77, 255]));
    }
}
