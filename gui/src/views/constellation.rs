//! The swarm constellation: one star per worker, lit by its state and
//! joined in launch order back to the lead.
//!
//! A custom painted element. Stars sit in one to four rows by worker
//! count, with a small jitter seeded by each item so a layout never
//! moves between frames. Running stars twinkle on a clock the card
//! keeps, so a repaint from a state change continues the animation
//! instead of restarting it.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::{
    App, Bounds, Hsla, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    PathBuilder, Pixels, Point, Styled as _, Window, canvas, div, point, px, quad, size,
};
use web_time::Instant;

use crate::theme::Palette;
use crate::views::agents::Phase;

/// One worker's star.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Star {
    /// The child session id a click opens.
    pub id: String,
    /// The swarm item, which seeds the star's jitter.
    pub item: String,
    /// Where the worker stands.
    pub phase: Phase,
}

/// The star core radius.
const CORE_R: f32 = 4.2;
/// The twinkle halo radius at rest.
const HALO_R: f32 = 10.0;
/// The lead star radius.
const LEAD_R: f32 = 6.0;
/// How near a click must land to a star to open it.
const HIT_R: f32 = 10.0;

/// The field height for `n` workers: 26px plus 18px per row.
#[must_use]
pub(crate) fn field_height(n: usize) -> f32 {
    26.0 + rows(n) as f32 * 18.0
}

fn rows(n: usize) -> usize {
    match n {
        0..=8 => 1,
        9..=24 => 2,
        25..=64 => 3,
        _ => 4,
    }
}

/// Where each star sits in a field `width` wide, in launch order, and
/// where the lead sits.
fn layout(stars: &[Star], width: f32) -> (Point<f32>, Vec<Point<f32>>) {
    let n = stars.len();
    let rows = rows(n);
    let cols = n.div_ceil(rows).max(1);
    let height = field_height(n);
    let span = width - 96.0;
    let points = stars
        .iter()
        .enumerate()
        .map(|(i, star)| {
            let jx = (i64::from(crate::views::kit::design_hash(&format!("{}x", star.item)) % 21)
                - 10) as f32;
            let jy = (i64::from(crate::views::kit::design_hash(&format!("{}y", star.item)) % 13)
                - 6) as f32;
            let col = i / rows;
            let row = i % rows;
            let x =
                64.0 + if cols > 1 {
                    col as f32 * span / (cols - 1) as f32
                } else {
                    span / 2.0
                } + jx;
            let y =
                (row as f32 + 0.5) * height / rows as f32 + if rows > 1 { jy } else { jy / 2.0 };
            point(x, y)
        })
        .collect();
    (point(18.0, height / 2.0), points)
}

/// The color a star's core carries.
fn core_color(phase: Option<Phase>, pal: &Palette) -> Hsla {
    match phase {
        Some(Phase::Running) => pal.done,
        Some(Phase::Paused) => pal.warn,
        Some(Phase::Done) => pal.ok,
        Some(Phase::Failed) => pal.danger,
        Some(Phase::Cancelled) | None => pal.ghost,
    }
}

/// One twinkle's scale and opacity at `t`: 0.6 to 1.25 and back, 0.1
/// to 0.3 and back, over `period`.
fn twinkle(t: Duration, period: Duration) -> (f32, f32) {
    let phase = (t.as_secs_f32() % period.as_secs_f32()) / period.as_secs_f32();
    let wave = (1.0 - (phase * std::f32::consts::TAU).cos()) / 2.0;
    (0.6 + 0.65 * wave, 0.1 + 0.2 * wave)
}

/// Paints a filled circle.
fn circle(
    window: &mut Window,
    center: Point<Pixels>,
    r: f32,
    fill: Hsla,
    stroke: Option<(Hsla, f32)>,
) {
    let bounds = Bounds::new(
        point(center.x - px(r), center.y - px(r)),
        size(px(2.0 * r), px(2.0 * r)),
    );
    let (border, width) = stroke.map_or((gpui_kit::transparent_black(), 0.0), |(color, w)| {
        (color, w)
    });
    window.paint_quad(quad(
        bounds,
        px(r),
        fill,
        px(width),
        border,
        Default::default(),
    ));
}

