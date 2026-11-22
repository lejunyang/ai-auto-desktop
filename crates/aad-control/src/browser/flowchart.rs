//! Conservative, bitmap-only recognition on a flat canvas background.
//! Enclosed four-connected background regions supply both the node geometry and
//! the OCR mask. Of the remaining eight-connected ink, only a component explained
//! by two unique contacts, one simple route and exactly one tapered head is directed.
use crate::{error, Result};
use image::RgbaImage;
use serde_json::{json, Value};

const MAX_SIDE: usize = 2048;
const MAX_PIXELS: usize = 2_000_000;
const MAX_REGIONS: usize = 32_768;
const MAX_OBJECTS: usize = 128;
const MAX_EDGES: usize = 256;
const MAX_ROUTE_PIXELS: usize = 16_384;
const MAX_WORDS: usize = 4096;
const CONTACT_GAP: i32 = 6;
const MIXED: u8 = u8::MAX;

fn limit() -> aad_core::AutomationError {
    error(
        "CONTROL.FLOWCHART_LIMIT",
        "bitmap flowchart exceeds recognition limits",
    )
}

#[derive(Clone, Copy)]
struct Span {
    left: i32,
    right: i32,
}

impl Span {
    const EMPTY: Self = Self {
        left: i32::MAX,
        right: i32::MIN,
    };

    fn include(&mut self, other: Self) {
        self.left = self.left.min(other.left);
        self.right = self.right.max(other.right);
    }

    fn width(self) -> i32 {
        self.right - self.left + 1
    }
}

struct Profile {
    top: i32,
    rows: Vec<Span>,
}

impl Profile {
    fn encloses(&self, other: &Profile) -> bool {
        let offset = other.top - self.top;
        if offset <= 0 || offset as usize + other.rows.len() >= self.rows.len() {
            return false;
        }
        self.rows[offset as usize..]
            .iter()
            .zip(&other.rows)
            .all(|(outer, inner)| outer.left < inner.left && outer.right > inner.right)
    }

    fn span(&self, row: i32, grow: i32) -> Option<Span> {
        let first = (row - self.top - grow).max(0) as usize;
        let end = (row - self.top + grow + 1).clamp(0, self.rows.len() as i32) as usize;
        let mut result = *self.rows.get(first).filter(|_| first < end)?;
        for &span in &self.rows[first + 1..end] {
            result.include(span);
        }
        Some(Span {
            left: result.left - grow,
            right: result.right + grow,
        })
    }

    fn horizontal(&self) -> Span {
        let mut total = Span::EMPTY;
        for &span in &self.rows {
            total.include(span);
        }
        total
    }

    fn shell(&self, grow: i32, width: i32, height: i32, mut visit: impl FnMut(i32, Span)) {
        let end = self.top + self.rows.len() as i32;
        for row in (self.top - grow).max(0)..(end + grow).min(height) {
            let Some(outer) = self.span(row, grow) else {
                continue;
            };
            let mut range = |left: i32, right: i32| {
                let span = Span {
                    left: left.max(0),
                    right: right.min(width - 1),
                };
                if span.left <= span.right {
                    visit(row, span);
                }
            };
            if let Some(inner) = self.span(row, grow - 1) {
                range(outer.left, inner.left - 1);
                range(inner.right + 1, outer.right);
            } else {
                range(outer.left, outer.right);
            }
        }
    }
}

struct Object {
    kind: &'static str,
    inside: Profile,
    outline: Profile,
    label: String,
}

struct Bitmap {
    width: i32,
    height: i32,
    stride: usize,
    ink: Vec<u8>,
}

impl Bitmap {
    fn new(image: &RgbaImage) -> Result<Self> {
        let (width, height) = (image.width() as usize, image.height() as usize);
        if width == 0
            || height == 0
            || width > MAX_SIDE
            || height > MAX_SIDE
            || width * height > MAX_PIXELS
        {
            return Err(limit());
        }
        let rgb = |pixel: &image::Rgba<u8>| -> [u8; 3] {
            let alpha = u16::from(pixel[3]);
            std::array::from_fn(|channel| {
                (255 - (255 - u16::from(pixel[channel])) * alpha / 255) as u8
            })
        };
        let mut samples = [Vec::new(), Vec::new(), Vec::new()];
        for step in 0..32 {
            let col = ((width - 1) * step / 31) as u32;
            let row = ((height - 1) * step / 31) as u32;
            for (col, row) in [
                (col, 0),
                (col, height as u32 - 1),
                (0, row),
                (width as u32 - 1, row),
            ] {
                for (values, channel) in samples.iter_mut().zip(rgb(image.get_pixel(col, row))) {
                    values.push(channel);
                }
            }
        }
        let background = samples.map(|mut values| {
            let mid = values.len() / 2;
            *values.select_nth_unstable(mid).1
        });
        let stride = width + 2;
        let mut ink = vec![2; stride * (height + 2)];
        for (row, pixels) in image.rows().enumerate() {
            let start = (row + 1) * stride + 1;
            for (col, pixel) in pixels.enumerate() {
                ink[start + col] = u8::from(
                    rgb(pixel)
                        .into_iter()
                        .zip(background)
                        .any(|(a, b)| a.abs_diff(b) >= 64),
                );
            }
        }
        Ok(Self {
            width: width as i32,
            height: height as i32,
            stride,
            ink,
        })
    }

