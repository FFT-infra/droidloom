//! The feedback the side-swipe Back gesture was missing.
//!
//! Android's back gesture draws a pill with a chevron that tracks the finger:
//! it arrives as soon as the finger leaves the edge, opens up as the swipe
//! commits, and springs away on release. The geometry and springs here follow
//! AOSP's `BackPanel`/`EdgePanelParams` and `NavigationBarEdgePanel`; the
//! raster is done per frame into a shared-memory buffer because the values are
//! driven by the hand, not by a fixed bank of frames.
//!
//! Everything is drawn in the parent surface's logical coordinates through a
//! desynchronized subsurface, so it moves with the window and never needs a
//! round trip to appear.

use super::*;
use spring::Spring;
use std::fs::File;
use std::os::unix::fs::FileExt;
use wayland_client::protocol::{wl_shm, wl_subsurface};

/// Panel surface size, in logical pixels. Wide enough for the pill at its
/// furthest travel, tall enough to hold it.
const PANEL_WIDTH: u32 = 160;
const PANEL_HEIGHT: u32 = 72;
/// Inward travel that brings the pill to full size.
const ENTRY: f64 = 16.0;
/// Inward travel over which the pill stretches past full size.
const STRETCH: f64 = 200.0;
/// The pill at rest, and fully stretched.
const PILL_WIDTH: f64 = 48.0;
const PILL_STRETCH: f64 = 12.0;
const PILL_HEIGHT: f64 = 46.0;
const PILL_HEIGHT_STRETCH: f64 = 2.0;
const CORNER: f64 = 6.0;
const CORNER_STRETCH: f64 = 18.0;
/// Chevron leg length, and its half-angle at rest and fully stretched. The
/// stretched angle is AOSP's 55 degrees.
const ARROW_LENGTH: f64 = 11.0;
const ARROW_REST_DEGREES: f64 = 38.0;
const ARROW_STRETCH_DEGREES: f64 = 55.0;
/// Degrees the chevron opens per 1000 logical pixels per second of swipe,
/// capped at AOSP's four degree offset.
const ARROW_LEAN_PER_1000: f64 = 4.0;
const ARROW_LEAN_MAX_DEGREES: f64 = 4.0;
/// Half the chevron's stroke width.
const STROKE: f64 = 2.0;
/// Progress at which the chevron appears, and at which it disappears again.
/// AOSP steps the arrow alpha with hysteresis so it cannot flicker.
const ARROW_IN: f64 = 0.173;
const ARROW_OUT: f64 = 0.157;
/// Longest a release animation may run before the panel is taken down.
const FAILSAFE: f64 = 0.5;
/// How close a spring must be to its target to stop redrawing.
const EPSILON: f64 = 0.02;
/// Motion samples between indicator traces. Enough to read a swipe's geometry
/// out of the journal without one line per event.
const TRACE_EVERY: u32 = 8;

/// The slate AOSP's current back panel uses, day and night.
const NIGHT_PILL: [f32; 3] = [0x40 as f32, 0x46 as f32, 0x59 as f32];
const NIGHT_ARROW: [f32; 3] = [0xDC as f32, 0xE2 as f32, 0xF9 as f32];
const DAY_PILL: [f32; 3] = [0xC0 as f32, 0xC6 as f32, 0xDC as f32];
const DAY_ARROW: [f32; 3] = [0x15 as f32, 0x1B as f32, 0x2C as f32];

/// Which part of the release animation is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// The finger owns the geometry.
    Tracking,
    /// The swipe committed; the pill pops and the arrow flies out.
    Committed,
    /// The swipe was cancelled; the pill collapses back into the edge.
    Cancelled,
    /// Nothing left to draw.
    Done,
}

/// How many images the panel rotates through. A `wl_shm` buffer may not be
/// written while the compositor still holds it, and the presenter tracks no
/// release events for shared memory, so a rewritten slot must have been
/// attached several frames ago.
const SLOTS: usize = 3;

pub(super) struct EdgePanel {
    surface: wl_surface::WlSurface,
    role: wl_subsurface::WlSubsurface,
    slots: Vec<wl_buffer::WlBuffer>,
    slot: usize,
    file: File,
    pixels: Vec<u8>,
    scale: u32,
    from_left: bool,
    night: bool,
    /// Panel origin in the parent surface's coordinates.
    origin: (i32, i32),
    translation: Spring,
    width: Spring,
    alpha: Spring,
    pop: Spring,
    arrow_visible: bool,
    phase: Phase,
    elapsed: f64,
    /// Where the finger is, so the pill can follow it vertically.
    finger_y: f64,
    lean: f64,
    samples: u32,
}

