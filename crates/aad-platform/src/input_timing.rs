//! Deterministic movement and timing for a single physical click.

pub const CLICK_PLANNER_VERSION: u32 = 1;
pub const MIN_CLICK_SPEED: f64 = 0.5;
pub const MAX_CLICK_SPEED: f64 = 2.0;

type Point = (i32, i32);

/// Inclusive screen-pixel bounds, in the coordinate space of the backend.
#[derive(Clone, Copy, Debug)]
pub struct ClickBounds {
    pub min: Point,
    pub max: Point,
}

impl ClickBounds {
    pub fn contains(self, (x, y): Point) -> bool {
        (self.min.0..=self.max.0).contains(&x) && (self.min.1..=self.max.1).contains(&y)
    }

    fn constrain(self, x: f64, y: f64) -> Point {
        (
            (x.round() as i32).clamp(self.min.0, self.max.0),
            (y.round() as i32).clamp(self.min.1, self.max.1),
        )
    }

    fn room(self, (x, y): Point, (dx, dy): (f64, f64)) -> f64 {
        fn axis(value: i32, min: i32, max: i32, direction: f64) -> f64 {
            if direction == 0.0 {
                f64::INFINITY
            } else {
                let edge = if direction > 0.0 { max } else { min };
                (f64::from(edge) - f64::from(value)) / direction
            }
        }
        axis(x, self.min.0, self.max.0, dx).min(axis(y, self.min.1, self.max.1, dy))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClickMove {
    pub at_ms: u32,
    pub point: Point,
    /// Past the landing point. The dispatcher must prove the element hit here too.
    pub overshoot: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ClickPlan {
    pub moves: Vec<ClickMove>,
    pub movement_ms: u32,
    pub settle_ms: u32,
    pub hold_ms: u32,
}

impl ClickPlan {
    pub fn duration_ms(&self) -> u32 {
        self.movement_ms + self.settle_ms + self.hold_ms
    }
}

#[derive(Clone, Copy)]
pub struct ClickGeometry {
    pub start: Point,
    pub target: Point,
    pub window: ClickBounds,
    pub hit: ClickBounds,
    pub allow_overshoot: bool,
}

/// Fixed 32-bit LCG. Changes to arithmetic, draw order or rounding require
/// bumping the planner version because a saved seed may be replayed.
struct Rng(u32);

impl Rng {
    fn draw(&mut self, range: u32) -> u32 {
        self.0 = self.0.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.0 >> 8) % range
    }
}

fn smooth(t: f64) -> f64 {
    t * t * t * (10.0 + t * (-15.0 + 6.0 * t))
}

/// The backend supplies its fresh landing rectangle and window bounds. Each
/// generated pixel still needs the backend's window and hit checks at dispatch.
pub fn plan_click(
    geometry: ClickGeometry,
    seed: u32,
    speed: f64,
    hold_ms: Option<u16>,
) -> Option<ClickPlan> {
    let ClickGeometry {
        start,
        target,
        window,
        hit,
        allow_overshoot,
    } = geometry;
    if !(MIN_CLICK_SPEED..=MAX_CLICK_SPEED).contains(&speed)
        || !window.contains(start)
        || !window.contains(target)
        || !hit.contains(target)
        || matches!(hold_ms, Some(0 | 5001..))
    {
        return None;
    }
    let mut rng = Rng(seed);
    let jitter = f64::from(rng.draw(17)) - 8.0;
    let bend = (f64::from(rng.draw(101)) - 50.0) / 50.0;
    let overshoot = rng.draw(5) == 0;
    let overshoot_px = 3.0 + f64::from(rng.draw(5));
    let scaled = |ms: f64| (ms / speed).round() as u32;
    let mut plan = ClickPlan {
        moves: Vec::new(),
        movement_ms: 0,
        settle_ms: scaled(f64::from(24 + rng.draw(17))),
        hold_ms: hold_ms.map_or_else(|| scaled(f64::from(55 + rng.draw(31))), u32::from),
    };
    if start == target {
        return Some(plan);
    }

    let dx = f64::from(target.0) - f64::from(start.0);
    let dy = f64::from(target.1) - f64::from(start.1);
    let distance = dx.hypot(dy);
    let along = (dx / distance, dy / distance);
    let across = (-along.1, along.0);
    let forward_room = hit.room(target, along).min(window.room(target, along));
    let width = (1.0 + hit.room(target, along) + hit.room(target, (-along.0, -along.1))).max(1.0);
    let duration = scaled(
        (115.0 + 65.0 * (1.0 + distance / width).log2() + jitter)
            .round()
            .clamp(160.0, 650.0),
    );
    plan.movement_ms = duration;
    let extra =
        if allow_overshoot && overshoot && distance >= 48.0 && forward_room >= overshoot_px + 1.0 {
            overshoot_px
        } else {
            0.0
        };
    let correction = if extra == 0.0 { 0 } else { scaled(40.0) };
    let approach = duration - correction;
    let bend = bend.signum() * (0.35 + 0.65 * bend.abs()) * (distance * 0.16).min(44.0);
    let mut previous = start;
    let mut sample = |at_ms, progress: f64, offset: f64| {
        let mut point = window.constrain(
            f64::from(start.0) + along.0 * progress + across.0 * offset,
            f64::from(start.1) + along.1 * progress + across.1 * offset,
        );
        if at_ms == duration || (progress > distance && !hit.contains(point)) {
            point = target;
        }
        if point != previous || at_ms == duration {
            plan.moves.push(ClickMove {
                at_ms,
                point,
                overshoot: progress > distance && point != target,
            });
            previous = point;
        }
    };
    for at in (8..approach).step_by(8) {
        let progress = smooth(f64::from(at) / f64::from(approach));
        let offset = bend * 16.0 * (progress * (1.0 - progress)).powi(2);
        let progress = (distance + extra) * progress;
        // Once past the landing point, keep the return segment on the proved
        // approach axis. A rounded pixel outside the hit box stays at target.
        sample(at, progress, if progress > distance { 0.0 } else { offset });
    }
    if correction != 0 {
        sample(approach, distance + extra, 0.0);
        for at in (8..correction).step_by(8) {
            let progress = smooth(f64::from(at) / f64::from(correction));
            sample(approach + at, distance + extra * (1.0 - progress), 0.0);
        }
    }
    sample(duration, distance, 0.0);
    Some(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geometry(distance: i32, width: i32) -> ClickGeometry {
        ClickGeometry {
            start: (100, 100),
            target: (100 + distance, 100),
            window: ClickBounds {
                min: (0, 0),
                max: (900, 300),
            },
            hit: ClickBounds {
                min: (100 + distance - width / 2, 90),
                max: (100 + distance + width / 2 - 1, 110),
            },
            allow_overshoot: true,
        }
    }

    #[test]
    fn seed_geometry_and_speed_determine_the_timetable() {
        let plan = |d, w, speed| plan_click(geometry(d, w), 42, speed, Some(123)).unwrap();
        let near = plan(120, 48, 1.0);
        let far = plan(720, 48, 1.0);
        assert_eq!(near, plan(120, 48, 1.0));
        assert!(near.movement_ms < far.movement_ms);
        assert!(near.movement_ms < plan(120, 12, 1.0).movement_ms);
        assert!(far.movement_ms < plan(720, 12, 1.0).movement_ms);
        let fast = plan(720, 48, 2.0);
        assert_eq!(fast.hold_ms, 123);
        assert!(fast.settle_ms < far.settle_ms);
        assert!(fast.movement_ms < far.movement_ms);
        assert_eq!(far.moves.last().unwrap().point, (820, 100));
        assert!(far.moves.iter().any(|event| event.point.1 != 100));
        let largest_step = far
            .moves
            .windows(2)
            .map(|p| p[1].point.0 - p[0].point.0)
            .max()
            .unwrap();
        let last = &far.moves[far.moves.len() - 2..];
        assert!((last[1].point.0 - last[0].point.0).abs() < largest_step);
    }

    #[test]
    fn overshoot_is_seeded_and_stays_inside_the_hit_rectangle() {
        let roomy = geometry(720, 48);
        let seed = (0..256)
            .find(|seed| {
                plan_click(roomy, *seed, 1.0, None)
                    .unwrap()
                    .moves
                    .iter()
                    .any(|p| p.overshoot)
            })
            .unwrap();
        let wide = plan_click(roomy, seed, 1.0, None).unwrap();
        assert_eq!(wide.moves.last().unwrap().point, roomy.target);
        assert!(wide
            .moves
            .iter()
            .filter(|p| p.overshoot)
            .all(|p| roomy.hit.contains(p.point)));
        let mut edge = roomy;
        edge.hit.max.0 = edge.target.0;
        let plan = plan_click(edge, seed, 1.0, None).unwrap();
        assert!(plan
            .moves
            .iter()
            .all(|p| !p.overshoot && p.point.0 <= edge.target.0));
        edge.start = edge.target;
        assert!(plan_click(edge, seed, 1.0, None).unwrap().moves.is_empty());
        assert!(plan_click(edge, seed, f64::NAN, None).is_none());
    }
}