    fn index(&self, col: i32, row: i32) -> usize {
        (row + 1) as usize * self.stride + (col + 1) as usize
    }

    fn coords(&self, index: usize) -> (i32, i32) {
        (
            (index % self.stride) as i32 - 1,
            (index / self.stride) as i32 - 1,
        )
    }

    fn eight(&self, index: usize) -> [usize; 8] {
        let stride = self.stride;
        [
            index - 1,
            index + 1,
            index - stride,
            index + stride,
            index - stride - 1,
            index + stride + 1,
            index - stride + 1,
            index + stride - 1,
        ]
    }

    fn objects(&mut self) -> Result<Vec<Object>> {
        let mut objects: Vec<Object> = Vec::new();
        let mut stack = Vec::new();
        let mut rows = vec![Span::EMPTY; self.height as usize];
        let mut regions = 0;
        for seed in 0..self.ink.len() {
            if self.ink[seed] != 0 {
                continue;
            }
            regions += 1;
            if regions > MAX_REGIONS {
                return Err(limit());
            }
            let (_, seed_row) = self.coords(seed);
            let (mut top, mut bottom) = (seed_row, seed_row);
            let mut touches_frame = false;
            let mut count = 0;
            self.ink[seed] = 2;
            stack.push(seed);
            while let Some(index) = stack.pop() {
                let (col, row) = self.coords(index);
                top = top.min(row);
                bottom = bottom.max(row);
                rows[row as usize].include(Span {
                    left: col,
                    right: col,
                });
                count += 1;
                touches_frame |=
                    col == 0 || row == 0 || col == self.width - 1 || row == self.height - 1;
                for next in [
                    index - 1,
                    index + 1,
                    index - self.stride,
                    index + self.stride,
                ] {
                    if self.ink[next] == 0 {
                        self.ink[next] = 2;
                        stack.push(next);
                    }
                }
            }
            let spans = &mut rows[top as usize..=bottom as usize];
            if !touches_frame && spans.len() >= 12 && count >= 100 {
                if let Some(kind) = classify(spans, count) {
                    let inside = Profile {
                        top,
                        rows: spans.to_vec(),
                    };
                    if let Some(outline) = self
                        .outline(&inside, kind)
                        .filter(|_| !objects.iter().any(|object| object.inside.encloses(&inside)))
                    {
                        if objects.len() == MAX_OBJECTS {
                            return Err(limit());
                        }
                        objects.push(Object {
                            kind,
                            inside,
                            outline,
                            label: String::new(),
                        });
                    }
                }
            }
            spans.fill(Span::EMPTY);
        }
        Ok(objects)
    }

    fn outline(&self, inside: &Profile, kind: &str) -> Option<Profile> {
        let mut grow = 0;
        for radius in 1..=7 {
            let (mut ink, mut total) = (0, 0);
            inside.shell(radius, self.width, self.height, |row, span| {
                let start = self.index(span.left, row);
                let width = span.width() as usize;
                ink += self.ink[start..start + width]
                    .iter()
                    .filter(|&&pixel| pixel == 1)
                    .count();
                total += width;
            });
            let coverage = if kind == "diamond" { 20 } else { 5 };
            if total == 0 || ink * coverage < total {
                break;
            }
            grow = radius;
        }
        if !(1..=6).contains(&grow) {
            return None;
        }
        let top = (inside.top - grow).max(0);
        let end = (inside.top + inside.rows.len() as i32 + grow).min(self.height);
        let rows = (top..end)
            .filter_map(|row| inside.span(row, grow))
            .map(|span| Span {
                left: span.left.max(0),
                right: span.right.min(self.width - 1),
            })
            .collect();
        Some(Profile { top, rows })
    }
}

