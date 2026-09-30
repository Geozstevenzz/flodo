//! The reward for checking something off.
//!
//! A to-do list is only as good as the habit of opening it, and the habit is
//! built on one moment: ticking a thing off. So that moment gets a small burst
//! of the accent colour out of the checkbox, a bigger one when several go in a
//! row, and a proper one when the whole list is done.
//!
//! Everything is painted on a foreground layer, over the list and outside its
//! layout, so a burst can never nudge a row. And nothing here is random: each
//! spark's direction and speed come from a hash of its index, so a burst looks
//! the same every time and the screenshots stay reproducible.

use crate::markdown::FAMILY_UI;
use crate::theme::Palette;
use eframe::egui::{self, Color32, FontFamily, FontId, Pos2, Stroke, Vec2};
use std::time::{Duration, Instant};

/// Check-offs closer together than this count as one run.
pub const COMBO_WINDOW: Duration = Duration::from_secs(4);

/// The run length after a check-off at `now`, given the previous one.
pub fn combo_after(last: Option<Instant>, combo: u32, now: Instant) -> u32 {
    match last {
        Some(t) if now.saturating_duration_since(t) <= COMBO_WINDOW => combo.saturating_add(1),
        _ => 1,
    }
}

struct Burst {
    /// The widget it comes out of, looked up when it is painted, so a
    /// check-off from the keyboard lands on the same checkbox a click would.
    from: egui::Id,
    /// Where that widget was when first found. Kept, because the row may be
    /// gone a frame later — checked off under "hide completed" — and the
    /// burst is still owed.
    at: Option<Pos2>,
    born: Instant,
    sparks: u32,
    /// How far the sparks fly, in points.
    reach: f32,
    life: f32,
    /// A ring that expands with the sparks. Only the finale has one.
    ring: bool,
    /// Floated up out of the burst: the length of the current run.
    label: Option<String>,
    seed: u32,
}

#[derive(Default)]
pub struct Fireworks {
    bursts: Vec<Burst>,
    combo: u32,
    last: Option<Instant>,
    seed: u32,
}

impl Fireworks {
    /// A to-do was checked off. `checkbox` is the widget id of its checkbox.
    /// Returns how many have now gone in a row.
    pub fn check_off(&mut self, checkbox: egui::Id, now: Instant) -> u32 {
        self.combo = combo_after(self.last, self.combo, now);
        self.last = Some(now);
        let heat = self.combo.min(6) as f32;
        self.seed = self.seed.wrapping_add(1);
        self.bursts.push(Burst {
            from: checkbox,
            at: None,
            born: now,
            sparks: 9 + 3 * heat as u32,
            reach: 20.0 + 4.0 * heat,
            life: 0.7 + 0.04 * heat,
            ring: false,
            label: (self.combo >= 2).then(|| format!("×{}", self.combo)),
            seed: self.seed,
        });
        self.combo
    }

    /// The last open to-do was just checked off: a bigger burst from its
    /// checkbox, and a ring out of the progress mark in the title bar.
    pub fn finale(&mut self, checkbox: egui::Id, ring: egui::Id, now: Instant) {
        self.seed = self.seed.wrapping_add(1);
        self.bursts.push(Burst {
            from: checkbox,
            at: None,
            born: now,
            sparks: 30,
            reach: 54.0,
            life: 1.1,
            ring: true,
            label: None,
            seed: self.seed,
        });
        self.seed = self.seed.wrapping_add(1);
        self.bursts.push(Burst {
            from: ring,
            at: None,
            born: now,
            sparks: 14,
            reach: 26.0,
            life: 0.9,
            ring: true,
            label: None,
            seed: self.seed,
        });
    }

    /// Makes a burst that started `age` ago, for the screenshot harness,
    /// which only ever captures the second frame.
    pub fn backdate(&mut self, age: Duration) {
        for b in &mut self.bursts {
            b.born = b.born.checked_sub(age).unwrap_or(b.born);
        }
        self.last = self.last.and_then(|t| t.checked_sub(age));
    }

    /// Paints every live burst over the whole window, and forgets the ones
    /// that have burnt out.
    pub fn paint(&mut self, ctx: &egui::Context, p: &Palette, font_size: f32) {
        let now = Instant::now();
        self.bursts
            .retain(|b| now.saturating_duration_since(b.born).as_secs_f32() < b.life);
        if self.bursts.is_empty() {
            return;
        }
        let painter = ctx.layer_painter(egui::LayerId::new(
            egui::Order::Foreground,
            egui::Id::new("fireworks"),
        ));
        let bright = p.accent.lerp_to_gamma(Color32::WHITE, 0.45);
        let deep = p.accent.lerp_to_gamma(p.text, 0.25);

        for b in &mut self.bursts {
            if b.at.is_none() {
                b.at = ctx.read_response(b.from).map(|r| r.rect.center());
            }
            // Never drawn: there is nowhere to burst from, so it is not seen.
            let Some(origin) = b.at else {
                continue;
            };
            let t = (now.saturating_duration_since(b.born).as_secs_f32() / b.life).clamp(0.0, 1.0);
            paint_burst(
                &painter,
                b,
                origin,
                t,
                [p.accent, bright, deep],
                p,
                font_size,
            );
        }
        ctx.request_repaint();
    }
}