impl EdgePanel {
    pub(super) fn new(
        globals: &layers::Globals,
        compositor: &CompositorState,
        parent: &wl_surface::WlSurface,
        qh: &QueueHandle<App>,
        scale_120: u32,
        from_left: bool,
    ) -> Result<Self, PresenterError> {
        let scale = (f64::from(scale_120) / f64::from(FRACTIONAL_SCALE_DENOMINATOR))
            .ceil()
            .clamp(1.0, 64.0) as u32;
        let width = PANEL_WIDTH * scale;
        let height = PANEL_HEIGHT * scale;
        let stride = i32::try_from(width * 4)
            .map_err(|_| PresenterError::Configuration("edge panel stride overflow"))?;
        let bytes = usize::try_from(u64::from(width) * u64::from(height) * 4)
            .map_err(|_| PresenterError::Configuration("edge panel size overflow"))?;
        let file = tempfile::tempfile()?;
        let total = bytes
            .checked_mul(SLOTS)
            .ok_or(PresenterError::Configuration("edge panel pool overflow"))?;
        file.set_len(total as u64)?;
        let size = i32::try_from(total)
            .map_err(|_| PresenterError::Configuration("edge panel pool too large"))?;
        let pool = globals.shm.create_pool(file.as_fd(), size, qh, ());
        let mut slots = Vec::with_capacity(SLOTS);
        for index in 0..SLOTS {
            let offset = i32::try_from(index * bytes)
                .map_err(|_| PresenterError::Configuration("edge panel offset overflow"))?;
            slots.push(pool.create_buffer(
                offset,
                i32::try_from(width).unwrap_or(i32::MAX),
                i32::try_from(height).unwrap_or(i32::MAX),
                stride,
                wl_shm::Format::Argb8888,
                qh,
                (),
            ));
        }
        pool.destroy();

        let surface = compositor.create_surface(qh);
        surface.set_buffer_scale(i32::try_from(scale).unwrap_or(1));
        let role = globals.subcompositor.get_subsurface(&surface, parent, qh, ());
        role.set_desync();
        // The indicator must never take input away from the application.
        let region = Region::new(compositor)?;
        surface.set_input_region(Some(region.wl_region()));
        drop(region);

        let night = sctk_adwaita::theme::ColorTheme::auto().active.button_icon.red() > 0.5;
        Ok(Self {
            surface,
            role,
            slots,
            slot: 0,
            file,
            pixels: vec![0; bytes],
            scale,
            from_left,
            night,
            origin: (0, 0),
            translation: Spring::new(spring::STIFFNESS_MEDIUM, 0.8, 0.0),
            width: Spring::new(spring::STIFFNESS_MEDIUM, 0.75, 0.0),
            alpha: Spring::new(spring::STIFFNESS_MEDIUM, 1.0, 0.0),
            pop: Spring::new(spring::STIFFNESS_HIGH, 1.0, 1.0),
            arrow_visible: false,
            phase: Phase::Tracking,
            elapsed: 0.0,
            finger_y: 0.0,
            lean: 0.0,
            samples: 0,
        })
    }

    pub(super) fn from_left(&self) -> bool {
        self.from_left
    }

    /// Whether the release animation still wants frames.
    pub(super) fn needs_frames(&self) -> bool {
        self.phase != Phase::Done
    }

    /// Whether the finger still owns the geometry.
    pub(super) fn tracking(&self) -> bool {
        self.phase == Phase::Tracking
    }

    /// Follow the finger. `inward` is how far it has moved away from its edge,
    /// `y` its position along the edge, `velocity` its inward speed.
    pub(super) fn track(&mut self, content: (u32, u32), inward: f64, y: f64, velocity: f64) {
        self.phase = Phase::Tracking;
        self.elapsed = 0.0;
        self.finger_y = y;

        let entry = (inward / ENTRY).clamp(0.0, 1.0);
        let stretch = ((inward - ENTRY).max(0.0) / STRETCH).clamp(0.0, 1.0);
        let target_width = PILL_WIDTH * ease(entry) + PILL_STRETCH * stretch;
        // The pill must stay inside the panel at any hand travel.
        let furthest = f64::from(PANEL_WIDTH.saturating_sub(PANEL_WIDTH / 8));
        let translation = inward.clamp(0.0, (furthest - PILL_WIDTH - PILL_STRETCH).max(0.0));

        self.translation.track(translation);
        self.width.track(target_width);
        self.alpha.track(entry * entry);
        self.pop.track(1.0);
        self.lean = (velocity / 1000.0 * ARROW_LEAN_PER_1000)
            .clamp(-ARROW_LEAN_MAX_DEGREES, ARROW_LEAN_MAX_DEGREES);
        self.arrow_visible = arrow_step(self.width.value() / PILL_WIDTH, self.arrow_visible);
        self.place(content);
        self.redraw();
        self.samples = self.samples.wrapping_add(1);
        if self.samples % TRACE_EVERY == 0 {
            eprintln!(
                "Droidloom trace: stage=swipe event=track edge={} inward={inward:.1} translation={:.1} width={:.1} alpha={:.2} finger_y={y:.1} origin={:?}",
                if self.from_left { "left" } else { "right" },
                self.translation.value(),
                self.width.value(),
                self.alpha.value(),
                self.origin,
            );
        }
    }