fn classify(rows: &[Span], area: i32) -> Option<&'static str> {
    let mut bounds = Span::EMPTY;
    let mut envelope = 0;
    for &row in rows {
        bounds.include(row);
        envelope += row.width();
    }
    let width = bounds.width() as f64;
    let height = rows.len() as f64;
    if width < 14.0 || area * 2 < envelope {
        return None;
    }
    let fits = |inset: &dyn Fn(f64) -> f64, tolerance: f64| {
        rows.iter().enumerate().all(|(index, row)| {
            let gap = inset(index as f64);
            (f64::from(row.left - bounds.left) - gap).abs() <= tolerance
                && (f64::from(bounds.right - row.right) - gap).abs() <= tolerance
        })
    };
    if fits(&|_| 0.0, 1.0) {
        return Some("rectangle");
    }
    let diamond = |row: f64| (width - 1.0) * (row / (height - 1.0) - 0.5).abs();
    if fits(&diamond, 1.5 + width / height) {
        return Some("diamond");
    }
    let missing = width * height - envelope as f64;
    let radius = (missing / (4.0 - std::f64::consts::PI)).sqrt();
    if radius < 2.0
        || radius > width.min(height) * 0.5 + 1.0
        || 2.0 * radius > width.max(height) * 0.88
    {
        return None;
    }
    let rounded = |row: f64| {
        let depth = row.min(height - 1.0 - row) + 0.5;
        let delta = (radius - depth).max(0.0);
        radius - (radius * radius - delta * delta).max(0.0).sqrt()
    };
    fits(&rounded, 1.5 + radius.sqrt() * 0.35).then_some("rounded_rectangle")
}

