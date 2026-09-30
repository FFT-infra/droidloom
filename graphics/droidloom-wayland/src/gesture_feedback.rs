//! What the edge gestures draw while a finger is still down.
//!
//! The recognizers stay pure; this module owns the picture. Both feedbacks are
//! banks of pre-rendered stages — a teardrop that grows out of the side edge
//! for the Back swipe, a pull tab that drops out of the top edge for the top
//! pull — and the hand picks the stage. Nothing here is driven by a clock, so
//! the feedback is exactly as responsive as the finger and costs one attach per
//! motion sample.
//!
//! Each window gets one desynchronized subsurface of its own with an empty
//! input region: the indicator must never take a touch away from the
//! application, and it must move with the window without a round trip.

use super::*;
use std::io::Write;
use wayland_client::protocol::{wl_shm, wl_subsurface};

/// The side teardrop: a 48x144 logical stage, tall enough that the shape's
/// middle can follow the finger anywhere down the window.
const SIDE_WIDTH: u32 = 48;
const SIDE_HEIGHT: u32 = 144;
const SIDE_STAGES: usize = 20;
/// Inward travel before the teardrop appears, and the travel each stage adds:
/// `SIDE_APPEAR + SIDE_STAGES * SIDE_STAGE` is 40 logical pixels, just short of
/// the 48 that confirm the swipe.
const SIDE_APPEAR: f64 = 4.0;
const SIDE_STAGE: f64 = 1.8;
/// The deepest teardrop's reach from the edge at stage 0, and per stage.
const SIDE_DEPTH: f32 = 2.0;
const SIDE_DEPTH_STAGE: f32 = 2.0;

/// The top pull tab: 72x32 logical, centred on the window's top edge.
const TOP_WIDTH: u32 = 72;
const TOP_HEIGHT: u32 = 32;
const TOP_STAGES: usize = 12;
/// Pull before the tab appears, and the travel each stage adds.
const TOP_APPEAR: f64 = 3.0;
const TOP_STAGE: f64 = 2.5;
const TOP_DEPTH: f32 = 4.0;
const TOP_DEPTH_STAGE: f32 = 2.0;
/// Half the tab's width, and the radius that rounds its lower corners.
const TOP_HALF_WIDTH: f32 = 26.0;
const TOP_CORNER: f32 = 10.0;

/// The dark slab both indicators are cut from, and the white they draw with.
const SLAB: [f32; 3] = [0.10, 0.11, 0.13];
const SLAB_ALPHA: f32 = 0.88;
const INK: [f32; 3] = [1.0, 1.0, 1.0];

/// Half the line width the arrows are stroked with.
const SIDE_ARROW_STROKE: f32 = 1.35;
const TOP_ARROW_STROKE: f32 = 1.25;
/// Stage at which each arrow starts to appear, and the stages it fades in over:
/// the shape arrives first, the chevron once the gesture is going somewhere.
const SIDE_ARROW_START: usize = 7;
const SIDE_ARROW_RAMP: usize = 7;
const TOP_ARROW_START: usize = 4;
const TOP_ARROW_RAMP: usize = 4;

fn paint(rgb: [f32; 3], alpha: f32) -> [f32; 4] {
    [rgb[0] * alpha, rgb[1] * alpha, rgb[2] * alpha, alpha]
}

/// Source-over in premultiplied form, which is what `wl_shm`'s ARGB8888 wants.
fn over(below: [f32; 4], above: [f32; 4]) -> [f32; 4] {
    let mix = |index: usize| above[index] + below[index] * (1.0 - above[3]);
    [
        mix(0),
        mix(1),
        mix(2),
        above[3] + below[3] * (1.0 - above[3]),
    ]
}

fn distance_to_segment(x: f32, y: f32, (ax, ay): (f32, f32), (bx, by): (f32, f32)) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let length = dx * dx + dy * dy;
    let t = if length > 0.0 {
        (((x - ax) * dx + (y - ay) * dy) / length).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (x - ax - t * dx).hypot(y - ay - t * dy)
}

/// The teardrop's reach at `norm_y` down the stage: a point at the edge,
/// opening along a rounded flank into full depth at the middle and back.
fn teardrop(norm_y: f32) -> f32 {
    let t = norm_y.clamp(0.0, 1.0);
    let y = if t > 0.5 { 1.0 - t } else { t };
    if y <= 0.167 {
        let p = y / 0.167;
        p * p * 0.115
    } else if y <= 0.350 {
        let p = (y - 0.167) / (0.350 - 0.167);
        0.115 + (0.620 - 0.115) * (p * p * (3.0 - 2.0 * p))
    } else {
        let p = (y - 0.350) / (0.500 - 0.350);
        0.620 + (1.000 - 0.620) * (p * (2.0 - p))
    }
}