    /// The finger left. `confirmed` picks between the pop and the collapse.
    pub(super) fn release(&mut self, confirmed: bool) {
        if self.phase != Phase::Tracking {
            return;
        }
        self.phase = if confirmed {
            Phase::Committed
        } else {
            Phase::Cancelled
        };
        self.elapsed = 0.0;
        eprintln!(
            "Droidloom trace: stage=swipe event=release confirmed={confirmed} width={:.1} alpha={:.2} origin={:?}",
            self.width.value(),
            self.alpha.value(),
            self.origin,
        );
        self.translation.animate_to(self.translation.value(), 0.0);
        if confirmed {
            // AOSP's committed state: keep the width and throw the whole panel
            // out towards the edge with an initial kick on the scale.
            self.width
                .animate_to(self.width.value().max(PILL_WIDTH), 260.0);
            self.alpha.animate_to(0.0, 0.0);
            self.pop.animate_to(0.85, 3.0);
        } else {
            // AOSP's cancelled state: the pill's width collapses to nothing
            // while the panel dissolves.
            self.width.animate_to(0.0, 0.0);
            self.alpha.animate_to(0.0, 0.0);
            self.pop.animate_to(1.0, 0.0);
        }
    }

    /// Advance the release animation. `true` while the panel needs more frames.
    pub(super) fn advance(&mut self, seconds: f64) -> bool {
        if self.phase == Phase::Done {
            return false;
        }
        if self.phase == Phase::Tracking {
            return true;
        }
        self.elapsed += seconds;
        for spring in [
            &mut self.translation,
            &mut self.width,
            &mut self.alpha,
            &mut self.pop,
        ] {
            spring.advance(seconds);
        }
        self.arrow_visible = arrow_step(self.width.value() / PILL_WIDTH, self.arrow_visible);
        self.redraw();
        let settled = self.translation.settled(EPSILON)
            && self.width.settled(EPSILON)
            && self.alpha.settled(EPSILON)
            && self.pop.settled(EPSILON);
        // The failsafe is a deadline, not a hint: an indicator the springs
        // never settle must still leave the screen.
        if settled || self.elapsed >= FAILSAFE {
            self.phase = Phase::Done;
            self.width.track(0.0);
            self.surface.attach(None, 0, 0);
            self.surface.commit();
            eprintln!(
                "Droidloom trace: stage=swipe event=done elapsed={:.3} settled={settled}",
                self.elapsed
            );
            return false;
        }
        true
    }

    /// Put the panel where the finger is, so the pill follows it vertically.
    fn place(&mut self, content: (u32, u32)) {
        let x = if self.from_left {
            0
        } else {
            i32::try_from(content.0.saturating_sub(PANEL_WIDTH)).unwrap_or(0)
        };
        let centred = self.finger_y - f64::from(PANEL_HEIGHT) / 2.0;
        let limit = f64::from(content.1.saturating_sub(PANEL_HEIGHT));
        // Clamped into the window, so the cast cannot lose anything.
        let y = centred.clamp(0.0, limit).round() as i32;
        if self.origin == (x, y) {
            return;
        }
        self.origin = (x, y);
        self.role.set_position(x, y);
    }

    fn redraw(&mut self) {
        // Rotate first: the slot written below is the one being attached, and
        // it must be the one the compositor let go of longest ago.
        self.slot = (self.slot + 1) % SLOTS;
        self.raster();
        self.surface.attach(Some(&self.slots[self.slot]), 0, 0);
        self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.surface.commit();
    }

