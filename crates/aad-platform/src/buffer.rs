//! Pictures kept between captures: the next one is made from the one before
//! last, caught up on what changed since, instead of copied whole.
use crate::damage::{Damage, Move, Rect};
use crate::Raw;
use std::sync::Arc;

/// A part of the screen: left, top, width, height in physical pixels.
pub type Bounds = (i32, i32, u32, u32);

/// Where a capture writes: the next picture, made on first use from the one
/// before last when no one holds that any more. It then only needs the
/// regions the last capture changed, not a copy of the whole screen.
pub struct Target<'a> {
    pub frame: &'a mut Option<Arc<Vec<u8>>>,
    pub spare: &'a mut Option<Arc<Vec<u8>>>,
    /// Where the spare differs from the frame.
    pub stale: &'a mut Vec<Rect>,
    pub width: usize,
    pub height: usize,
    pub ready: bool,
}

impl Target<'_> {
    pub fn pixels(&mut self) -> &mut Vec<u8> {
        if !self.ready {
            self.ready = true;
            let size = self.width * self.height * 4;
            let next = match (self.spare.take(), self.frame.as_ref()) {
                // No other holder (no weak references are ever made).
                (Some(mut spare), Some(frame))
                    if spare.len() == size && Arc::strong_count(&spare) == 1 =>
                {
                    let pixels = Arc::get_mut(&mut spare).unwrap_or_else(|| unreachable!());
                    for rect in self.stale.iter() {
                        for y in rect.y..rect.y + rect.h {
                            let at = (y as usize * self.width + rect.x as usize) * 4;
                            let span = at..at + rect.w as usize * 4;
                            pixels[span.clone()].copy_from_slice(&frame[span]);
                        }
                    }
                    spare
                }
                (_, Some(frame)) => Arc::new(frame.as_ref().clone()),
                (_, None) => Arc::new(vec![0u8; size]),
            };
            self.stale.clear();
            *self.spare = self.frame.replace(next);
        }
        let frame = self.frame.as_mut().unwrap_or_else(|| unreachable!());
        Arc::get_mut(frame).unwrap_or_else(|| unreachable!())
    }
}

/// The shared part cut out of the whole desktop, double buffered like the
/// desktop itself, so a change costs only its own rows.
#[derive(Default)]
pub struct Crop {
    pub at: Option<Bounds>,
    frame: Option<Arc<Vec<u8>>>,
    spare: Option<Arc<Vec<u8>>>,
    stale: Vec<Rect>,
}

impl Crop {
    pub fn cut(&mut self, whole: &Raw, desktop: Bounds, at: Bounds) -> Raw {
        let offset = ((at.0 - desktop.0) as u32, (at.1 - desktop.1) as u32);
        let inside = |r: &Rect| -> Option<Rect> {
            let left = r.x.max(offset.0);
            let top = r.y.max(offset.1);
            let right = (r.x + r.w).min(offset.0 + at.2);
            let bottom = (r.y + r.h).min(offset.1 + at.3);
            (right > left && bottom > top).then(|| Rect {
                x: left - offset.0,
                y: top - offset.1,
                w: right - left,
                h: bottom - top,
            })
        };
        let full = Rect {
            x: 0,
            y: 0,
            w: at.2,
            h: at.3,
        };
        let same_place = self.at == Some(at) && self.frame.is_some();
        let damage = match (&whole.damage, same_place) {
            (Some(damage), true) => {
                let mut cut = Damage::default();
                for m in &damage.moves {
                    let from = Rect {
                        x: m.from.0,
                        y: m.from.1,
                        w: m.to.w,
                        h: m.to.h,
                    };
                    match (inside(&from), inside(&m.to)) {
                        (Some(f), Some(t))
                            if f.w == m.to.w && f.h == m.to.h && t.w == m.to.w && t.h == m.to.h =>
                        {
                            cut.moves.push(Move {
                                from: (f.x, f.y),
                                to: t,
                            });
                        }
                        (_, Some(t)) => cut.dirty.push(t),
                        _ => {}
                    }
                }
                cut.dirty.extend(damage.dirty.iter().filter_map(inside));
                Some(cut)
            }
            _ => None,
        };
        if !same_place {
            *self = Crop {
                at: Some(at),
                ..Crop::default()
            };
        }
        let copy: Vec<Rect> = match &damage {
            Some(d) => d
                .dirty
                .iter()
                .chain(d.moves.iter().map(|m| &m.to))
                .copied()
                .collect(),
            None => vec![full],
        };
        let mut target = Target {
            frame: &mut self.frame,
            spare: &mut self.spare,
            stale: &mut self.stale,
            width: at.2 as usize,
            height: at.3 as usize,
            ready: false,
        };
        if !copy.is_empty() {
            let pixels = target.pixels();
            let stride = whole.width as usize * 4;
            for r in &copy {
                for y in r.y..r.y + r.h {
                    let from = ((offset.1 + y) as usize) * stride + ((offset.0 + r.x) as usize) * 4;
                    let to = (y as usize * at.2 as usize + r.x as usize) * 4;
                    pixels[to..to + r.w as usize * 4]
                        .copy_from_slice(&whole.pixels[from..from + r.w as usize * 4]);
                }
            }
            self.stale = copy;
        }
        Raw {
            pixels: self.frame.clone().unwrap_or_default(),
            width: at.2,
            height: at.3,
            format: whole.format,
            damage,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spare_picture_is_caught_up_instead_of_copied() {
        let (width, height) = (4usize, 2usize);
        let mut frame = Some(Arc::new(vec![1u8; width * height * 4]));
        // The picture before last: out of date in its second row.
        let mut old = vec![1u8; width * height * 4];
        old[width * 4..].fill(9);
        let mut spare = Some(Arc::new(old));
        let spare_at = Arc::as_ptr(spare.as_ref().unwrap());
        let mut stale = vec![Rect {
            x: 0,
            y: 1,
            w: 4,
            h: 1,
        }];
        let mut target = Target {
            frame: &mut frame,
            spare: &mut spare,
            stale: &mut stale,
            width,
            height,
            ready: false,
        };
        let pixels = target.pixels();
        assert!(pixels.iter().all(|p| *p == 1));
        pixels[0] = 5;
        // The spare's buffer became the new picture; the old picture is the spare.
        assert_eq!(Arc::as_ptr(frame.as_ref().unwrap()), spare_at);
        assert_eq!(frame.as_ref().unwrap()[0], 5);
        assert_eq!(spare.as_ref().unwrap()[0], 1);
        assert!(stale.is_empty());

        // Someone still holds the spare: the picture is copied instead.
        let held = spare.clone();
        let mut target = Target {
            frame: &mut frame,
            spare: &mut spare,
            stale: &mut stale,
            width,
            height,
            ready: false,
        };
        assert_eq!(target.pixels()[0], 5);
        assert_ne!(
            Arc::as_ptr(frame.as_ref().unwrap()),
            Arc::as_ptr(held.as_ref().unwrap())
        );
    }
}
