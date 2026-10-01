//! The eclipse mark: kage's disc with a shade across it, and the working
//! indicator when the shade orbits.
//!
//! Painted as one crescent, the disc minus the shade circle, filled with
//! the palette's orb gradient, so the mark sits on any background. The
//! orbit turns the shade around the disc's center every 2.2 seconds.

use std::f32::consts::{PI, TAU};
use std::time::Duration;

use gpui_kit::{
    Bounds, IntoElement, PathBuilder, Pixels, Point, Styled as _, Window, canvas,
    linear_color_stop, linear_gradient, point, px,
};
use web_time::Instant;

use crate::theme::Palette;

/// The design draws the mark in a 24-unit box.
const BOX: f32 = 24.0;
/// The disc radius.
const DISC_R: f32 = 10.0;
/// The shade radius.
const SHADE_R: f32 = 8.5;
/// Where the shade sits at rest: up and to the right of center.
const SHADE_AT: (f32, f32) = (16.5, 9.0);
/// One orbit of the shade.
const ORBIT: Duration = Duration::from_millis(2200);

/// The shade's center after `turn` of a full orbit, in box units.
fn shade_center(turn: f32) -> (f32, f32) {
    let (dx, dy) = (SHADE_AT.0 - 12.0, SHADE_AT.1 - 12.0);
    let angle = turn * TAU;
    let (sin, cos) = angle.sin_cos();
    (12.0 + dx * cos - dy * sin, 12.0 + dx * sin + dy * cos)
}

/// The two points where the disc and the shade circles cross, in box
/// units, ordered so the disc's visible arc runs from the first to the
/// second the long way round.
fn crossings(shade: (f32, f32)) -> Option<((f32, f32), (f32, f32))> {
    let (cx, cy) = (12.0, 12.0);
    let (dx, dy) = (shade.0 - cx, shade.1 - cy);
    let d = (dx * dx + dy * dy).sqrt();
    if d >= DISC_R + SHADE_R || d <= (DISC_R - SHADE_R).abs() || d == 0.0 {
        return None;
    }
    let a = (DISC_R * DISC_R - SHADE_R * SHADE_R + d * d) / (2.0 * d);
    let h = (DISC_R * DISC_R - a * a).max(0.0).sqrt();
    let (mx, my) = (cx + a * dx / d, cy + a * dy / d);
    let first = (mx + h * dy / d, my - h * dx / d);
    let second = (mx - h * dy / d, my + h * dx / d);
    Some((first, second))
}

/// Paints the crescent into `bounds`.
fn paint(bounds: Bounds<Pixels>, turn: f32, pal: &Palette, window: &mut Window) {
    let scale = f32::from(bounds.size.width.min(bounds.size.height)) / BOX;
    let at = |(x, y): (f32, f32)| -> Point<Pixels> {
        bounds.origin + point(px(x * scale), px(y * scale))
    };
    let shade = shade_center(turn);
    let fill = linear_gradient(
        135.,
        linear_color_stop(pal.orb_1, 0.),
        linear_color_stop(pal.orb_2, 1.),
    );
    if let Some((first, second)) = crossings(shade) {
        let mut path = PathBuilder::fill();
        path.move_to(at(first));
        // The disc's arc outside the shade, then the shade's arc inside
        // the disc back to the start.
        path.arc_to(
            point(px(DISC_R * scale), px(DISC_R * scale)),
            px(0.),
            true,
            false,
            at(second),
        );
        path.arc_to(
            point(px(SHADE_R * scale), px(SHADE_R * scale)),
            px(0.),
            false,
            true,
            at(first),
        );
        if let Ok(path) = path.build() {
            window.paint_path(path, fill);
        }
    }
    let mut ring = PathBuilder::stroke(px(0.8 * scale));
    let steps = 48;
    for step in 0..=steps {
        let angle = step as f32 / steps as f32 * 2.0 * PI;
        let p = at((12.0 + DISC_R * angle.cos(), 12.0 + DISC_R * angle.sin()));
        if step == 0 {
            ring.move_to(p);
        } else {
            ring.line_to(p);
        }
    }
    if let Ok(ring) = ring.build() {
        window.paint_path(ring, pal.line_strong);
    }
}

/// The eclipse mark at `size` pixels. With `orbit` the shade turns on
/// the clock `origin` started, asking for frames while it does.
pub(crate) fn eclipse(size_px: f32, orbit: Option<Instant>, pal: &Palette) -> impl IntoElement {
    let pal = pal.clone();
    canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            let turn = orbit.map_or(0.0, |origin| {
                let t = origin.elapsed().as_secs_f32() % ORBIT.as_secs_f32();
                t / ORBIT.as_secs_f32()
            });
            paint(bounds, turn, &pal, window);
            if orbit.is_some() {
                window.request_animation_frame();
            }
        },
    )
    .flex_none()
    .size(px(size_px))
}

#[cfg(test)]
mod tests {
    use super::{DISC_R, SHADE_AT, SHADE_R, crossings, shade_center};

    #[test]
    fn the_shade_rests_where_the_design_draws_it_and_orbits_the_center() {
        let rest = shade_center(0.0);
        assert!((rest.0 - SHADE_AT.0).abs() < 1e-4 && (rest.1 - SHADE_AT.1).abs() < 1e-4);
        let half = shade_center(0.5);
        assert!((half.0 - (24.0 - SHADE_AT.0)).abs() < 1e-3, "{half:?}");
        assert!((half.1 - (24.0 - SHADE_AT.1)).abs() < 1e-3, "{half:?}");
    }

    #[test]
    fn the_crossings_lie_on_both_circles() {
        let shade = shade_center(0.25);
        let (a, b) = crossings(shade).expect("the circles cross");
        for (x, y) in [a, b] {
            let disc = ((x - 12.0).powi(2) + (y - 12.0).powi(2)).sqrt();
            let shaded = ((x - shade.0).powi(2) + (y - shade.1).powi(2)).sqrt();
            assert!((disc - DISC_R).abs() < 1e-3);
            assert!((shaded - SHADE_R).abs() < 1e-3);
        }
    }
}