    fn raster(&mut self) {
        let (pill_color, arrow_color) = if self.night {
            (NIGHT_PILL, NIGHT_ARROW)
        } else {
            (DAY_PILL, DAY_ARROW)
        };
        let panel_alpha = self.alpha.value().clamp(0.0, 1.0);
        if panel_alpha <= 0.0 || self.width.value() <= f64::EPSILON {
            self.pixels.fill(0);
            self.write_pixels();
            return;
        }
        let pop = self.pop.value().clamp(0.2, 2.0);
        let width = self.width.value().max(0.0);
        let height = PILL_HEIGHT + PILL_HEIGHT_STRETCH * stretch_of(width);
        let corner = CORNER + CORNER_STRETCH * stretch_of(width);
        // The scale pop shrinks the pill about its own centre.
        let pill_width = width * pop;
        let pill_height = height * pop;
        let x0 = self.translation.value() + (width - pill_width) / 2.0;
        let x1 = x0 + pill_width;
        let centre_y = f64::from(PANEL_HEIGHT) / 2.0;
        let y0 = centre_y - pill_height / 2.0;
        let y1 = centre_y + pill_height / 2.0;

        let half_angle = (ARROW_REST_DEGREES
            + (ARROW_STRETCH_DEGREES - ARROW_REST_DEGREES) * stretch_of(width)
            + self.lean)
            .to_radians();
        let leg = (
            ARROW_LENGTH * half_angle.cos(),
            ARROW_LENGTH * half_angle.sin(),
        );
        let tip = ((x0 + x1) / 2.0 - leg.0 / 2.0, centre_y);
        let arrow_alpha = if self.arrow_visible {
            panel_alpha
        } else {
            0.0
        };

        let scale = self.scale;
        for y in 0..PANEL_HEIGHT * scale {
            for x in 0..PANEL_WIDTH * scale {
                let mut sample_x = (f64::from(x) + 0.5) / f64::from(scale);
                let sample_y = (f64::from(y) + 0.5) / f64::from(scale);
                if !self.from_left {
                    sample_x = f64::from(PANEL_WIDTH) - sample_x;
                }
                let inside_pill = rounded_rect(sample_x, sample_y, x0, y0, x1, y1, corner) <= 0.0;
                let on_arrow = arrow_alpha > 0.0
                    && (distance_to_segment(sample_x, sample_y, tip, (tip.0 + leg.0, tip.1 - leg.1))
                        <= STROKE
                        || distance_to_segment(
                            sample_x,
                            sample_y,
                            tip,
                            (tip.0 + leg.0, tip.1 + leg.1),
                        ) <= STROKE);
                let offset = ((y * PANEL_WIDTH * scale + x) * 4) as usize;
                let quad = if on_arrow {
                    premultiply(arrow_color, arrow_alpha)
                } else if inside_pill {
                    premultiply(pill_color, panel_alpha)
                } else {
                    [0, 0, 0, 0]
                };
                self.pixels[offset..offset + 4].copy_from_slice(&quad);
            }
        }
        self.write_pixels();
    }

    fn write_pixels(&mut self) {
        let offset = (self.slot * self.pixels.len()) as u64;
        if let Err(error) = self.file.write_all_at(&self.pixels, offset) {
            eprintln!("Droidloom could not draw the edge indicator: {error}");
        }
    }
}

impl Drop for EdgePanel {
    fn drop(&mut self) {
        self.role.destroy();
        self.surface.destroy();
        for slot in self.slots.drain(..) {
            slot.destroy();
        }
    }
}

/// The chevron's alpha, stepped with hysteresis so it cannot flicker while a
/// hand hovers around the threshold.
fn arrow_step(progress: f64, visible: bool) -> bool {
    if progress > ARROW_IN {
        true
    } else if progress < ARROW_OUT {
        false
    } else {
        visible
    }
}

/// How far the pill has stretched past its rest size, in 0..1.
fn stretch_of(width: f64) -> f64 {
    ((width - PILL_WIDTH) / PILL_STRETCH).clamp(0.0, 1.0)
}

