//! What changed between two pictures, as rectangles.
//!
//! A source that knows (the Windows desktop through DXGI) reports its own
//! changes, including regions that only moved, such as a scrolled page; any
//! other source is compared with its previous picture tile by tile.

/// A rectangle in picture pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn area(&self) -> u64 {
        u64::from(self.w) * u64::from(self.h)
    }

    /// The same rectangle in a picture scaled by `scale`, grown to whole
    /// pixels.
    pub fn scale(self, scale: (f64, f64)) -> Rect {
        let left = (f64::from(self.x) * scale.0).floor() as u32;
        let top = (f64::from(self.y) * scale.1).floor() as u32;
        let right = (f64::from(self.x + self.w) * scale.0).ceil() as u32;
        let bottom = (f64::from(self.y + self.h) * scale.1).ceil() as u32;
        Rect {
            x: left,
            y: top,
            w: right.saturating_sub(left).max(1),
            h: bottom.saturating_sub(top).max(1),
        }
    }

    /// The part inside a picture of this size, if any.
    pub fn clip(self, width: u32, height: u32) -> Option<Rect> {
        let right = (self.x.saturating_add(self.w)).min(width);
        let bottom = (self.y.saturating_add(self.h)).min(height);
        (self.x < right && self.y < bottom).then(|| Rect {
            x: self.x,
            y: self.y,
            w: right - self.x,
            h: bottom - self.y,
        })
    }
}

/// Pixels that moved: the destination's content was at `from` before.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Move {
    pub from: (u32, u32),
    pub to: Rect,
}

/// Changes since the previous picture: moves first, then redrawn regions.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Damage {
    pub moves: Vec<Move>,
    pub dirty: Vec<Rect>,
}

impl Damage {
    pub fn is_empty(&self) -> bool {
        self.moves.is_empty() && self.dirty.is_empty()
    }

    /// Adds changes that came after these. Moves cannot be composed with
    /// what came before them, so both sides' moves become redrawn regions.
    pub fn merge(&mut self, later: Damage) {
        let moved: Vec<Rect> = self
            .moves
            .drain(..)
            .chain(later.moves)
            .map(|m| m.to)
            .collect();
        self.dirty.extend(moved);
        self.dirty.extend(later.dirty);
    }

    /// The same changes with every move redrawn instead.
    pub fn without_moves(&self) -> Damage {
        let mut dirty = self.dirty.clone();
        dirty.extend(self.moves.iter().map(|m| m.to));
        Damage {
            moves: Vec::new(),
            dirty,
        }
    }
}

/// Side of the square tiles changes are tracked in.
pub const TILE: u32 = 32;

/// Which tiles of a picture changed.
#[derive(Clone, Debug)]
pub struct Mask {
    width: u32,
    height: u32,
    columns: u32,
    rows: u32,
    tiles: Vec<bool>,
}

impl Mask {
    pub fn new(width: u32, height: u32) -> Self {
        let columns = width.div_ceil(TILE);
        let rows = height.div_ceil(TILE);
        Self {
            width,
            height,
            columns,
            rows,
            tiles: vec![false; (columns * rows) as usize],
        }
    }

    pub fn of(damage: &Damage, width: u32, height: u32) -> Self {
        let mut mask = Self::new(width, height);
        for rect in damage
            .dirty
            .iter()
            .chain(damage.moves.iter().map(|m| &m.to))
        {
            mask.mark(*rect);
        }
        mask
    }

    pub fn mark(&mut self, rect: Rect) {
        let Some(rect) = rect.clip(self.width, self.height) else {
            return;
        };
        for row in rect.y / TILE..=(rect.y + rect.h - 1) / TILE {
            for column in rect.x / TILE..=(rect.x + rect.w - 1) / TILE {
                self.tiles[(row * self.columns + column) as usize] = true;
            }
        }
    }

    pub fn union(&mut self, other: &Mask) {
        if (other.width, other.height) != (self.width, self.height) {
            *self = Mask::new(self.width, self.height);
            self.mark(Rect {
                x: 0,
                y: 0,
                w: self.width,
                h: self.height,
            });
            return;
        }
        for (mine, theirs) in self.tiles.iter_mut().zip(&other.tiles) {
            *mine |= *theirs;
        }
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        !self.tiles.contains(&true)
    }

    /// The share of the picture's area that changed, 0..1.
    pub fn share(&self) -> f64 {
        let area: u64 = self.rects().iter().map(Rect::area).sum();
        let total = u64::from(self.width) * u64::from(self.height);
        if total == 0 {
            0.0
        } else {
            area as f64 / total as f64
        }
    }

    /// The changed tiles as few rectangles: runs along each row of tiles,
    /// joined downwards while they span the same columns.
    pub fn rects(&self) -> Vec<Rect> {
        let mut done = Vec::new();
        // Runs still growing downwards: (first column, last column, first row).
        let mut open: Vec<(u32, u32, u32)> = Vec::new();
        for row in 0..=self.rows {
            let mut runs = Vec::new();
            if row < self.rows {
                let mut column = 0;
                while column < self.columns {
                    if self.tiles[(row * self.columns + column) as usize] {
                        let start = column;
                        while column < self.columns
                            && self.tiles[(row * self.columns + column) as usize]
                        {
                            column += 1;
                        }
                        runs.push((start, column - 1));
                    } else {
                        column += 1;
                    }
                }
            }
            let mut next = Vec::new();
            for (first, last, top) in open.drain(..) {
                if let Some(index) = runs.iter().position(|run| *run == (first, last)) {
                    runs.remove(index);
                    next.push((first, last, top));
                } else {
                    let rect = Rect {
                        x: first * TILE,
                        y: top * TILE,
                        w: (last - first + 1) * TILE,
                        h: (row - top) * TILE,
                    };
                    done.extend(rect.clip(self.width, self.height));
                }
            }
            next.extend(runs.into_iter().map(|(first, last)| (first, last, row)));
            open = next;
        }
        done
    }
}

