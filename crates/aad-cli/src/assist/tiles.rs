//! The `tiles` codec: only the parts of the screen that changed, drawn over
//! the picture the viewer already shows.
//!
//! A frame's payload is a u16 count of patches, each patch big-endian:
//! kind u8 (1 = picture, 2 = copy), format u8 (picture: 2 = WebP, 0 for a
//! copy), x, y, w, h u16 in frame pixels, then for a copy the source x, y u16
//! (pixels the viewer already has, moved, e.g. by scrolling) and for a picture
//! a u32 length and an encoded picture exactly w by h. Patches are drawn in
//! order; a keyframe's single patch covers the whole frame.
use super::damage::{Damage, Mask, Rect};
use super::{Codec, Frame, PixelFormat, Raw, Result};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};

pub const PICTURE: u8 = 1;
pub const COPY: u8 = 2;

/// RGB for a region of a raw picture.
fn rgb(raw: &Raw, rect: Rect) -> Vec<u8> {
    let (r, b) = match raw.format {
        PixelFormat::Bgra => (2, 0),
        PixelFormat::Rgba => (0, 2),
    };
    let stride = raw.width as usize * 4;
    let mut out = Vec::with_capacity(rect.area() as usize * 3);
    for y in rect.y..rect.y + rect.h {
        let at = y as usize * stride + rect.x as usize * 4;
        for p in raw.pixels[at..at + rect.w as usize * 4].chunks_exact(4) {
            out.extend_from_slice(&[p[r], p[1], p[b]]);
        }
    }
    out
}

/// The region of `raw` behind `rect` of the output, scaled to it.
fn region(raw: &Raw, rect: Rect, scale: (f64, f64)) -> Result<image::RgbImage> {
    if scale == (1.0, 1.0) {
        return image::RgbImage::from_raw(rect.w, rect.h, rgb(raw, rect))
            .ok_or_else(|| "bad region".into());
    }
    let left = (f64::from(rect.x) / scale.0).floor() as u32;
    let top = (f64::from(rect.y) / scale.1).floor() as u32;
    let right = ((f64::from(rect.x + rect.w) / scale.0).ceil() as u32).min(raw.width);
    let bottom = ((f64::from(rect.y + rect.h) / scale.1).ceil() as u32).min(raw.height);
    let source = Rect {
        x: left,
        y: top,
        w: right.saturating_sub(left).max(1),
        h: bottom.saturating_sub(top).max(1),
    };
    let source =
        image::RgbImage::from_raw(source.w, source.h, rgb(raw, source)).ok_or("bad region")?;
    Ok(image::imageops::thumbnail(&source, rect.w, rect.h))
}

fn webp(image: &image::RgbImage, quality: u8) -> Vec<u8> {
    let encoder = webp::Encoder::from_rgb(image.as_raw(), image.width(), image.height());
    if quality >= 100 {
        encoder.encode_lossless().to_vec()
    } else {
        encoder.encode(f32::from(quality)).to_vec()
    }
}

fn push_u16(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&(value.min(u32::from(u16::MAX)) as u16).to_be_bytes());
}