/// How strongly a stage draws its arrow.
fn arrow_intensity(stage: usize, start: usize, ramp: usize) -> f32 {
    match stage {
        s if s < start => 0.0,
        s if s < start + ramp => (s - start) as f32 / ramp as f32,
        _ => 1.0,
    }
}

/// One stage of the side teardrop, growing inward from `from_left`'s edge.
///
/// The chevron points left on both edges: it names the gesture — Back — rather
/// than the direction the hand happened to travel.
fn side_image(from_left: bool, stage: usize, scale: u32) -> Vec<u32> {
    let width = SIDE_WIDTH * scale;
    let height = SIDE_HEIGHT * scale;
    let mut pixels = vec![0u32; (width * height) as usize];

    let depth = SIDE_DEPTH + stage.min(SIDE_STAGES - 1) as f32 * SIDE_DEPTH_STAGE;
    let arrow = arrow_intensity(stage, SIDE_ARROW_START, SIDE_ARROW_RAMP);
    let centre_x = if from_left {
        depth * 0.52
    } else {
        SIDE_WIDTH as f32 - depth * 0.52
    };
    let centre_y = SIDE_HEIGHT as f32 / 2.0;
    let tip = (centre_x - 3.5, centre_y);
    let top = (centre_x + 2.5, centre_y - 6.0);
    let bottom = (centre_x + 2.5, centre_y + 6.0);

    for y in 0..height {
        let sample_y = (y as f32 + 0.5) / scale as f32;
        let reach = teardrop(sample_y / SIDE_HEIGHT as f32) * depth;
        for x in 0..width {
            let sample_x = (x as f32 + 0.5) / scale as f32;
            let from_edge = if from_left {
                sample_x
            } else {
                SIDE_WIDTH as f32 - sample_x
            };
            let slab = (reach - from_edge).clamp(0.0, 1.0) * SLAB_ALPHA;
            let mut pixel = paint(SLAB, slab);
            if arrow > 0.0 {
                let distance = distance_to_segment(sample_x, sample_y, top, tip)
                    .min(distance_to_segment(sample_x, sample_y, bottom, tip));
                let ink = (SIDE_ARROW_STROKE + 0.5 - distance).clamp(0.0, 1.0) * arrow;
                pixel = over(pixel, paint(INK, ink));
            }
            pixels[(y * width + x) as usize] = encode(pixel);
        }
    }
    pixels
}

/// One stage of the top pull tab, hanging from the top edge.
fn top_image(stage: usize, scale: u32) -> Vec<u32> {
    let width = TOP_WIDTH * scale;
    let height = TOP_HEIGHT * scale;
    let mut pixels = vec![0u32; (width * height) as usize];

    let depth = TOP_DEPTH + stage.min(TOP_STAGES - 1) as f32 * TOP_DEPTH_STAGE;
    let arrow = arrow_intensity(stage, TOP_ARROW_START, TOP_ARROW_RAMP);
    let centre_x = TOP_WIDTH as f32 / 2.0;
    let corner = TOP_CORNER.min(depth / 2.0);
    let chevron_y = (depth * 0.6).max(8.0);
    let left = (centre_x - 5.5, chevron_y - 2.5);
    let right = (centre_x + 5.5, chevron_y - 2.5);
    let tip = (centre_x, chevron_y + 3.0);

    for y in 0..height {
        let sample_y = (y as f32 + 0.5) / scale as f32;
        if sample_y > depth + 1.0 {
            continue;
        }
        for x in 0..width {
            let sample_x = (x as f32 + 0.5) / scale as f32;
            // Distance inside the slab: down from the top edge, and in from
            // whichever side is nearer, with the lower corners rounded off.
            let side = TOP_HALF_WIDTH - (sample_x - centre_x).abs();
            if side < -1.0 {
                continue;
            }
            let down = depth - sample_y;
            let inside = if side < corner && down < corner {
                corner - (corner - side).hypot(corner - down)
            } else {
                down.min(side)
            };
            let slab = inside.clamp(0.0, 1.0) * SLAB_ALPHA;
            let mut pixel = paint(SLAB, slab);
            if arrow > 0.0 {
                let distance = distance_to_segment(sample_x, sample_y, left, tip)
                    .min(distance_to_segment(sample_x, sample_y, right, tip));
                let ink = (TOP_ARROW_STROKE + 0.5 - distance).clamp(0.0, 1.0) * arrow;
                pixel = over(pixel, paint(INK, ink));
            }
            pixels[(y * width + x) as usize] = encode(pixel);
        }
    }
    pixels
}

