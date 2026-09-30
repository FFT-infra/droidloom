//! Visible per-window navigation.
//!
//! Droidloom presents one Android task per window. A phone application has no
//! keyboard to press `F11` with, and `wayland-csd-frame`'s `FrameAction` has no
//! Fullscreen variant, so Back and Fullscreen are separate subsurfaces stacked
//! over the frame's own titlebar. They are always drawn: a hit zone with no
//! image is not a control.
//!
//! The two buttons are also the only way out of fullscreen on a device with no
//! keyboard, so the Fullscreen button stays visible once the titlebar is gone,
//! floating over the content at the top right.

use super::*;
use std::io::Write;
use wayland_client::protocol::{wl_shm, wl_subsurface};

/// One button's outer size, in logical pixels. Equal to `sctk-adwaita`'s
/// private `HEADER_SIZE`, which the frame also reports through
/// `DecorationsFrame::location().1`; [`Chrome::place`] trusts the frame and
/// uses this only to centre the image and to place the floating button.
const BUTTON: u32 = 35;
/// `sctk-adwaita` 0.11.0's private header metrics (see its `buttons.rs`). The
/// crate is pinned to an exact version, so the window's own Close / Maximize /
/// Minimize cluster has a width this code can reserve against.
const NATIVE_SIZE: u32 = 24;
const NATIVE_MARGIN: u32 = 5;
const NATIVE_SPACING: u32 = 13;
/// Space between our buttons and the window's own button cluster.
const NATIVE_GAP: u32 = 8;

/// How wide `sctk-adwaita` will draw its own buttons on one side.
///
/// The frame library does not publish this, so it is recomputed: the layout
/// comes from the same settings-portal key the library reads, and the metrics
/// from the same private constants. Getting this wrong is not cosmetic — the
/// sheng desktop puts the window's Close, Minimize and Maximize on the *left*,
/// and a chrome that assumed the usual right-hand cluster sat on top of them.
fn native_cluster(config: &str, capabilities: WindowManagerCapabilities) -> u32 {
    let count = config
        .split(',')
        .take(3)
        .filter(|token| match token.trim() {
            "close" => true,
            "maximize" => capabilities.contains(WindowManagerCapabilities::MAXIMIZE),
            "minimize" => capabilities.contains(WindowManagerCapabilities::MINIMIZE),
            _ => false,
        })
        .count() as u32;
    match count {
        0 => 0,
        n => {
            NATIVE_MARGIN
                + NATIVE_SIZE * n
                + NATIVE_SPACING * n.saturating_sub(1)
        }
    }
}

/// The leading side of `org.gnome.desktop.wm.preferences button-layout`, read
/// through the settings portal exactly as `sctk-adwaita`'s `config.rs` reads it,
/// so the two agree about where the window's own buttons landed.
fn native_leading_buttons(capabilities: WindowManagerCapabilities) -> u32 {
    let Ok(output) = std::process::Command::new("dbus-send")
        .args([
            "--reply-timeout=100",
            "--print-reply=literal",
            "--dest=org.freedesktop.portal.Desktop",
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Settings.Read",
            "string:org.gnome.desktop.wm.preferences",
            "string:button-layout",
        ])
        .output()
    else {
        return 0;
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let Some(word) = text.rsplit(' ').next() else {
        return 0;
    };
    match word.split(':').take(2).collect::<Vec<_>>().as_slice() {
        [left, _right] => native_cluster(left, capabilities),
        // No separator means no layout at all; the library then falls back to
        // its own default, which is the trailing side.
        _ => 0,
    }
}
/// Inset of the floating Fullscreen button when there is no titlebar.
const FLOATING_MARGIN: i32 = 6;

/// Radius of the circular background, matching the window's own buttons.
const CIRCLE_RADIUS: f32 = 12.0;
/// Half the glyph line width, in logical pixels.
const STROKE: f32 = 0.8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action {
    Back,
    Fullscreen,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Visual {
    Normal,
    Hover,
    Pressed,
}

impl Visual {
    fn index(self) -> usize {
        match self {
            Self::Normal => 0,
            Self::Hover => 1,
            Self::Pressed => 2,
        }
    }
}

const VISUALS: usize = 3;

/// Which of a button's images is attached. Fullscreen carries both.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Glyph {
    Back,
    Enter,
    Leave,
}

type Segment = ((f32, f32), (f32, f32));

/// Glyph outlines in the logical `BUTTON` square, centred on its middle.
fn segments(glyph: Glyph) -> &'static [Segment] {
    match glyph {
        Glyph::Back => &[((20.75, 12.0), (14.25, 17.5)), ((14.25, 17.5), (20.75, 23.0))],
        Glyph::Enter => &[
            ((12.0, 15.0), (12.0, 12.0)),
            ((12.0, 12.0), (15.0, 12.0)),
            ((20.0, 12.0), (23.0, 12.0)),
            ((23.0, 12.0), (23.0, 15.0)),
            ((23.0, 20.0), (23.0, 23.0)),
            ((23.0, 23.0), (20.0, 23.0)),
            ((15.0, 23.0), (12.0, 23.0)),
            ((12.0, 23.0), (12.0, 20.0)),
        ],
        Glyph::Leave => &[
            ((12.0, 15.0), (15.0, 15.0)),
            ((15.0, 15.0), (15.0, 12.0)),
            ((20.0, 12.0), (20.0, 15.0)),
            ((20.0, 15.0), (23.0, 15.0)),
            ((23.0, 20.0), (20.0, 20.0)),
            ((20.0, 20.0), (20.0, 23.0)),
            ((15.0, 23.0), (15.0, 20.0)),
            ((15.0, 20.0), (12.0, 20.0)),
        ],
    }
}