fn assign_words(objects: &mut [Object], lines: &Value, width: i32, height: i32) -> Result<()> {
    let Some(lines) = lines.as_array().or_else(|| lines["lines"].as_array()) else {
        return Ok(());
    };
    if lines.len() > MAX_WORDS {
        return Err(limit());
    }
    let mut count = 0;
    let mut bytes = 0;
    for line in lines {
        let Some(words) = line["words"].as_array() else {
            continue;
        };
        count += words.len();
        if count > MAX_WORDS {
            return Err(limit());
        }
        for word in words {
            let Some(text) = word["text"].as_str() else {
                continue;
            };
            if text.len() > 4096 {
                return Err(limit());
            }
            bytes += text.len();
            if bytes > 65_536 {
                return Err(limit());
            }
            let text = text.trim();
            let extent = |start: &str, size: &str, side: i32| -> Option<(i32, i32)> {
                let bounds = &word["box"];
                let start = bounds[start].as_f64()?;
                let size = bounds[size].as_f64()?;
                if !(0.0..1.0).contains(&start)
                    || !(0.0..=1.0).contains(&size)
                    || size == 0.0
                    || start + size > 1.00011
                {
                    return None;
                }
                let side = f64::from(side);
                let first = ((start + 0.000051) * side).floor() as i32;
                let last = (((start + size - 0.000101) * side).ceil() as i32 - 1).max(first);
                Some((first, last))
            };
            let (Some((left, right)), Some((top, bottom))) =
                (extent("fx", "fw", width), extent("fy", "fh", height))
            else {
                continue;
            };
            let mut hits = objects.iter_mut().filter(|object| {
                [top, bottom].iter().all(|row| {
                    object
                        .inside
                        .span(*row, 0)
                        .is_some_and(|span| span.left <= left && right <= span.right)
                })
            });
            if let Some(object) = hits.next() {
                if !text.is_empty() && hits.next().is_none() {
                    if !object.label.is_empty() {
                        object.label.push(' ');
                    }
                    object.label.push_str(text);
                }
            }
        }
    }
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct Mark {
    owner: u8,
    gap: u8,
}

impl Bitmap {
    fn isolate(&mut self, objects: &[Object]) -> Vec<Mark> {
        let mut marks = vec![Mark::default(); self.ink.len()];
        for (number, object) in objects.iter().enumerate() {
            let owner = number as u8 + 1;
            let outline = &object.outline;
            for (offset, span) in outline.rows.iter().enumerate() {
                let first = self.index(span.left, outline.top + offset as i32);
                self.ink[first..first + span.width() as usize].fill(2);
            }
            for gap in 1..=CONTACT_GAP {
                outline.shell(gap, self.width, self.height, |row, span| {
                    for col in span.left..=span.right {
                        let index = self.index(col, row);
                        if self.ink[index] == 1 {
                            let prior = &mut marks[index];
                            if prior.owner == 0 {
                                *prior = Mark {
                                    owner,
                                    gap: gap as u8,
                                };
                            } else if prior.owner != owner {
                                prior.owner = MIXED;
                            }
                        }
                    }
                });
            }
        }
        marks
    }

    fn anchor(&self, component: &[usize], marks: &[Mark], owner: u8) -> Option<usize> {
        let gap = component
            .iter()
            .filter(|&&index| marks[index].owner == owner)
            .map(|&index| marks[index].gap)
            .min()?;
        let nearest = || {
            component
                .iter()
                .copied()
                .filter(|&index| marks[index].owner == owner && marks[index].gap == gap)
        };
        let (mut cols, mut rows) = (Span::EMPTY, Span::EMPTY);
        for index in nearest() {
            let (col, row) = self.coords(index);
            cols.include(Span {
                left: col,
                right: col,
            });
            rows.include(Span {
                left: row,
                right: row,
            });
        }
        if cols.width().max(rows.width()) > 9 {
            return None;
        }
        nearest().min_by_key(|&index| {
            let (col, row) = self.coords(index);
            (2 * col - cols.left - cols.right).pow(2) + (2 * row - rows.left - rows.right).pow(2)
        })
    }

    fn point(&self, index: usize) -> Point {
        let (col, row) = self.coords(index);
        Point(f64::from(col), f64::from(row))
    }

    fn path(&self, start: usize, end: usize, parents: &mut [u8]) -> Vec<Point> {
        let mut queue = Vec::from([start]);
        parents[start] = 9;
        let mut cursor = 0;
        while parents[end] == 0 && cursor < queue.len() {
            for (dir, next) in self.eight(queue[cursor]).into_iter().enumerate() {
                if self.ink[next] == 3 && parents[next] == 0 {
                    parents[next] = (dir ^ 1) as u8 + 1;
                    queue.push(next);
                }
            }
            cursor += 1;
        }
        let mut trace = Vec::new();
        if parents[end] != 0 {
            let mut index = end;
            while index != start {
                trace.push(self.point(index));
                index = self.eight(index)[usize::from(parents[index] - 1)];
            }
            trace.push(self.point(start));
        }
        trace.reverse();
        trace
    }

    fn direction(
        &self,
        component: &[usize],
        marks: &[Mark],
        parents: &mut [u8],
        owners: [u8; 2],
    ) -> Option<(u8, u8)> {
        let start = self.anchor(component, marks, owners[0])?;
        let end = self.anchor(component, marks, owners[1])?;
        let segments = route(&self.path(start, end, parents))?;
        let pixels: Vec<_> = component.iter().map(|&index| self.point(index)).collect();
        let first = segments[0];
        let last = segments.last()?.reversed();
        let (head, axis, direction) = match (head(&pixels, first), head(&pixels, last)) {
            (Some(head), None) => (head, first, (owners[1], owners[0])),
            (None, Some(head)) => (head, last, (owners[0], owners[1])),
            _ => return None,
        };
        pixels
            .iter()
            .all(|&point| {
                let (along, across) = axis.project(point);
                (along >= -1.5
                    && along <= head.depth + 1.5
                    && across.abs() <= head.spread * (along.max(0.0) / head.depth) + 2.0)
                    || segments
                        .iter()
                        .any(|segment| segment.distance(point) <= head.shaft_tolerance)
            })
            .then_some(direction)
    }

    fn edges(&mut self, objects: &[Object], ids: &[String]) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        if objects.is_empty() {
            return Ok(result);
        }
        let marks = self.isolate(objects);
        let mut parents = vec![0; self.ink.len()];
        let mut component = Vec::new();
        let mut regions = 0;
        for seed in 0..self.ink.len() {
            if self.ink[seed] != 1 {
                continue;
            }
            regions += 1;
            if regions > MAX_REGIONS {
                return Err(limit());
            }
            component.clear();
            component.push(seed);
            self.ink[seed] = 3;
            let mut owners = [0; 2];
            let mut ambiguous = false;
            let mut cursor = 0;
            while cursor < component.len() {
                let index = component[cursor];
                let owner = marks[index].owner;
                if owner == MIXED {
                    ambiguous = true;
                } else if owner != 0 && !owners.contains(&owner) {
                    if let Some(slot) = owners.iter_mut().find(|slot| **slot == 0) {
                        *slot = owner;
                    } else {
                        ambiguous = true;
                    }
                }
                for next in self.eight(index) {
                    if self.ink[next] == 1 {
                        self.ink[next] = 3;
                        component.push(next);
                    }
                }
                cursor += 1;
            }
            if component.len() < 8
                || (!ambiguous
                    && (owners[0] == 0
                        || (owners[1] == 0
                            && component.iter().all(|&index| marks[index].owner != 0))))
            {
                continue;
            }
            if result.len() == MAX_EDGES {
                return Err(limit());
            }
            let directed = (!ambiguous && owners[1] != 0 && component.len() <= MAX_ROUTE_PIXELS)
                .then(|| self.direction(&component, &marks, &mut parents, owners))
                .flatten();
            result.push(match directed {
                Some((from, to)) => json!({"from":ids[usize::from(from)-1], "to":ids[usize::from(to)-1], "status":"confirmed"}),
                None => json!({"from":null, "to":null, "status":"uncertain"}),
            });
        }
        Ok(result)
    }
}

#[derive(Clone, Copy)]
struct Point(f64, f64);

#[derive(Clone, Copy)]
struct Segment {
    start: Point,
    end: Point,
    unit: Point,
    length: f64,
}

impl Segment {
    fn new(start: Point, end: Point) -> Self {
        let delta = Point(end.0 - start.0, end.1 - start.1);
        let length = delta.0.hypot(delta.1);
        Self {
            start,
            end,
            unit: Point(delta.0 / length, delta.1 / length),
            length,
        }
    }

    fn reversed(self) -> Self {
        Self::new(self.end, self.start)
    }