fn encode(pixel: [f32; 4]) -> u32 {
    let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u32;
    (byte(pixel[3]) << 24) | (byte(pixel[0]) << 16) | (byte(pixel[1]) << 8) | byte(pixel[2])
}

/// Which teardrop stage an inward travel shows, or `None` while the finger is
/// still too close to its edge to draw anything.
fn side_stage(inward: f64) -> Option<usize> {
    if !inward.is_finite() || inward < SIDE_APPEAR {
        return None;
    }
    let stage = ((inward - SIDE_APPEAR) / SIDE_STAGE) as usize;
    Some(stage.min(SIDE_STAGES - 1))
}

/// Which pull tab stage a downward travel shows.
fn top_stage(pull: f64) -> Option<usize> {
    if !pull.is_finite() || pull < TOP_APPEAR {
        return None;
    }
    let stage = ((pull - TOP_APPEAR) / TOP_STAGE) as usize;
    Some(stage.min(TOP_STAGES - 1))
}

/// Where the teardrop's stage sits: centred on where the finger landed, and
/// kept inside the window.
fn side_origin(from_left: bool, along: f64, window: (u32, u32)) -> (i32, i32) {
    let x = if from_left {
        0
    } else {
        i32::try_from(window.0.saturating_sub(SIDE_WIDTH)).unwrap_or(0)
    };
    let lowest = f64::from(window.1.saturating_sub(SIDE_HEIGHT)).max(0.0);
    let centred = along - f64::from(SIDE_HEIGHT) / 2.0;
    let y = if centred.is_finite() {
        centred.clamp(0.0, lowest).round() as i32
    } else {
        0
    };
    (x, y)
}

/// Where the pull tab's stage sits: centred on the window's top edge.
fn top_origin(window: (u32, u32)) -> (i32, i32) {
    (
        i32::try_from(window.0.saturating_sub(TOP_WIDTH) / 2).unwrap_or(0),
        0,
    )
}

/// The gesture feedback of one window.
pub(super) struct Feedback {
    surface: wl_surface::WlSurface,
    role: wl_subsurface::WlSubsurface,
    buffers: Vec<wl_buffer::WlBuffer>,
    /// Index of the teardrop's first stage and of the pull tab's, in the order
    /// they were written into the pool.
    side: usize,
    top: usize,
    visible: bool,
}

impl Drop for Feedback {
    fn drop(&mut self) {
        self.role.destroy();
        self.surface.destroy();
        for buffer in self.buffers.drain(..) {
            buffer.destroy();
        }
    }
}