/// One frame of patches for `damage` on `raw`, or the whole picture when
/// `full`. The picture is scaled to `fitted` and the frame is `size`, at most
/// a pixel less each way: cut to even for video, not resampled.
pub fn encode(
    raw: &Raw,
    damage: &Damage,
    full: bool,
    fitted: (u32, u32),
    size: (u32, u32),
    quality: u8,
) -> Result<Frame> {
    let scale = (
        f64::from(fitted.0) / f64::from(raw.width),
        f64::from(fitted.1) / f64::from(raw.height),
    );
    let whole = Rect {
        x: 0,
        y: 0,
        w: size.0,
        h: size.1,
    };
    let mut copies = Vec::new();
    let mut rects = Vec::new();
    if full {
        rects.push(whole);
    } else if scale == (1.0, 1.0) {
        copies = damage
            .moves
            .iter()
            .filter_map(|m| {
                let to = m.to.clip(size.0, size.1)?;
                Some(super::damage::Move { from: m.from, to })
            })
            .collect();
        rects = Mask::of(
            &Damage {
                moves: Vec::new(),
                dirty: damage.dirty.clone(),
            },
            raw.width,
            raw.height,
        )
        .rects()
        .into_iter()
        .filter_map(|rect| rect.clip(size.0, size.1))
        .collect();
    } else {
        // Scaled moves would land a pixel off; redraw them instead.
        let changed = Mask::of(&damage.without_moves(), raw.width, raw.height).rects();
        for rect in changed {
            let left = (f64::from(rect.x) * scale.0).floor() as u32;
            let top = (f64::from(rect.y) * scale.1).floor() as u32;
            let right = (f64::from(rect.x + rect.w) * scale.0).ceil() as u32;
            let bottom = (f64::from(rect.y + rect.h) * scale.1).ceil() as u32;
            let scaled = Rect {
                x: left,
                y: top,
                w: right.saturating_sub(left).max(1),
                h: bottom.saturating_sub(top).max(1),
            };
            rects.extend(scaled.clip(size.0, size.1));
        }
    }
    let mut data = Vec::new();
    push_u16(&mut data, (copies.len() + rects.len()) as u32);
    for copy in &copies {
        data.extend_from_slice(&[COPY, 0]);
        for value in [
            copy.to.x,
            copy.to.y,
            copy.to.w,
            copy.to.h,
            copy.from.0,
            copy.from.1,
        ] {
            push_u16(&mut data, value);
        }
    }
    for rect in &rects {
        let picture = webp(&region(raw, *rect, scale)?, quality);
        data.extend_from_slice(&[PICTURE, Codec::Webp as u8]);
        for value in [rect.x, rect.y, rect.w, rect.h] {
            push_u16(&mut data, value);
        }
        data.extend_from_slice(&(picture.len() as u32).to_be_bytes());
        data.extend_from_slice(&picture);
    }
    Ok(Frame {
        codec: Codec::Tiles,
        keyframe: full,
        data,
        width: size.0,
        height: size.1,
        regions: None,
    })
}

struct Job {
    raw: Raw,
    damage: Damage,
    full: bool,
}

#[derive(Default)]
struct Slot {
    next: Option<Job>,
    working: bool,
    closed: bool,
}

/// Encodes patches on its own thread. A job replaced before it started is
/// merged into the newer one: its changes still have to reach the viewer.
pub struct Tiles {
    slot: Arc<(Mutex<Slot>, Condvar)>,
    frames: mpsc::Receiver<Result<Frame>>,
}

impl Tiles {
    /// `even`: frames of even size, as video next to them has.
    pub fn start(quality: u8, max: (u32, u32), even: bool) -> Self {
        let slot = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
        let (sender, frames) = mpsc::channel();
        {
            let slot = slot.clone();
            std::thread::spawn(move || {
                let (lock, wake) = &*slot;
                loop {
                    let job = {
                        let mut state = lock.lock().unwrap();
                        loop {
                            if state.closed {
                                return;
                            }
                            if let Some(job) = state.next.take() {
                                state.working = true;
                                break job;
                            }
                            state = wake.wait(state).unwrap();
                        }
                    };
                    let fitted = super::fit(job.raw.width, job.raw.height, max.0, max.1);
                    let size = if even {
                        ((fitted.0 & !1).max(2), (fitted.1 & !1).max(2))
                    } else {
                        fitted
                    };
                    let frame = encode(&job.raw, &job.damage, job.full, fitted, size, quality);
                    let sent = sender.send(frame);
                    lock.lock().unwrap().working = false;
                    if sent.is_err() {
                        return;
                    }
                }
            });
        }
        Self { slot, frames }
    }

    pub fn push(&self, raw: Raw, damage: Damage, full: bool) {
        let (lock, wake) = &*self.slot;
        let mut state = lock.lock().unwrap();
        let job = match state.next.take() {
            Some(mut pending)
                if (pending.raw.width, pending.raw.height) == (raw.width, raw.height) =>
            {
                pending.damage.merge(damage);
                Job {
                    raw,
                    damage: pending.damage,
                    full: pending.full || full,
                }
            }
            Some(_) => Job {
                raw,
                damage,
                full: true,
            },
            None => Job { raw, damage, full },
        };
        state.next = Some(job);
        wake.notify_one();
    }