    fn project(self, point: Point) -> (f64, f64) {
        let (col, row) = (point.0 - self.start.0, point.1 - self.start.1);
        (
            col * self.unit.0 + row * self.unit.1,
            row * self.unit.0 - col * self.unit.1,
        )
    }

    fn distance(self, point: Point) -> f64 {
        let (along, across) = self.project(point);
        (along - along.clamp(0.0, self.length)).hypot(across)
    }
}

fn route(trace: &[Point]) -> Option<Vec<Segment>> {
    if trace.len() < 9 {
        return None;
    }
    let mut knots = vec![0, trace.len() - 1];
    let mut cursor = 1;
    let mut segments = Vec::new();
    while cursor < knots.len() {
        let (first, last) = (knots[cursor - 1], knots[cursor]);
        let segment = Segment::new(trace[first], trace[last]);
        let furthest = ((first + 1)..last)
            .map(|index| (index, segment.distance(trace[index])))
            .max_by(|left, right| left.1.total_cmp(&right.1));
        if let Some((split, _)) = furthest.filter(|(_, distance)| *distance > 3.0) {
            if knots.len() == 7 {
                return None;
            }
            knots.insert(cursor, split);
        } else {
            segments.push(segment);
            cursor += 1;
        }
    }
    if segments.len() > 1
        && (segments.iter().any(|segment| {
            segment.length < 4.0 || segment.unit.0.abs().min(segment.unit.1.abs()) > 0.28
        }) || segments.windows(2).any(|pair| {
            (pair[0].unit.0 * pair[1].unit.0 + pair[0].unit.1 * pair[1].unit.1).abs() > 0.38
        }))
    {
        return None;
    }
    if segments.len() > 1 {
        let axes: Vec<_> = segments
            .iter()
            .enumerate()
            .map(|(index, segment)| {
                let horizontal = segment.unit.0.abs() > segment.unit.1.abs();
                let coordinate = |point: Point| if horizontal { point.1 } else { point.0 };
                let fixed = if index == 0 {
                    coordinate(segment.start)
                } else if index + 1 == segments.len() {
                    coordinate(segment.end)
                } else {
                    (coordinate(segment.start) + coordinate(segment.end)) * 0.5
                };
                (horizontal, fixed)
            })
            .collect();
        let mut start = segments[0].start;
        for index in 0..segments.len() {
            let (horizontal, fixed) = axes[index];
            let end = match axes.get(index + 1) {
                Some(&(_, next)) if horizontal => Point(next, fixed),
                Some(&(_, next)) => Point(fixed, next),
                None => segments[index].end,
            };
            segments[index] = Segment::new(start, end);
            start = end;
        }
    }
    Some(segments)
}

struct Head {
    depth: f64,
    spread: f64,
    shaft_tolerance: f64,
}

fn head(pixels: &[Point], axis: Segment) -> Option<Head> {
    let mut bands = [(f64::INFINITY, f64::NEG_INFINITY); 27];
    for &pixel in pixels {
        let (along, across) = axis.project(pixel);
        for center in [along.floor(), along.ceil()] {
            if (0.0..27.0).contains(&center) && (along - center).abs() <= 0.75 {
                let band = &mut bands[center as usize];
                band.0 = band.0.min(across);
                band.1 = band.1.max(across);
            }
        }
    }
    for depth in (4..=21.min(axis.length as usize)).rev() {
        if depth as f64 + 4.0 > axis.length {
            continue;
        }
        let (lo, hi) = bands[depth];
        let spread = hi.max(-lo);
        if hi.min(-lo) < 2.25 || hi.min(-lo) < 0.48 * spread || spread > depth as f64 * 1.45 {
            continue;
        }
        let shaft = &bands[depth + 2..=depth + 4];
        if shaft.iter().any(|&(left, right)| {
            left > right
                || left.abs().max(right.abs()) > 3.0
                || right - left > 4.0
                || hi - lo < right - left + 4.0
        }) {
            continue;
        }
        if bands[0].1 - bands[0].0 > hi - lo - 2.0 {
            continue;
        }
        if (1..depth).any(|step| {
            let fraction = step as f64 / depth as f64;
            let (left, right) = bands[step];
            (left - lo * fraction).abs() > 2.0 || (right - hi * fraction).abs() > 2.0
        }) {
            continue;
        }
        let thickness = shaft
            .iter()
            .map(|&(left, right)| right - left)
            .fold(0.0, f64::max);
        return Some(Head {
            depth: depth as f64,
            spread,
            shaft_tolerance: (thickness + 1.25).clamp(1.75, 4.25),
        });
    }
    None
}