/// The constellation of `stars`, opening a star's worker on click.
/// `origin` is the card's clock, so the twinkle continues across
/// repaints; `selected` marks the worker the workbench shows.
pub(crate) fn constellation(
    stars: Vec<Star>,
    selected: Option<String>,
    origin: Instant,
    on_open: impl Fn(String, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let pal = Palette::active(cx).clone();
    let height = field_height(stars.len());
    let painted: Rc<Cell<Option<Bounds<Pixels>>>> = Rc::default();
    let hit_stars = stars.clone();
    let hit_bounds = painted.clone();
    div()
        .w_full()
        .h(px(height))
        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
            let Some(bounds) = hit_bounds.get() else {
                return;
            };
            let width = f32::from(bounds.size.width);
            let (_, points) = layout(&hit_stars, width);
            let at = event.position - bounds.origin;
            let hit = points.iter().position(|p| {
                let (dx, dy) = (f32::from(at.x) - p.x, f32::from(at.y) - p.y);
                dx * dx + dy * dy <= HIT_R * HIT_R
            });
            if let Some(i) = hit {
                cx.stop_propagation();
                on_open(hit_stars[i].id.clone(), window, cx);
            }
        })
        .child(
            canvas(
                move |bounds, _, _| bounds,
                move |bounds, _, window, _| {
                    painted.set(Some(bounds));
                    let (lead, points) = layout(&stars, f32::from(bounds.size.width));
                    let at = |p: Point<f32>| bounds.origin + point(px(p.x), px(p.y));
                    let now = origin.elapsed();
                    let mut animate = false;
                    let phases: Vec<Phase> = stars.iter().map(|star| star.phase).collect();
                    let per_row = rows(stars.len());
                    for (i, p) in points.iter().enumerate() {
                        let (from, from_phase) = if i < per_row {
                            (lead, Some(Phase::Done))
                        } else {
                            (points[i - per_row], Some(phases[i - per_row]))
                        };
                        let (color, dashed) = match (from_phase, phases[i]) {
                            (_, Phase::Running | Phase::Paused) => (pal.done.opacity(0.55), true),
                            (Some(Phase::Done), Phase::Done) => (pal.ok.opacity(0.35), false),
                            (_, Phase::Failed) => (pal.danger.opacity(0.4), false),
                            _ => (pal.line, false),
                        };
                        let mut path = PathBuilder::stroke(px(1.0));
                        if dashed {
                            animate = true;
                            path = path.dash_array(&[px(3.0), px(4.0)]);
                        }
                        path.move_to(at(from));
                        path.line_to(at(*p));
                        if let Ok(path) = path.build() {
                            window.paint_path(path, color);
                        }
                    }
                    circle(window, at(lead), LEAD_R, pal.accent, None);
                    for (i, (star, p)) in stars.iter().zip(&points).enumerate() {
                        let center = at(*p);
                        let period = match star.phase {
                            Phase::Running => Some(Duration::from_millis(1600)),
                            Phase::Paused => Some(Duration::from_millis(2600)),
                            _ => None,
                        };
                        if let Some(period) = period {
                            animate = true;
                            let offset = Duration::from_millis((i as u64 * 263) % 1600);
                            let (scale, alpha) = twinkle(now + offset, period);
                            circle(
                                window,
                                center,
                                HALO_R * scale,
                                core_color(Some(star.phase), &pal).opacity(alpha),
                                None,
                            );
                        }
                        let core = core_color(Some(star.phase), &pal);
                        let selected = selected.as_deref() == Some(star.id.as_str());
                        let stroke = if selected {
                            (pal.ink_strong, 2.2)
                        } else {
                            (core, 1.4)
                        };
                        let fill = if matches!(star.phase, Phase::Cancelled) {
                            pal.ghost
                        } else {
                            core
                        };
                        circle(window, center, CORE_R, fill, Some(stroke));
                    }
                    if animate {
                        window.request_animation_frame();
                    }
                },
            )
            .size_full(),
        )
}

#[cfg(test)]
mod tests {
    use super::{Star, field_height, layout, rows, twinkle};
    use crate::views::agents::Phase;
    use std::time::Duration;

    fn stars(n: usize) -> Vec<Star> {
        (0..n)
            .map(|i| Star {
                id: format!("c{i}"),
                item: format!("item-{i}"),
                phase: Phase::Running,
            })
            .collect()
    }

    #[test]
    fn rows_grow_with_the_worker_count_like_the_design() {
        assert_eq!(
            (rows(8), rows(9), rows(24), rows(25), rows(65)),
            (1, 2, 2, 3, 4)
        );
        assert_eq!(field_height(4), 44.0);
        assert_eq!(field_height(30), 80.0);
    }

    #[test]
    fn a_layout_is_the_same_every_frame_and_keeps_launch_order_left_to_right() {
        let field = stars(6);
        let (lead, first) = layout(&field, 720.0);
        let (_, again) = layout(&field, 720.0);
        assert_eq!(first, again, "the jitter is seeded, not random");
        assert_eq!(lead.x, 18.0);
        assert!(first.windows(2).all(|pair| pair[0].x < pair[1].x));
    }

    #[test]
    fn a_twinkle_breathes_between_its_bounds() {
        let period = Duration::from_millis(1600);
        let (rest, faint) = twinkle(Duration::ZERO, period);
        let (peak, bright) = twinkle(Duration::from_millis(800), period);
        assert!((rest - 0.6).abs() < 1e-4 && (faint - 0.1).abs() < 1e-4);
        assert!((peak - 1.25).abs() < 1e-4 && (bright - 0.3).abs() < 1e-4);
    }
}