    pub fn busy(&self) -> bool {
        let state = self.slot.0.lock().unwrap();
        state.next.is_some() || state.working
    }

    pub fn poll(&self) -> Result<Option<Frame>> {
        match self.frames.try_recv() {
            Ok(frame) => frame.map(Some),
            Err(_) => Ok(None),
        }
    }
}

impl Drop for Tiles {
    fn drop(&mut self) {
        let (lock, wake) = &*self.slot;
        lock.lock().unwrap().closed = true;
        wake.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::super::damage::Move;
    use super::*;

    /// Patches back from a payload: (kind, rect, source or picture).
    fn patches(data: &[u8]) -> Vec<(u8, Rect, Vec<u8>)> {
        let be = |at: usize| u32::from(u16::from_be_bytes([data[at], data[at + 1]]));
        let count = be(0);
        let mut at = 2;
        let mut out = Vec::new();
        for _ in 0..count {
            let kind = data[at];
            let rect = Rect {
                x: be(at + 2),
                y: be(at + 4),
                w: be(at + 6),
                h: be(at + 8),
            };
            at += 10;
            if kind == COPY {
                out.push((kind, rect, data[at..at + 4].to_vec()));
                at += 4;
            } else {
                let len = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
                out.push((kind, rect, data[at + 4..at + 4 + len].to_vec()));
                at += 4 + len;
            }
        }
        assert_eq!(at, data.len());
        out
    }

    fn raw(width: u32, height: u32) -> Raw {
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        for (index, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[(index % 251) as u8, 128, 200, 255]);
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
    fn only_changed_tiles_are_sent_and_moves_are_copies() {
        let picture = raw(200, 100);
        let damage = Damage {
            moves: vec![Move {
                from: (0, 20),
                to: Rect {
                    x: 0,
                    y: 0,
                    w: 200,
                    h: 60,
                },
            }],
            dirty: vec![Rect {
                x: 70,
                y: 70,
                w: 3,
                h: 3,
            }],
        };
        let frame = encode(&picture, &damage, false, (200, 100), (200, 100), 80).unwrap();
        assert!(!frame.keyframe);
        let found = patches(&frame.data);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].0, COPY);
        assert_eq!(found[0].2, vec![0, 0, 0, 20]);
        assert_eq!(found[1].0, PICTURE);
        assert_eq!(
            found[1].1,
            Rect {
                x: 64,
                y: 64,
                w: 32,
                h: 32
            }
        );
        let decoded = image::load_from_memory(&found[1].2).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (32, 32));
    }

    #[test]
    fn scaled_patches_cover_their_region_and_keyframes_the_whole() {
        let picture = raw(400, 200);
        let damage = Damage {
            moves: vec![Move {
                from: (0, 0),
                to: Rect {
                    x: 0,
                    y: 100,
                    w: 64,
                    h: 64,
                },
            }],
            dirty: vec![Rect {
                x: 100,
                y: 10,
                w: 10,
                h: 10,
            }],
        };
        let frame = encode(&picture, &damage, false, (200, 100), (200, 100), 80).unwrap();
        let found = patches(&frame.data);
        // The move is redrawn, not copied, once scaled.
        assert!(found.iter().all(|p| p.0 == PICTURE));
        assert!(found.iter().any(|p| p.1
            == Rect {
                x: 48,
                y: 0,
                w: 16,
                h: 16
            }));
        for (_, rect, picture) in &found {
            let decoded = image::load_from_memory(picture).unwrap();
            assert_eq!((decoded.width(), decoded.height()), (rect.w, rect.h));
        }
        let key = encode(
            &picture,
            &Damage::default(),
            true,
            (200, 100),
            (200, 100),
            100,
        )
        .unwrap();
        assert!(key.keyframe);
        let found = patches(&key.data);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].1,
            Rect {
                x: 0,
                y: 0,
                w: 200,
                h: 100
            }
        );
    }
}