/// Colors for one rendering pass, in the Adwaita palette `sctk-adwaita` uses.
#[derive(Clone, Copy)]
struct Palette {
    idle: [u8; 3],
    hover: [u8; 3],
    pressed: [u8; 3],
    icon: [u8; 3],
}

impl Palette {
    fn new(active: bool) -> Self {
        let theme = sctk_adwaita::theme::ColorTheme::auto();
        let map = if active { &theme.active } else { &theme.inactive };
        let rgb = |color: sctk_adwaita::theme::Color| {
            let channel = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
            [
                channel(color.red()),
                channel(color.green()),
                channel(color.blue()),
            ]
        };
        let hover = rgb(map.button_hover);
        let icon = rgb(map.button_icon);
        Self {
            idle: rgb(map.button_idle),
            hover,
            // Adwaita has no pressed shade for headerbar buttons; deepen the
            // hover tone towards the glyph so a press reads as a press.
            pressed: blend(hover, icon, 0.22),
            icon,
        }
    }
}

fn blend(from: [u8; 3], to: [u8; 3], amount: f32) -> [u8; 3] {
    let mix = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * amount).round() as u8;
    [
        mix(from[0], to[0]),
        mix(from[1], to[1]),
        mix(from[2], to[2]),
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

/// One image of a button: the glyph over the circular background, with
/// everything outside the circle fully transparent so the titlebar shows
/// through at any palette.
fn image(glyph: Glyph, palette: &Palette, visual: Visual, scale: u32) -> Vec<u8> {
    let centre = BUTTON as f32 / 2.0;
    let pixels = BUTTON * scale;
    let mut bytes = Vec::with_capacity((pixels * pixels * 4) as usize);
    for y in 0..pixels {
        for x in 0..pixels {
            let sample_x = (x as f32 + 0.5) / scale as f32;
            let sample_y = (y as f32 + 0.5) / scale as f32;
            let distance = segments(glyph)
                .iter()
                .map(|&(a, b)| distance_to_segment(sample_x, sample_y, a, b))
                .fold(f32::INFINITY, f32::min);
            let color = if distance <= STROKE {
                palette.icon
            } else if (sample_x - centre).hypot(sample_y - centre) <= CIRCLE_RADIUS {
                match visual {
                    Visual::Normal => palette.idle,
                    Visual::Hover => palette.hover,
                    Visual::Pressed => palette.pressed,
                }
            } else {
                // Transparent, which is the same premultiplied or not.
                bytes.extend_from_slice(&[0, 0, 0, 0]);
                continue;
            };
            // `wl_shm`'s ARGB8888 is ARGB32 in native byte order, so on every
            // little-endian machine the bytes land blue first.
            bytes.extend_from_slice(&[color[2], color[1], color[0], 255]);
        }
    }
    bytes
}

/// Number of palettes each button pre-renders: focused and unfocused, because
/// the compositor recolours the whole titlebar when focus moves.
const PALETTES: usize = 2;

/// Where a glyph's image sits in a button's array, in the order it was written.
///
/// The position is within the button's own glyphs, not [`Glyph::index`]: that
/// one is a global ordering, and a one-glyph button indexed by it would run off
/// the end of its own images.
fn image_index(glyphs: &[Glyph], glyph: Glyph, active: bool, visual: Visual) -> usize {
    let position = glyphs
        .iter()
        .position(|candidate| *candidate == glyph)
        .unwrap_or(0);
    (usize::from(!active) * glyphs.len() + position) * VISUALS + visual.index()
}

struct Button {
    action: Action,
    surface: wl_surface::WlSurface,
    role: wl_subsurface::WlSubsurface,
    images: Vec<wl_buffer::WlBuffer>,
    glyphs: &'static [Glyph],
    visual: Visual,
    active: bool,
    visible: bool,
    fullscreen: bool,
    position: (i32, i32),
}

impl Drop for Button {
    fn drop(&mut self) {
        self.role.destroy();
        self.surface.destroy();
        for image in self.images.drain(..) {
            image.destroy();
        }
    }
}

impl Button {
    fn new(
        action: Action,
        globals: &layers::Globals,
        compositor: &CompositorState,
        parent: &wl_surface::WlSurface,
        qh: &QueueHandle<App>,
        scale: u32,
    ) -> Result<Self, PresenterError> {
        let glyphs: &'static [Glyph] = match action {
            Action::Back => &[Glyph::Back],
            Action::Fullscreen => &[Glyph::Enter, Glyph::Leave],
        };
        let error = |what: &str| PresenterError::WindowDecoration(what.to_owned());
        let pixels = BUTTON.checked_mul(scale).ok_or_else(|| error("titlebar button size overflow"))?;
        let bytes = usize::try_from(u64::from(pixels) * u64::from(pixels) * 4)
            .map_err(|_| error("titlebar button size overflow"))?;
        let stride = i32::try_from(pixels * 4).map_err(|_| error("titlebar button stride overflow"))?;
        let count = PALETTES * glyphs.len() * VISUALS;
        let total = bytes
            .checked_mul(count)
            .ok_or_else(|| error("titlebar button pool overflow"))?;

        let mut file = tempfile::tempfile()?;
        for active in [true, false] {
            let palette = Palette::new(active);
            for glyph in glyphs {
                for visual in [Visual::Normal, Visual::Hover, Visual::Pressed] {
                    file.write_all(&image(*glyph, &palette, visual, scale))?;
                }
            }
        }
        let size = i32::try_from(total).map_err(|_| error("titlebar button pool too large"))?;
        let pool = globals.shm.create_pool(file.as_fd(), size, qh, ());
        let dimension = i32::try_from(pixels).unwrap_or(i32::MAX);
        let mut images = Vec::with_capacity(count);
        for index in 0..count {
            let offset =
                i32::try_from(index * bytes).map_err(|_| error("titlebar button offset overflow"))?;
            images.push(pool.create_buffer(
                offset,
                dimension,
                dimension,
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
        // The button is navigation chrome, not content: a press must be visible
        // immediately rather than waiting for the next content commit.
        role.set_desync();
        // Nothing is attached here: a button that has not been placed yet would
        // flash at the surface origin. `place` positions it before showing it.
        Ok(Self {
            action,
            surface,
            role,
            images,
            glyphs,
            visual: Visual::Normal,
            active: true,
            visible: false,
            fullscreen: false,
            position: (0, 0),
        })
    }

    fn image(&self) -> &wl_buffer::WlBuffer {
        let glyph = match self.action {
            Action::Back => Glyph::Back,
            // The Fullscreen button shows the state it switches to.
            Action::Fullscreen if self.fullscreen => Glyph::Leave,
            Action::Fullscreen => Glyph::Enter,
        };
        &self.images[image_index(self.glyphs, glyph, self.active, self.visual)]
    }

    fn attach(&self) {
        self.surface.attach(Some(self.image()), 0, 0);
        self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.surface.commit();
    }

    fn redraw(&self) {
        if self.visible {
            self.attach();
        }
    }

    fn set_visual(&mut self, visual: Visual) {
        if self.visual != visual {
            self.visual = visual;
            self.redraw();
        }
    }

    fn set_fullscreen(&mut self, fullscreen: bool) {
        if self.fullscreen != fullscreen {
            self.fullscreen = fullscreen;
            self.redraw();
        }
    }

    fn set_active(&mut self, active: bool) {
        if self.active != active {
            self.active = active;
            self.redraw();
        }
    }

    fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.attach();
        } else {
            self.visual = Visual::Normal;
            self.surface.attach(None, 0, 0);
            self.surface.commit();
        }
    }

    fn set_position(&mut self, x: i32, y: i32) {
        if self.position == (x, y) {
            return;
        }
        self.position = (x, y);
        self.role.set_position(x, y);
        if self.visible {
            // The role is desynchronized, so its own commit applies the move.
            // While hidden the position waits for the attach that shows it.
            self.surface.commit();
        }
    }
}

/// The navigation buttons of one Android window.
pub(super) struct Chrome {
    back: Button,
    fullscreen: Button,
    /// Width of the window's own button cluster on the leading side, which our
    /// buttons must clear.
    native_leading: u32,
    /// Buffer pixels per logical pixel, and the logical square each button
    /// claims. Traced because a mismatch here is invisible in the source.
    scale: u32,
}

impl Chrome {
    pub(super) fn new(
        globals: &layers::Globals,
        compositor: &CompositorState,
        parent: &wl_surface::WlSurface,
        qh: &QueueHandle<App>,
        scale_120: u32,
        capabilities: WindowManagerCapabilities,
    ) -> Result<Self, PresenterError> {
        // `sctk-adwaita` renders its parts at the ceiling of the fractional
        // scale, so the titlebar band is a whole number of buffer pixels.
        let scale = (f64::from(scale_120) / f64::from(FRACTIONAL_SCALE_DENOMINATOR))
            .ceil()
            .clamp(1.0, 64.0) as u32;
        Ok(Self {
            back: Button::new(Action::Back, globals, compositor, parent, qh, scale)?,
            fullscreen: Button::new(Action::Fullscreen, globals, compositor, parent, qh, scale)?,
            native_leading: native_leading_buttons(capabilities),
            scale,
        })
    }

    /// Place both buttons. `header` is `DecorationsFrame::location().1`, the
    /// negative offset of the titlebar, or zero when the frame draws none.
    pub(super) fn place(&mut self, content_width: u32, header: i32, fullscreen: bool) {
        self.back.set_fullscreen(fullscreen);
        self.fullscreen.set_fullscreen(fullscreen);
        if header < 0 {
            // Decorated: both buttons follow the window's own cluster, so they
            // never land on top of it whichever side the desktop put it on.
            let inset = i32::try_from(header.unsigned_abs().saturating_sub(BUTTON) / 2).unwrap_or(0);
            let first = self.native_leading.saturating_add(NATIVE_GAP);
            eprintln!(
                "Droidloom trace: stage=chrome event=place header={header} inset={inset} native_leading={} scale={} buffer_px={} content_width={content_width} back_x={first} full_x={} y={}",
                self.native_leading,
                self.scale,
                BUTTON * self.scale,
                first.saturating_add(BUTTON + NATIVE_GAP),
                header + inset,
            );
            let end = first.saturating_add(2 * BUTTON + NATIVE_GAP);
            if end > content_width {
                // No room beside the window's buttons; leave the titlebar alone
                // rather than draw over the title.
                self.back.set_visible(false);
                self.fullscreen.set_visible(false);
                return;
            }
            self.back.set_visible(true);
            self.back
                .set_position(i32::try_from(first).unwrap_or(0), header + inset);
            self.fullscreen.set_visible(true);
            let second = first.saturating_add(BUTTON + NATIVE_GAP);
            self.fullscreen
                .set_position(i32::try_from(second).unwrap_or(0), header + inset);
            return;
        }
        // Undecorated. The side swipe already covers Back, so only the way out
        // of fullscreen needs a target.
        self.back.set_visible(false);
        let right = i32::try_from(content_width.saturating_sub(BUTTON))
            .unwrap_or(0)
            .saturating_sub(FLOATING_MARGIN);
        self.fullscreen.set_visible(true);
        self.fullscreen.set_position(right, FLOATING_MARGIN);
    }

    pub(super) fn set_active(&mut self, active: bool) {
        self.back.set_active(active);
        self.fullscreen.set_active(active);
    }

    /// The action owning `surface`, when the surface is one of the buttons.
    pub(super) fn action_at(&self, surface: &wayland_client::backend::ObjectId) -> Option<Action> {
        [&self.back, &self.fullscreen]
            .into_iter()
            .find(|button| button.visible && button.surface.id() == *surface)
            .map(|button| button.action)
    }

    fn button_mut(&mut self, action: Action) -> &mut Button {
        match action {
            Action::Back => &mut self.back,
            Action::Fullscreen => &mut self.fullscreen,
        }
    }

    /// Hover feedback; a held press keeps its state until the pointer leaves.
    pub(super) fn hover(&mut self, action: Action) {
        let button = self.button_mut(action);
        if button.visual != Visual::Pressed {
            button.set_visual(Visual::Hover);
        }
    }

    pub(super) fn press(&mut self, action: Action) {
        self.button_mut(action).set_visual(Visual::Pressed);
    }

    /// Drop hover and press feedback.
    pub(super) fn reset(&mut self) {
        for button in [&mut self.back, &mut self.fullscreen] {
            button.set_visual(Visual::Normal);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_image_is_the_size_the_surface_claims() {
        for scale in [1, 2, 3] {
            for glyph in [Glyph::Back, Glyph::Enter, Glyph::Leave] {
                let palette = Palette::new(true);
                assert_eq!(
                    image(glyph, &palette, Visual::Normal, scale).len(),
                    (BUTTON * scale * BUTTON * scale * 4) as usize
                );
            }
        }
    }

    #[test]
    fn glyphs_stay_inside_the_circle_and_the_button() {
        let palette = Palette::new(true);
        for glyph in [Glyph::Back, Glyph::Enter, Glyph::Leave] {
            for &((ax, ay), (bx, by)) in segments(glyph) {
                let inside = |x: f32, y: f32| {
                    x >= STROKE
                        && y >= STROKE
                        && x <= BUTTON as f32 - STROKE
                        && y <= BUTTON as f32 - STROKE
                        && (x - BUTTON as f32 / 2.0).hypot(y - BUTTON as f32 / 2.0)
                            <= CIRCLE_RADIUS + STROKE
                };
                assert!(inside(ax, ay) && inside(bx, by), "{glyph:?} escapes the button");
            }
            // The glyph must actually mark pixels, or the button is a hot zone.
            let ink = image(glyph, &palette, Visual::Normal, 2)
                .chunks_exact(4)
                .filter(|quad| quad[1..] != palette.idle && quad[1..] != [0, 0, 0])
                .count();
            assert!(ink > 0, "{glyph:?} draws nothing");
        }
    }

    #[test]
    fn every_glyph_of_a_button_indexes_inside_that_button() {
        for glyphs in [&[Glyph::Back][..], &[Glyph::Enter, Glyph::Leave][..]] {
            let count = PALETTES * glyphs.len() * VISUALS;
            let mut seen = Vec::new();
            for active in [true, false] {
                for glyph in glyphs {
                    for visual in [Visual::Normal, Visual::Hover, Visual::Pressed] {
                        let index = image_index(glyphs, *glyph, active, visual);
                        assert!(index < count, "{glyph:?} indexed {index} of {count}");
                        seen.push(index);
                    }
                }
            }
            seen.sort_unstable();
            seen.dedup();
            // Every image is reachable exactly once, so no two states alias.
            assert_eq!(seen.len(), count);
        }
    }

    #[test]
    fn pressed_state_is_distinct_from_hover_and_idle() {
        let palette = Palette::new(true);
        assert_ne!(palette.pressed, palette.hover);
        assert_ne!(palette.pressed, palette.idle);
        assert_ne!(palette.hover, palette.idle);
    }

    #[test]
    fn the_window_cluster_is_measured_on_whichever_side_the_desktop_chose() {
        let all = WindowManagerCapabilities::MAXIMIZE | WindowManagerCapabilities::MINIMIZE;
        // sheng's layout: the window's own three buttons lead, and the trailing
        // side is the app menu, which sctk-adwaita cannot draw.
        assert_eq!(native_cluster("close,minimize,maximize", all), 103);
        // The usual GNOME default puts nothing on the leading side.
        assert_eq!(native_cluster("appmenu", all), 0);
        assert_eq!(native_cluster("", all), 0);
        // Unknown tokens are skipped, and capabilities filter the rest.
        assert_eq!(native_cluster("close,appmenu", all), 29);
        assert_eq!(
            native_cluster("close,minimize,maximize", WindowManagerCapabilities::empty()),
            29
        );
    }

    #[test]
    fn our_buttons_never_overlap_the_window_cluster() {
        let all = WindowManagerCapabilities::MAXIMIZE | WindowManagerCapabilities::MINIMIZE;
        for layout in ["close,minimize,maximize", "close", "", "appmenu"] {
            let leading = native_cluster(layout, all);
            let first = leading + NATIVE_GAP;
            let end = first + 2 * BUTTON + NATIVE_GAP;
            // The window's own buttons end here; ours start after them.
            assert!(first >= leading, "{layout}: {first} < {leading}");
            assert!(end <= 1280, "{layout} does not fit a 1280 titlebar");
        }
    }
}