impl Feedback {
    pub(super) fn new(
        globals: &layers::Globals,
        compositor: &CompositorState,
        parent: &wl_surface::WlSurface,
        qh: &QueueHandle<App>,
        scale_120: u32,
    ) -> Result<Self, PresenterError> {
        let scale = (f64::from(scale_120) / f64::from(FRACTIONAL_SCALE_DENOMINATOR))
            .ceil()
            .clamp(1.0, 64.0) as u32;
        let error = |what: &str| PresenterError::WindowDecoration(what.to_owned());

        // Every stage goes into one pool: the bank is fixed, and an attach per
        // motion sample picks between the stages.
        let mut stages: Vec<(u32, u32, Vec<u32>)> = Vec::new();
        for from_left in [true, false] {
            for stage in 0..SIDE_STAGES {
                stages.push((
                    SIDE_WIDTH * scale,
                    SIDE_HEIGHT * scale,
                    side_image(from_left, stage, scale),
                ));
            }
        }
        let side = 0;
        let top = stages.len();
        for stage in 0..TOP_STAGES {
            stages.push((
                TOP_WIDTH * scale,
                TOP_HEIGHT * scale,
                top_image(stage, scale),
            ));
        }

        let mut file = tempfile::tempfile()?;
        let mut total = 0_usize;
        for (_, _, pixels) in &stages {
            let mut bytes = Vec::with_capacity(pixels.len() * 4);
            for pixel in pixels {
                bytes.extend_from_slice(&pixel.to_ne_bytes());
            }
            file.write_all(&bytes)?;
            total += bytes.len();
        }
        let size = i32::try_from(total).map_err(|_| error("gesture feedback pool too large"))?;
        let pool = globals.shm.create_pool(file.as_fd(), size, qh, ());
        let mut buffers = Vec::with_capacity(stages.len());
        let mut offset = 0_usize;
        for (width, height, pixels) in &stages {
            let stride = i32::try_from(width * 4)
                .map_err(|_| error("gesture feedback stride overflow"))?;
            buffers.push(pool.create_buffer(
                i32::try_from(offset).map_err(|_| error("gesture feedback offset overflow"))?,
                i32::try_from(*width).unwrap_or(i32::MAX),
                i32::try_from(*height).unwrap_or(i32::MAX),
                stride,
                wl_shm::Format::Argb8888,
                qh,
                (),
            ));
            offset += pixels.len() * 4;
        }
        pool.destroy();

        let surface = compositor.create_surface(qh);
        surface.set_buffer_scale(i32::try_from(scale).unwrap_or(1));
        let role = globals.subcompositor.get_subsurface(&surface, parent, qh, ());
        role.set_desync();
        // Feedback is never a target: an empty input region keeps every touch
        // on the application underneath it.
        let region = Region::new(compositor)?;
        surface.set_input_region(Some(region.wl_region()));
        drop(region);
        surface.attach(None, 0, 0);
        surface.commit();

        Ok(Self {
            surface,
            role,
            buffers,
            side,
            top,
            visible: false,
        })
    }

    /// Follow a Back swipe: the teardrop's depth tracks the finger, and it
    /// stays on the spot the finger landed on. `inward` and `along` are logical.
    pub(super) fn show_side(&mut self, from_left: bool, inward: f64, along: f64, window: (u32, u32)) {
        let Some(stage) = side_stage(inward) else {
            self.hide();
            return;
        };
        let index = self.side + usize::from(!from_left) * SIDE_STAGES + stage;
        let (x, y) = side_origin(from_left, along, window);
        self.show(index, x, y);
    }

    /// Follow a top pull. This is feedback only: nothing about the window's
    /// state changes here, which is what keeps it from fighting the shell for
    /// the same touch.
    pub(super) fn show_top(&mut self, pull: f64, window: (u32, u32)) {
        let Some(stage) = top_stage(pull) else {
            self.hide();
            return;
        };
        let index = self.top + stage;
        let (x, y) = top_origin(window);
        self.show(index, x, y);
    }

    fn show(&mut self, index: usize, x: i32, y: i32) {
        let Some(buffer) = self.buffers.get(index) else {
            return;
        };
        self.role.set_position(x, y);
        self.surface.attach(Some(buffer), 0, 0);
        self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.surface.commit();
        self.visible = true;
    }