/// Whether one tile differs between two pictures of four bytes per pixel.
fn tile_differs(
    before: &[u8],
    after: &[u8],
    width: u32,
    height: u32,
    row: u32,
    column: u32,
) -> bool {
    let stride = width as usize * 4;
    let top = (row * TILE) as usize;
    let bottom = ((row + 1) * TILE).min(height) as usize;
    let left = (column * TILE) as usize * 4;
    let right = (((column + 1) * TILE).min(width) as usize) * 4;
    (top..bottom).any(|y| {
        let at = y * stride;
        before[at + left..at + right] != after[at + left..at + right]
    })
}

/// The tiles where two pictures of four bytes per pixel differ.
pub fn diff(before: &[u8], after: &[u8], width: u32, height: u32) -> Mask {
    let mut mask = Mask::new(width, height);
    for row in 0..mask.rows {
        for column in 0..mask.columns {
            if tile_differs(before, after, width, height, row, column) {
                mask.tiles[(row * mask.columns + column) as usize] = true;
            }
        }
    }
    mask
}

/// Redrawn regions cut down to the tiles that really differ: applications
/// often repaint what did not change, and comparing only inside what was
/// repainted costs little.
pub fn refine(before: &[u8], after: &[u8], width: u32, height: u32, damage: &Damage) -> Damage {
    let candidates = Mask::of(
        &Damage {
            moves: Vec::new(),
            dirty: damage.dirty.clone(),
        },
        width,
        height,
    );
    let mut mask = Mask::new(width, height);
    for (index, candidate) in candidates.tiles.iter().enumerate() {
        let (row, column) = (index as u32 / mask.columns, index as u32 % mask.columns);
        if *candidate && tile_differs(before, after, width, height, row, column) {
            mask.tiles[index] = true;
        }
    }
    Damage {
        moves: damage.moves.clone(),
        dirty: mask.rects(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn changed_tiles_join_into_few_rectangles() {
        let mut mask = Mask::new(200, 100);
        mask.mark(rect(5, 5, 50, 50));
        // Columns 0..1, rows 0..1: one 64x64 block.
        assert_eq!(mask.rects(), vec![rect(0, 0, 64, 64)]);
        mask.mark(rect(190, 90, 5, 5));
        let rects = mask.rects();
        assert_eq!(rects.len(), 2);
        // The last tile is cut to the picture's edge.
        assert!(rects.contains(&rect(160, 64, 40, 32)));
        assert!(Mask::new(10, 10).rects().is_empty());
    }

    #[test]
    fn a_differing_pixel_marks_only_its_tile() {
        let (width, height) = (100, 70);
        let before = vec![0u8; (width * height * 4) as usize];
        let mut after = before.clone();
        let at = ((40 * width + 70) * 4) as usize;
        after[at + 2] = 9;
        let mask = diff(&before, &after, width, height);
        assert_eq!(mask.rects(), vec![rect(64, 32, 32, 32)]);
        assert!(diff(&before, &before, width, height).is_empty());
        assert!((mask.share() - 1024.0 / 7000.0).abs() < 1e-9);
    }

    #[test]
    fn repainted_but_unchanged_regions_are_dropped() {
        let (width, height) = (128, 64);
        let before = vec![7u8; (width * height * 4) as usize];
        let mut after = before.clone();
        after[((10 * width + 100) * 4) as usize] = 8;
        let reported = Damage {
            moves: Vec::new(),
            dirty: vec![rect(0, 0, 128, 64)],
        };
        let refined = refine(&before, &after, width, height, &reported);
        assert_eq!(refined.dirty, vec![rect(96, 0, 32, 32)]);
        // A change outside what was reported is not looked for.
        let elsewhere = Damage {
            moves: Vec::new(),
            dirty: vec![rect(0, 0, 32, 32)],
        };
        assert!(refine(&before, &after, width, height, &elsewhere).is_empty());
    }

    #[test]
    fn merged_damage_redraws_what_moved() {
        let mut damage = Damage {
            moves: vec![Move {
                from: (0, 10),
                to: rect(0, 0, 50, 40),
            }],
            dirty: vec![rect(0, 40, 50, 10)],
        };
        damage.merge(Damage {
            moves: Vec::new(),
            dirty: vec![rect(60, 0, 5, 5)],
        });
        assert!(damage.moves.is_empty());
        assert_eq!(damage.dirty.len(), 3);
        assert_eq!(
            rect(90, 90, 20, 20).clip(100, 100),
            Some(rect(90, 90, 10, 10))
        );
        assert_eq!(rect(100, 0, 5, 5).clip(100, 100), None);
    }
}