fn label_image(image: &RgbaImage, objects: &[Object]) -> RgbaImage {
    let mut clean = RgbaImage::from_pixel(image.width(), image.height(), image::Rgba([255; 4]));
    let width = image.width() as usize;
    for object in objects {
        for (offset, rows) in object.inside.rows.windows(3).enumerate() {
            let left = rows.iter().map(|span| span.left).max().unwrap() + 1;
            let right = rows.iter().map(|span| span.right).min().unwrap() - 1;
            if left <= right {
                let row = object.inside.top as usize + offset + 1;
                let bytes =
                    4 * (row * width + left as usize)..4 * (row * width + right as usize + 1);
                clean.as_mut()[bytes.clone()].copy_from_slice(&image.as_raw()[bytes]);
            }
        }
    }
    clean
}

pub(super) fn detect(
    image: &RgbaImage,
    read_labels: impl FnOnce(&RgbaImage) -> Result<Value>,
) -> Result<Value> {
    let mut bitmap = Bitmap::new(image)?;
    let mut objects = bitmap.objects()?;
    let ids: Vec<_> = (1..=objects.len())
        .map(|number| format!("shape-{number}"))
        .collect();
    let edges = bitmap.edges(&objects, &ids)?;
    let lines = read_labels(&label_image(image, &objects))?;
    assign_words(&mut objects, &lines, bitmap.width, bitmap.height)?;
    let objects: Vec<_> = objects.iter().zip(ids).map(|(object, id)| {
        let bounds = object.outline.horizontal();
        json!({"id":id,"kind":object.kind,"label":object.label,"box":{
            "fx":f64::from(bounds.left)/f64::from(bitmap.width), "fy":f64::from(object.outline.top)/f64::from(bitmap.height),
            "fw":f64::from(bounds.width())/f64::from(bitmap.width), "fh":object.outline.rows.len() as f64/f64::from(bitmap.height)}})
    }).collect();
    Ok(json!({"objects":objects,"edges":edges}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    const INK: Rgba<u8> = Rgba([24, 42, 68, 255]);
    const PAPER: Rgba<u8> = Rgba([255, 255, 255, 255]);
    type XY = (i32, i32);

    fn paper(width: u32, height: u32) -> RgbaImage {
        RgbaImage::from_pixel(width, height, PAPER)
    }

    fn line(image: &mut RgbaImage, start: XY, end: XY, radius: i32) {
        let (mut col, mut row) = start;
        let (dx, dy) = ((end.0 - col).abs(), -(end.1 - row).abs());
        let (sx, sy) = ((end.0 - col).signum(), (end.1 - row).signum());
        let mut residual = dx + dy;
        loop {
            for vertical in row - radius..=row + radius {
                for horizontal in col - radius..=col + radius {
                    image.put_pixel(horizontal as u32, vertical as u32, INK);
                }
            }
            if (col, row) == end {
                break;
            }
            let twice = 2 * residual;
            if twice >= dy {
                residual += dy;
                col += sx;
            }
            if twice <= dx {
                residual += dx;
                row += sy;
            }
        }
    }

    fn polygon(image: &mut RgbaImage, corners: &[XY], thickness: i32) {
        for (first, second) in corners.iter().zip(corners.iter().cycle().skip(1)) {
            line(image, *first, *second, thickness);
        }
    }

    fn rect(image: &mut RgbaImage, from: XY, to: XY, thickness: i32) {
        polygon(
            image,
            &[from, (to.0, from.1), to, (from.0, to.1)],
            thickness,
        );
    }

    fn rounded(image: &mut RgbaImage, from: XY, to: XY, radius: i32) {
        for row in from.1..=to.1 {
            for col in from.0..=to.0 {
                let center_col = col.clamp(from.0 + radius, to.0 - radius);
                let center_row = row.clamp(from.1 + radius, to.1 - radius);
                let squared = (col - center_col).pow(2) + (row - center_row).pow(2);
                if squared <= radius.pow(2)
                    && (squared > (radius - 2).pow(2)
                        || col - from.0 < 2
                        || to.0 - col < 2
                        || row - from.1 < 2
                        || to.1 - row < 2)
                {
                    image.put_pixel(col as u32, row as u32, INK);
                }
            }
        }
    }

    fn word(text: &str, from: XY, to: XY, image: &RgbaImage) -> Value {
        json!({"text":text, "box":{"fx":from.0 as f64/image.width() as f64,"fy":from.1 as f64/image.height() as f64,
            "fw":(to.0-from.0) as f64/image.width() as f64, "fh":(to.1-from.1) as f64/image.height() as f64}})
    }

    fn graph(image: &RgbaImage) -> Value {
        detect(image, |_| Ok(json!([]))).unwrap()
    }

    fn edge(from: usize, to: usize) -> Value {
        json!({"from":format!("shape-{from}"),"to":format!("shape-{to}"),"status":"confirmed"})
    }

    fn uncertain() -> Value {
        json!([{"from":null,"to":null,"status":"uncertain"}])
    }

    #[test]
    fn shape_labels_use_word_extents_and_stable_observation_local_ids() {
        let mut image = paper(300, 120);
        rect(&mut image, (5, 10), (85, 60), 1);
        rounded(&mut image, (95, 10), (180, 70), 12);
        polygon(&mut image, &[(240, 7), (286, 45), (240, 83), (194, 45)], 1);
        rect(&mut image, (15, 24), (20, 32), 0);
        let ocr = json!({"lines":[{"words":[word("One",(12,22),(27,34),&image),word("two",(30,22),(48,34),&image),word("three",(105,30),(150,50),&image),
            word("four",(228,40),(252,49),&image),word("straddles",(190,40),(212,50),&image),word("corner",(201,14),(217,25),&image),word("outside",(15,80),(33,97),&image),
            {"text":"invalid","box":{"fx":0.1,"fy":0.22,"fw":-1,"fh":0.1}}]}]});
        let result = detect(&image, |mask| {
            assert_eq!(mask.dimensions(), image.dimensions());
            for (col, row, pixel) in mask.enumerate_pixels() {
                let expected = if (15..=20).contains(&col) && (24..=32).contains(&row) {
                    image.get_pixel(col, row)
                } else {
                    &PAPER
                };
                assert_eq!(pixel, expected, "unexpected OCR mask at {col},{row}");
            }
            Ok(ocr.clone())
        })
        .unwrap();
        assert_eq!(
            detect(&image, |_| Ok(ocr["lines"].clone())).unwrap(),
            result
        );
        let objects = result["objects"].as_array().unwrap();
        assert_eq!(objects.len(), 3, "{result}");
        for (index, object) in objects.iter().enumerate() {
            assert_eq!(object["id"], format!("shape-{}", index + 1));
        }
        for (kind, label) in [
            ("rectangle", "One two"),
            ("rounded_rectangle", "three"),
            ("diamond", "four"),
        ] {
            assert!(
                objects
                    .iter()
                    .any(|obj| obj["kind"] == kind && obj["label"] == label),
                "{result}"
            );
        }
        let rectangle = objects
            .iter()
            .find(|obj| obj["kind"] == "rectangle")
            .unwrap();
        assert_eq!(
            rectangle["box"],
            json!({"fx":4./300.,"fy":9./120.,"fw":83./300.,"fh":53./120.})
        );
        assert_eq!(result["edges"], json!([]));
    }

    #[test]
    fn filled_straight_arrow_joins_only_the_painted_destination() {
        let mut image = paper(620, 260);
        for (col, row) in [(20, 35), (300, 35), (300, 150)] {
            rect(&mut image, (col, row), (col + 140, row + 65), 1);
        }
        line(&mut image, (160, 68), (298, 68), 1);
        for col in 284..=298 {
            let half = (298 - col) / 2;
            line(&mut image, (col, 68 - half), (col, 68 + half), 0);
        }
        let result = graph(&image);
        assert_eq!(result["edges"], json!([edge(1, 2)]), "{result}");
    }

    #[test]
    fn open_elbow_head_can_point_up_and_reverse_scan_order() {
        let mut image = paper(240, 160);
        rect(&mut image, (14, 90), (75, 140), 1);
        rounded(&mut image, (145, 15), (210, 65), 10);
        line(&mut image, (75, 115), (177, 115), 1);
        line(&mut image, (177, 115), (177, 67), 1);
        for end in [(171, 78), (183, 78)] {
            line(&mut image, (177, 67), end, 0);
        }
        assert_eq!(graph(&image)["edges"], json!([edge(2, 1)]));
    }

    #[test]
    fn diagonal_arrow_crosses_neither_node_outline() {
        let mut image = paper(190, 155);
        rect(&mut image, (10, 10), (60, 40), 0);
        rect(&mut image, (120, 100), (175, 140), 0);
        line(&mut image, (60, 40), (118, 98), 0);
        for end in [(106, 93), (113, 86)] {
            line(&mut image, (118, 98), end, 0);
        }
        assert_eq!(graph(&image)["edges"], json!([edge(1, 2)]));
    }

    #[test]
    fn no_head_two_heads_and_unexplained_branch_do_not_confirm() {
        let mut image = paper(230, 140);
        rect(&mut image, (10, 40), (60, 95), 0);
        rect(&mut image, (160, 40), (220, 95), 0);
        line(&mut image, (60, 68), (158, 68), 0);
        assert_eq!(graph(&image)["edges"], uncertain());
        for end in [(147, 62), (147, 74)] {
            line(&mut image, (158, 68), end, 0);
        }
        assert_eq!(graph(&image)["edges"], json!([edge(1, 2)]));
        let mut fork = image.clone();
        line(&mut fork, (106, 40), (106, 92), 0);
        assert_eq!(graph(&fork)["edges"], uncertain());
        for end in [(73, 62), (73, 74)] {
            line(&mut image, (62, 68), end, 0);
        }
        assert_eq!(graph(&image)["edges"], uncertain());
    }

    #[test]
    fn four_way_crossing_is_not_paired_by_proximity() {
        let mut image = paper(260, 200);
        for (from, to) in [
            ((5, 78), (55, 122)),
            ((205, 78), (255, 122)),
            ((110, 5), (150, 39)),
            ((110, 160), (150, 195)),
        ] {
            rect(&mut image, from, to, 1);
        }
        line(&mut image, (55, 100), (202, 100), 1);
        line(&mut image, (130, 39), (130, 157), 1);
        for end in [(190, 94), (190, 106)] {
            line(&mut image, (202, 100), end, 1);
        }
        for end in [(124, 145), (136, 145)] {
            line(&mut image, (130, 157), end, 1);
        }
        let result = graph(&image);
        assert_eq!(result["objects"].as_array().unwrap().len(), 4);
        assert_eq!(result["edges"], uncertain());
    }

    #[test]
    fn broken_and_clipped_contours_are_not_objects() {
        let mut image = paper(120, 80);
        rect(&mut image, (10, 10), (90, 70), 0);
        image.put_pixel(10, 40, PAPER);
        assert_eq!(graph(&image), json!({"objects":[],"edges":[]}));
        for (first, last) in [
            ((0, 20), (60, 20)),
            ((60, 20), (60, 60)),
            ((60, 60), (0, 60)),
        ] {
            line(&mut image, first, last, 0);
        }
        assert_eq!(graph(&image)["objects"], json!([]));
    }

    #[test]
    fn two_elbows_attach_to_a_diamond_and_direction_is_not_reading_order() {
        let mut image = paper(300, 190);
        polygon(&mut image, &[(45, 15), (86, 43), (45, 71), (4, 43)], 1);
        rect(&mut image, (205, 108), (288, 170), 1);
        for (start, end) in [
            ((205, 139), (155, 139)),
            ((155, 139), (155, 43)),
            ((155, 43), (88, 43)),
        ] {
            line(&mut image, start, end, 1);
        }
        for end in [(101, 36), (101, 50)] {
            line(&mut image, (88, 43), end, 1);
        }
        let result = graph(&image);
        assert_eq!(result["edges"], json!([edge(2, 1)]), "{result}");
    }

    #[test]
    fn large_closed_glyphs_stay_in_the_label_not_the_graph() {
        let mut image = paper(150, 80);
        rect(&mut image, (4, 4), (144, 74), 0);
        rect(&mut image, (34, 20), (56, 50), 1);
        let result = detect(&image, |clean| {
            assert_eq!(clean.get_pixel(34, 30), &INK);
            Ok(json!([{"words":[word("0",(32,18),(58,52),clean)]}]))
        })
        .unwrap();
        assert_eq!(result["objects"].as_array().unwrap().len(), 1);
        assert_eq!(result["objects"][0]["label"], "0");
    }

    #[test]
    fn a_nearby_second_destination_or_single_wing_is_not_a_direction() {
        let mut image = paper(240, 150);
        rect(&mut image, (10, 40), (65, 100), 0);
        rect(&mut image, (140, 20), (220, 70), 0);
        line(&mut image, (65, 69), (138, 69), 0);
        line(&mut image, (138, 69), (126, 62), 0);
        assert_eq!(graph(&image)["edges"], uncertain());
        line(&mut image, (138, 69), (126, 76), 0);
        assert_eq!(graph(&image)["edges"], json!([edge(2, 1)]));
        rect(&mut image, (140, 73), (220, 130), 0);
        assert_eq!(graph(&image)["edges"], uncertain());
    }

    #[test]
    fn out_of_scope_ocr_failure_is_not_returned_as_an_empty_read() {
        let result = detect(&paper(64, 64), |_| {
            Err(error("DRIVER.OCR_FAILED", "fixture"))
        });
        assert_eq!(result.unwrap_err().code, "DRIVER.OCR_FAILED");
    }

    #[test]
    fn rejects_oversized_images_ocr_and_pathological_region_counts() {
        for (width, height) in [(0, 1), (2049, 1), (1500, 1500)] {
            assert!(detect(&paper(width, height), |_| panic!("oversized OCR input")).is_err());
        }
        let image = paper(28, 28);
        assert!(detect(&image, |_| Ok(
            json!([{"words":vec![Value::Null; MAX_WORDS+1]}])
        ))
        .is_err());
        let mut noise = paper(400, 400);
        for (col, row, pixel) in noise.enumerate_pixels_mut() {
            if (col + row) % 2 == 0 {
                *pixel = INK;
            }
        }
        assert!(detect(&noise, |_| panic!("noisy OCR input")).is_err());
        let mut crowded = paper(420, 320);
        for index in 0..=MAX_OBJECTS as i32 {
            let (col, row) = (3 + (index % 15) * 27, 3 + (index / 15) * 33);
            rect(&mut crowded, (col, row), (col + 19, row + 23), 0);
        }
        assert!(detect(&crowded, |_| panic!("crowded OCR input")).is_err());
    }
}