    pub(super) fn hide(&mut self) {
        if self.visible {
            self.surface.attach(None, 0, 0);
            self.surface.commit();
            self.visible = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Leftmost and rightmost column of a stage that marks any pixel.
    fn span(pixels: &[u32], width: u32, height: u32) -> (u32, u32) {
        let ink = |x: u32| {
            (0..height).any(|y| pixels[(y * width + x) as usize] >> 24 != 0)
        };
        let left = (0..width).find(|x| ink(*x)).unwrap_or(width);
        let right = (0..width).rev().find(|x| ink(*x)).unwrap_or(0);
        (left, right)
    }

    #[test]
    fn the_side_stages_cover_the_swipe_up_to_the_confirm_distance() {
        assert_eq!(side_stage(0.0), None);
        assert_eq!(side_stage(SIDE_APPEAR - 0.1), None);
        assert_eq!(side_stage(SIDE_APPEAR), Some(0));
        // A stage boundary is a value the arithmetic cannot land on exactly,
        // so pin it from both sides instead of at the knife edge.
        assert_eq!(side_stage(SIDE_APPEAR + SIDE_STAGE - 0.05), Some(0));
        assert_eq!(side_stage(SIDE_APPEAR + SIDE_STAGE + 0.05), Some(1));
        // Full depth is reached before the 48 logical pixels that confirm the
        // swipe, so the shape never stands still mid-gesture.
        let last = SIDE_APPEAR + (SIDE_STAGES - 1) as f64 * SIDE_STAGE;
        assert!(last < gesture::CONFIRM_DISTANCE, "{last} outruns the confirm");
        assert_eq!(side_stage(last), Some(SIDE_STAGES - 1));
        assert_eq!(side_stage(1_000.0), Some(SIDE_STAGES - 1));
        assert_eq!(side_stage(f64::NAN), None);
    }

    #[test]
    fn the_top_stages_only_draw_a_pull_that_has_started() {
        assert_eq!(top_stage(0.0), None);
        assert_eq!(top_stage(TOP_APPEAR - 0.1), None);
        assert_eq!(top_stage(TOP_APPEAR), Some(0));
        assert_eq!(top_stage(TOP_APPEAR + TOP_STAGE - 0.05), Some(0));
        assert_eq!(top_stage(TOP_APPEAR + TOP_STAGE + 0.05), Some(1));
        assert_eq!(top_stage(1_000.0), Some(TOP_STAGES - 1));
        assert_eq!(top_stage(f64::NAN), None);
    }

    #[test]
    fn every_stage_is_the_size_the_buffer_claims() {
        for scale in [1, 2, 3] {
            for stage in [0, SIDE_STAGES / 2, SIDE_STAGES - 1] {
                for from_left in [true, false] {
                    assert_eq!(
                        side_image(from_left, stage, scale).len(),
                        (SIDE_WIDTH * SIDE_HEIGHT * scale * scale) as usize
                    );
                }
            }
            for stage in [0, TOP_STAGES / 2, TOP_STAGES - 1] {
                assert_eq!(
                    top_image(stage, scale).len(),
                    (TOP_WIDTH * TOP_HEIGHT * scale * scale) as usize
                );
            }
        }
    }

    #[test]
    fn each_teardrop_grows_from_its_own_edge() {
        let (left, right) = span(&side_image(true, SIDE_STAGES - 1, 1), SIDE_WIDTH, SIDE_HEIGHT);
        assert_eq!(left, 0);
        assert!(right < SIDE_WIDTH - 1, "the left teardrop reaches {right}");
        let (left, right) = span(&side_image(false, SIDE_STAGES - 1, 1), SIDE_WIDTH, SIDE_HEIGHT);
        assert_eq!(right, SIDE_WIDTH - 1);
        assert!(left > 0, "the right teardrop reaches {left}");
        // Depth grows with the stage, monotonically and inside the stage.
        let width = |stage| {
            let (left, right) = span(&side_image(true, stage, 1), SIDE_WIDTH, SIDE_HEIGHT);
            right - left + 1
        };
        assert!(width(0) < width(SIDE_STAGES / 2));
        assert!(width(SIDE_STAGES / 2) < width(SIDE_STAGES - 1));
    }

    #[test]
    fn the_arrows_fade_in_only_once_the_shape_is_there() {
        // Ink is brighter than the slab it is drawn on, which is what makes an
        // arrow visible at all.
        assert!(INK[0] > SLAB[0]);
        assert_eq!(arrow_intensity(0, SIDE_ARROW_START, SIDE_ARROW_RAMP), 0.0);
        assert_eq!(arrow_intensity(6, SIDE_ARROW_START, SIDE_ARROW_RAMP), 0.0);
        assert!(arrow_intensity(8, SIDE_ARROW_START, SIDE_ARROW_RAMP) > 0.0);
        assert_eq!(arrow_intensity(20, SIDE_ARROW_START, SIDE_ARROW_RAMP), 1.0);
        assert_eq!(arrow_intensity(0, TOP_ARROW_START, TOP_ARROW_RAMP), 0.0);
        assert_eq!(arrow_intensity(20, TOP_ARROW_START, TOP_ARROW_RAMP), 1.0);
    }

    #[test]
    fn the_origin_follows_the_finger_and_stays_on_screen() {
        let window = (1524, 1016);
        // The middle of the teardrop sits on the finger.
        assert_eq!(side_origin(true, 400.0, window), (0, 328));
        assert_eq!(side_origin(false, 400.0, window), (1524 - 48, 328));
        // Near either end it is pushed back inside the window.
        assert_eq!(side_origin(true, 10.0, window), (0, 0));
        assert_eq!(side_origin(true, 1000.0, window), (0, 1016 - 144));
        assert_eq!(side_origin(true, f64::NAN, window), (0, 0));
        // A window shorter than the stage still gets a usable position.
        assert_eq!(side_origin(true, 10.0, (1524, 100)), (0, 0));
        assert_eq!(top_origin(window), ((1524 - 72) / 2, 0));
    }
}