fn paint_burst(
    painter: &egui::Painter,
    b: &Burst,
    origin: Pos2,
    t: f32,
    colors: [Color32; 3],
    p: &Palette,
    font_size: f32,
) {
    let out = ease_out(t);
    // Holds near full strength, then goes quickly: a spark that dims from
    // the moment it is born reads as a smudge.
    let fade = 1.0 - t * t;

    if b.ring {
        painter.circle_stroke(
            origin,
            6.0 + b.reach * 0.8 * out,
            Stroke::new(2.0 * (1.0 - t) + 0.3, p.accent.gamma_multiply(0.7 * fade)),
        );
    }

    for i in 0..b.sparks {
        let h = |k: u32| hash(b.seed, i * 4 + k);
        // Evenly spaced around the circle, then nudged, so a burst never
        // clumps on one side.
        let angle = std::f32::consts::TAU * (i as f32 + 0.6 * h(0)) / b.sparks as f32;
        let speed = 0.55 + 0.45 * h(1);
        let dir = Vec2::new(angle.cos(), angle.sin());
        // A little gravity, so sparks arc rather than simply expand.
        let fall = Vec2::new(0.0, 16.0 * t * t);
        let pos = origin + dir * (b.reach * speed * out) + fall;
        let color = colors[(h(2) * 3.0) as usize % 3];
        let r = (2.8 * (1.0 - t) + 0.6) * (0.7 + 0.5 * h(3));
        if i % 3 == 0 {
            // Every third spark is a streak along its path, which reads as
            // speed at the start and settles into a dot as it slows.
            let tail = dir * (6.0 * (1.0 - out));
            painter.line_segment(
                [pos - tail, pos],
                Stroke::new(r.max(0.8), color.gamma_multiply(fade)),
            );
        } else {
            painter.circle_filled(pos, r, color.gamma_multiply(fade));
        }
    }

    if let Some(label) = &b.label {
        // Straight up out of the checkbox, over the gutter rather than the
        // title beside it.
        let rise = 26.0 * out;
        painter.text(
            origin + Vec2::new(-10.0, -6.0 - rise),
            egui::Align2::RIGHT_CENTER,
            label,
            FontId::new(font_size * 0.9, FontFamily::Name(FAMILY_UI.into())),
            p.accent.gamma_multiply(fade),
        );
    }
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t).powi(3)
}

/// A cheap, stable 0..1 from two integers.
fn hash(seed: u32, i: u32) -> f32 {
    let mut x = seed.wrapping_mul(0x9E37_79B9) ^ i.wrapping_mul(0x85EB_CA6B);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846C_A68B);
    x ^= x >> 16;
    (x >> 8) as f32 / (1u32 << 24) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_offs_close_together_build_a_run() {
        let t0 = Instant::now();
        assert_eq!(combo_after(None, 0, t0), 1);
        assert_eq!(combo_after(Some(t0), 1, t0 + Duration::from_secs(1)), 2);
        assert_eq!(combo_after(Some(t0), 5, t0 + COMBO_WINDOW), 6);
    }

    #[test]
    fn a_pause_starts_the_run_over() {
        let t0 = Instant::now();
        let later = t0 + COMBO_WINDOW + Duration::from_millis(1);
        assert_eq!(combo_after(Some(t0), 7, later), 1);
    }

    #[test]
    fn a_run_is_counted_and_labelled_from_the_second() {
        let mut f = Fireworks::default();
        let id = egui::Id::new("check");
        let t0 = Instant::now();
        assert_eq!(f.check_off(id, t0), 1);
        assert!(f.bursts[0].label.is_none(), "one on its own is not a run");
        assert_eq!(f.check_off(id, t0 + Duration::from_millis(500)), 2);
        assert_eq!(f.bursts[1].label.as_deref(), Some("×2"));
        assert!(
            f.bursts[1].sparks > f.bursts[0].sparks,
            "a run burns hotter"
        );
    }

    #[test]
    fn the_hash_is_stable_and_in_range() {
        for i in 0..1000 {
            let v = hash(3, i);
            assert!((0.0..1.0).contains(&v));
            assert_eq!(v, hash(3, i));
        }
        assert_ne!(hash(1, 1), hash(2, 1));
    }
}