/// Slow-in, slow-out, so the pill arrives rather than snaps.
fn ease(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Signed distance to a rounded rectangle; negative inside.
fn rounded_rect(x: f64, y: f64, x0: f64, y0: f64, x1: f64, y1: f64, radius: f64) -> f64 {
    let half_width = ((x1 - x0) / 2.0).max(0.0);
    let half_height = ((y1 - y0) / 2.0).max(0.0);
    let radius = radius.clamp(0.0, half_width.min(half_height));
    let dx = (x - (x0 + x1) / 2.0).abs() - (half_width - radius);
    let dy = (y - (y0 + y1) / 2.0).abs() - (half_height - radius);
    let outside = dx.max(0.0).hypot(dy.max(0.0));
    outside + dx.max(dy).min(0.0) - radius
}

fn distance_to_segment(x: f64, y: f64, (ax, ay): (f64, f64), (bx, by): (f64, f64)) -> f64 {
    let (dx, dy) = (bx - ax, by - ay);
    let length = dx * dx + dy * dy;
    let t = if length > 0.0 {
        (((x - ax) * dx + (y - ay) * dy) / length).clamp(0.0, 1.0)
    } else {
        0.0
    };
    (x - ax - t * dx).hypot(y - ay - t * dy)
}

/// ARGB8888 over `wl_shm` is premultiplied, and ARGB32 is a native-endian
/// word: on a little-endian machine the bytes land blue, green, red, alpha.
fn premultiply(rgb: [f32; 3], alpha: f64) -> [u8; 4] {
    let alpha = alpha.clamp(0.0, 1.0);
    let channel = |value: f32| (f64::from(value) * alpha).round().clamp(0.0, 255.0) as u8;
    [
        channel(rgb[2]),
        channel(rgb[1]),
        channel(rgb[0]),
        (alpha * 255.0).round() as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pill_grows_with_the_finger_and_keeps_a_floor_under_it() {
        // Width is monotone in travel, so the indicator never jumps backwards.
        let mut previous = -1.0;
        for step in 0..=200 {
            let inward = f64::from(step);
            let entry = (inward / ENTRY).clamp(0.0, 1.0);
            let stretch = ((inward - ENTRY).max(0.0) / STRETCH).clamp(0.0, 1.0);
            let width = PILL_WIDTH * ease(entry) + PILL_STRETCH * stretch;
            assert!(width >= previous, "width fell at inward={inward}");
            previous = width;
        }
        assert!(previous > PILL_WIDTH);
    }

    #[test]
    fn the_chevron_only_appears_once_the_swipe_is_under_way() {
        let mut visible = false;
        assert!(!arrow_step(0.0, visible));
        assert!(!arrow_step(0.1, visible));
        visible = arrow_step(0.5, visible);
        assert!(visible);
        // Between the two thresholds the state is held, so a hand resting on
        // the boundary cannot make the chevron flicker.
        visible = arrow_step(0.16, visible);
        assert!(visible);
        visible = arrow_step(0.165, visible);
        assert!(visible);
        visible = arrow_step(0.0, visible);
        assert!(!visible);
    }

    #[test]
    fn the_rounded_rect_is_negative_inside_and_positive_outside() {
        assert!(rounded_rect(5.0, 5.0, 0.0, 0.0, 10.0, 10.0, 2.0) < 0.0);
        assert!(rounded_rect(0.5, 0.5, 0.0, 0.0, 10.0, 10.0, 2.0) > 0.0);
        assert!(rounded_rect(-1.0, 5.0, 0.0, 0.0, 10.0, 10.0, 2.0) > 0.0);
        // A zero-width rect is the collapsed pill: nothing is inside it.
        assert!(rounded_rect(0.0, 5.0, 0.0, 0.0, 0.0, 10.0, 0.0) >= 0.0);
    }

    #[test]
    fn premultiplied_alpha_never_exceeds_its_own_alpha() {
        for alpha in [0.0, 0.25, 0.5, 1.0] {
            let quad = premultiply([255.0, 128.0, 0.0], alpha);
            for channel in quad[..3].iter() {
                assert!(*channel <= quad[3]);
            }
        }
        assert_eq!(premultiply([255.0; 3], 0.0), [0, 0, 0, 0]);
        assert_eq!(premultiply([255.0; 3], 1.0), [255, 255, 255, 255]);
    }

    #[test]
    fn an_opaque_colour_keeps_its_channels_in_native_byte_order() {
        // 0x404659 is the AOSP night pill: blue 0x59 first, alpha last.
        let quad = premultiply([0x40 as f32, 0x46 as f32, 0x59 as f32], 1.0);
        assert_eq!(quad, [0x59, 0x46, 0x40, 255]);
    }

    #[test]
    fn the_panel_never_places_the_pill_past_its_own_edge() {
        let furthest = f64::from(PANEL_WIDTH.saturating_sub(PANEL_WIDTH / 8));
        let limit = (furthest - PILL_WIDTH - PILL_STRETCH).max(0.0);
        let translation = 10_000.0_f64.clamp(0.0, limit);
        assert!(translation + PILL_WIDTH + PILL_STRETCH <= furthest);
    }
}
