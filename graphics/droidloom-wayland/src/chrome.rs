//! Visible per-window navigation.
//!
//! Droidloom presents one Android task per window. A phone application has no
//! keyboard to press `F11` with, and `wayland-csd-frame`'s `FrameAction` has no
//! Fullscreen variant, so Back and Fullscreen are separate subsurfaces stacked
//! over the frame's own titlebar. They are always drawn: a hit zone with no
//! image is not a control.
//!
//! The two buttons are also the only way out of fullscreen on a device with no
//! keyboard, so once the titlebar is gone they stay visible as a centred pair
//! floating over the content at the top.

use super::*;
use std::io::Write;
use wayland_client::protocol::{wl_shm, wl_subsurface};

/// One button's outer square, in logical pixels: the window cluster's 24px
/// button plus the 2px its disc leaves on each side.
const BUTTON: u32 = 28;
/// Origin-to-origin distance between our two buttons: `BUTTON` plus a 9px gap,
/// so their period stays the window cluster's 24 + 13.
const PERIOD: u32 = 37;
/// `sctk-adwaita` 0.11.0's private header metrics (see its `buttons.rs`). The
/// crate is pinned to an exact version, so the window's own Close / Maximize /
/// Minimize cluster has a count this code can reserve against.
const NATIVE_SIZE: u32 = 24;
const NATIVE_MARGIN: u32 = 5;
const NATIVE_SPACING: u32 = 13;
/// The window cluster starts one pixel past its margin, after the headerbar's
/// visible border, and our square carries two pixels of padding before its
/// disc: both are the old geometry's `4 + n * 37`.
const NATIVE_BORDER: i32 = 1;
const BUTTON_PADDING: i32 = 2;
/// Distance from the top of the titlebar band down to our square. It puts the
/// disc's centre on the window buttons' centre line, `margin + size / 2`.
const TITLEBAR_INSET: i32 = 3;
/// Half the gap between the floating pair, measured from the window's centre
/// line, and how far the pair sits below the top edge. The pair is centred
/// rather than tucked into a corner: fullscreen content reaches every edge.
const FLOATING_GAP: i32 = 5;
const FLOATING_Y: i32 = 10;

/// Radius of the circular background, matching the window's own buttons.
const CIRCLE_RADIUS: f32 = 12.0;
/// Radius and half-width of the hairline the floating pair draws on its rim.
const RING_RADIUS: f32 = 11.5;
const RING_WIDTH: f32 = 0.75;
const RING_ALPHA: f32 = 0.28;

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

/// How many buttons `sctk-adwaita` draws on one side of its titlebar.
///
/// The frame library does not publish this, so it is recomputed with the same
/// private metrics `native_cluster_end` uses, from the same settings-portal
/// key the library reads. Getting this wrong is not cosmetic — the sheng
/// desktop puts the window's Close, Minimize and Maximize on the *left*, and a
/// chrome that assumed the usual right-hand cluster sat on top of them.
fn native_cluster(config: &str, capabilities: WindowManagerCapabilities) -> u32 {
    config
        .split(',')
        .take(3)
        .filter(|token| match token.trim() {
            "close" => true,
            "maximize" => capabilities.contains(WindowManagerCapabilities::MAXIMIZE),
            "minimize" => capabilities.contains(WindowManagerCapabilities::MINIMIZE),
            _ => false,
        })
        .count() as u32
}

/// Where the window's own cluster ends, measured from the leading edge.
fn native_cluster_end(count: u32) -> i32 {
    match count {
        0 => 0,
        // The margin, then `count` buttons with the library's spacing between
        // them.
        n => (NATIVE_MARGIN + n * NATIVE_SIZE + (n - 1) * NATIVE_SPACING) as i32,
    }
}

/// The leading-side origin of our first button, with `count` buttons in the
/// window's own cluster. The window's slots start at the headerbar's visible
/// border, and our square's padding carries it back to the disc.
fn leading_x(count: u32) -> i32 {
    NATIVE_MARGIN as i32 + NATIVE_BORDER + (count * PERIOD) as i32 - BUTTON_PADDING
}

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

/// Whether the button sits in a titlebar or floats over the content. The two
/// need different paint: there is no headerbar behind the floating pair to
/// match, and an opaque light disc there would sit on the application's own
/// pixels as a blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backing {
    Header,
    Floating,
}

type Segment = ((f32, f32), (f32, f32));

/// Glyph outlines in the logical `BUTTON` square, centred on its middle.
fn segments(glyph: Glyph) -> &'static [Segment] {
    match glyph {
        Glyph::Back => &[((16.0, 9.0), (11.0, 14.0)), ((11.0, 14.0), (16.0, 19.0))],
        Glyph::Enter => &[
            ((14.5, 9.5), (18.5, 9.5)),
            ((18.5, 9.5), (18.5, 13.5)),
            ((18.5, 9.5), (14.0, 14.0)),
            ((13.5, 18.5), (9.5, 18.5)),
            ((9.5, 18.5), (9.5, 14.5)),
            ((9.5, 18.5), (14.0, 14.0)),
        ],
        Glyph::Leave => &[
            ((18.0, 13.5), (14.5, 13.5)),
            ((14.5, 13.5), (14.5, 10.0)),
            ((14.5, 13.5), (18.5, 9.5)),
            ((10.0, 14.5), (13.5, 14.5)),
            ((13.5, 14.5), (13.5, 18.0)),
            ((13.5, 14.5), (9.5, 18.5)),
        ],
    }
}

/// Half the glyph's line width. The arrows are lighter than the fullscreen
/// marks, which carry more segments in the same square.
fn stroke(glyph: Glyph) -> f32 {
    match glyph {
        Glyph::Back => 1.35,
        Glyph::Enter | Glyph::Leave => 1.15,
    }
}

/// The straight colours one button's images are painted from.
struct Skin {
    /// The disc behind the glyph, per visual state: red, green, blue, alpha.
    disc: [[f32; 4]; VISUALS],
    ink: [f32; 3],
    /// Hairline drawn on the disc's rim, which only the floating pair needs to
    /// stay legible over an unknown picture.
    ring: Option<[f32; 3]>,
}

impl Skin {
    fn new(backing: Backing, active: bool) -> Self {
        let channel = |value: f32| value.clamp(0.0, 1.0);
        match backing {
            Backing::Header => {
                let theme = sctk_adwaita::theme::ColorTheme::auto();
                let map = if active { &theme.active } else { &theme.inactive };
                let rgb = |color: sctk_adwaita::theme::Color| {
                    [
                        channel(color.red()),
                        channel(color.green()),
                        channel(color.blue()),
                    ]
                };
                let hover = rgb(map.button_hover);
                let icon = rgb(map.button_icon);
                let opaque = |rgb: [f32; 3]| [rgb[0], rgb[1], rgb[2], 1.0];
                Self {
                    disc: [
                        opaque(rgb(map.button_idle)),
                        opaque(hover),
                        // Adwaita has no pressed shade for headerbar buttons;
                        // deepen the hover tone towards the glyph so a press
                        // reads as a press.
                        opaque(blend(hover, icon, 0.22)),
                    ],
                    ink: icon,
                    ring: None,
                }
            }
            // Focus does not reach a button with no titlebar around it, so both
            // focus palettes are the same disc.
            Backing::Floating => Self {
                disc: [
                    [0.15, 0.16, 0.18, 0.70],
                    [0.25, 0.27, 0.30, 0.85],
                    [0.35, 0.38, 0.42, 0.95],
                ],
                ink: [0.95, 0.95, 0.95],
                ring: Some([0.9, 0.9, 0.9]),
            },
        }
    }
}

fn blend(from: [f32; 3], to: [f32; 3], amount: f32) -> [f32; 3] {
    let mix = |a: f32, b: f32| a + (b - a) * amount;
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

/// A straight colour at an alpha, in premultiplied form.
fn paint(rgb: [f32; 3], alpha: f32) -> [f32; 4] {
    [rgb[0] * alpha, rgb[1] * alpha, rgb[2] * alpha, alpha]
}

/// Source-over in premultiplied form.
fn over(below: [f32; 4], above: [f32; 4]) -> [f32; 4] {
    let mix = |index: usize| above[index] + below[index] * (1.0 - above[3]);
    [
        mix(0),
        mix(1),
        mix(2),
        above[3] + below[3] * (1.0 - above[3]),
    ]
}

/// One image of a button: the glyph over the disc, with everything outside the
/// disc fully transparent so the titlebar — or the content under a floating
/// button — shows through at any palette.
fn image(glyph: Glyph, skin: &Skin, visual: Visual, scale: u32) -> Vec<u8> {
    let centre = BUTTON as f32 / 2.0;
    let stroke = stroke(glyph);
    let disc = skin.disc[visual.index()];
    let pixels = BUTTON * scale;
    let mut bytes = Vec::with_capacity((pixels * pixels * 4) as usize);
    for y in 0..pixels {
        for x in 0..pixels {
            let sample_x = (x as f32 + 0.5) / scale as f32;
            let sample_y = (y as f32 + 0.5) / scale as f32;
            let rim = (sample_x - centre).hypot(sample_y - centre);
            // Coverage rather than a cutoff: the disc's rim and the glyph's
            // diagonals are the only edges here, and a hard one crawls as the
            // circle turns.
            let coverage = (CIRCLE_RADIUS + 0.5 - rim).clamp(0.0, 1.0);
            let mut pixel = paint([disc[0], disc[1], disc[2]], disc[3] * coverage);
            if let Some(ring) = skin.ring {
                let band = (RING_WIDTH - (rim - RING_RADIUS).abs()).clamp(0.0, 1.0);
                pixel = over(pixel, paint(ring, RING_ALPHA * band * coverage));
            }
            let glyph_distance = segments(glyph)
                .iter()
                .map(|&(a, b)| distance_to_segment(sample_x, sample_y, a, b))
                .fold(f32::INFINITY, f32::min);
            // The glyph never paints outside the disc, so the two coverages
            // compound rather than one clipping the other.
            let ink = (stroke + 0.5 - glyph_distance).clamp(0.0, 1.0) * coverage;
            let pixel = over(pixel, paint(skin.ink, ink));
            // `wl_shm`'s ARGB8888 is ARGB32 in native byte order, so on every
            // little-endian machine the bytes land blue first.
            let byte = |value: f32| (value.clamp(0.0, 1.0) * 255.0).round() as u8;
            bytes.extend_from_slice(&[
                byte(pixel[2]),
                byte(pixel[1]),
                byte(pixel[0]),
                byte(pixel[3]),
            ]);
        }
    }
    bytes
}

/// Number of focus palettes each button pre-renders: focused and unfocused,
/// because the compositor recolours the whole titlebar when focus moves.
const PALETTES: usize = 2;

/// Where a glyph's image sits in a button's array, in the order it was written.
///
/// The position is within the button's own glyphs, not [`Glyph`]'s order: that
/// one is shared, and a one-glyph button indexed by it would run off the end of
/// its own images.
fn image_index(
    glyphs: &[Glyph],
    glyph: Glyph,
    backing: Backing,
    active: bool,
    visual: Visual,
) -> usize {
    let position = glyphs
        .iter()
        .position(|candidate| *candidate == glyph)
        .unwrap_or(0);
    let floating = usize::from(backing == Backing::Floating);
    ((floating * PALETTES + usize::from(!active)) * glyphs.len() + position) * VISUALS
        + visual.index()
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
    floating: bool,
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
        let count = 2 * PALETTES * glyphs.len() * VISUALS;
        let total = bytes
            .checked_mul(count)
            .ok_or_else(|| error("titlebar button pool overflow"))?;

        let mut file = tempfile::tempfile()?;
        for backing in [Backing::Header, Backing::Floating] {
            for active in [true, false] {
                let skin = Skin::new(backing, active);
                for glyph in glyphs {
                    for visual in [Visual::Normal, Visual::Hover, Visual::Pressed] {
                        file.write_all(&image(*glyph, &skin, visual, scale))?;
                    }
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
            floating: false,
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
        let backing = if self.floating {
            Backing::Floating
        } else {
            Backing::Header
        };
        &self.images[image_index(self.glyphs, glyph, backing, self.active, self.visual)]
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

    fn set_state(&mut self, fullscreen: bool, floating: bool) {
        if self.fullscreen != fullscreen || self.floating != floating {
            self.fullscreen = fullscreen;
            self.floating = floating;
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
    /// How many buttons the window's own cluster put on the leading side, which
    /// ours must clear.
    leading_buttons: u32,
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
            leading_buttons: native_leading_buttons(capabilities),
            scale,
        })
    }

    /// Place both buttons. `header` is `DecorationsFrame::location().1`, the
    /// negative offset of the titlebar, or zero when the frame draws none.
    pub(super) fn place(&mut self, content_width: u32, header: i32, fullscreen: bool) {
        let floating = header >= 0;
        self.back.set_state(fullscreen, floating);
        self.fullscreen.set_state(fullscreen, floating);
        let (back_x, fullscreen_x, y) = if floating {
            // No titlebar: the pair floats centred at the top, clear of the
            // side-swipe edge and of whatever the application draws in a
            // corner.
            let centre = i32::try_from(content_width / 2).unwrap_or(i32::MAX);
            (
                centre - BUTTON as i32 - FLOATING_GAP,
                centre + FLOATING_GAP,
                FLOATING_Y,
            )
        } else {
            // Decorated: our two buttons take the slots after the window's own
            // cluster, so they never land on top of it whichever side the
            // desktop put it on.
            let first = leading_x(self.leading_buttons);
            (first, first + PERIOD as i32, header + TITLEBAR_INSET)
        };
        let end = fullscreen_x.saturating_add(BUTTON as i32);
        let content_width = i32::try_from(content_width).unwrap_or(i32::MAX);
        eprintln!(
            "Droidloom trace: stage=chrome event=place header={header} floating={floating} fullscreen={fullscreen} y={y} disc_centre_y={} back_x={back_x} full_x={fullscreen_x} leading_buttons={} cluster_end={} scale={} buffer_px={} content_width={content_width}",
            y + (BUTTON / 2) as i32,
            self.leading_buttons,
            native_cluster_end(self.leading_buttons),
            self.scale,
            BUTTON * self.scale,
        );
        // A pair that runs off the window is worse than none: on a device with
        // no keyboard the frame's own Close and the keyboard's `F11` remain.
        let fits = back_x >= 0 && end <= content_width;
        for (button, x) in [(&mut self.back, back_x), (&mut self.fullscreen, fullscreen_x)] {
            if fits {
                // Position before showing: a desynchronized subsurface moves on
                // its next commit, and the attach that shows it is that commit.
                button.set_position(x, y);
            }
            button.set_visible(fits);
        }
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

    /// `sctk-adwaita`'s `HEADER_SIZE`, which is what the frame reports back
    /// through `DecorationsFrame::location().1`.
    const TITLEBAR_BAND: i32 = 35;

    #[test]
    fn every_image_is_the_size_the_surface_claims() {
        for scale in [1, 2, 3] {
            for backing in [Backing::Header, Backing::Floating] {
                for glyph in [Glyph::Back, Glyph::Enter, Glyph::Leave] {
                    let skin = Skin::new(backing, true);
                    assert_eq!(
                        image(glyph, &skin, Visual::Normal, scale).len(),
                        (BUTTON * scale * BUTTON * scale * 4) as usize
                    );
                }
            }
        }
    }

    #[test]
    fn glyphs_stay_inside_the_disc_and_the_button() {
        for glyph in [Glyph::Back, Glyph::Enter, Glyph::Leave] {
            let stroke = stroke(glyph);
            for &((ax, ay), (bx, by)) in segments(glyph) {
                let inside = |x: f32, y: f32| {
                    x >= stroke
                        && y >= stroke
                        && x <= BUTTON as f32 - stroke
                        && y <= BUTTON as f32 - stroke
                        && (x - BUTTON as f32 / 2.0).hypot(y - BUTTON as f32 / 2.0)
                            <= CIRCLE_RADIUS + stroke
                };
                assert!(inside(ax, ay) && inside(bx, by), "{glyph:?} escapes the button");
            }
            // The glyph must actually mark pixels, or the button is a hot zone.
            // Rendering it over an empty disc leaves only the glyph's own ink.
            let bare = Skin {
                disc: [[0.0; 4]; VISUALS],
                ink: [1.0, 1.0, 1.0],
                ring: None,
            };
            let ink = image(glyph, &bare, Visual::Normal, 2)
                .chunks_exact(4)
                .filter(|quad| quad[3] != 0)
                .count();
            assert!(ink > 0, "{glyph:?} draws nothing");
        }
    }

    #[test]
    fn every_glyph_of_a_button_indexes_inside_that_button() {
        for glyphs in [&[Glyph::Back][..], &[Glyph::Enter, Glyph::Leave][..]] {
            let count = 2 * PALETTES * glyphs.len() * VISUALS;
            let mut seen = Vec::new();
            for backing in [Backing::Header, Backing::Floating] {
                for active in [true, false] {
                    for glyph in glyphs {
                        for visual in [Visual::Normal, Visual::Hover, Visual::Pressed] {
                            let index = image_index(glyphs, *glyph, backing, active, visual);
                            assert!(index < count, "{glyph:?} indexed {index} of {count}");
                            seen.push(index);
                        }
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
        let skin = Skin::new(Backing::Header, true);
        assert_ne!(skin.disc[Visual::Pressed.index()], skin.disc[Visual::Hover.index()]);
        assert_ne!(skin.disc[Visual::Pressed.index()], skin.disc[Visual::Normal.index()]);
        assert_ne!(skin.disc[Visual::Hover.index()], skin.disc[Visual::Normal.index()]);
    }

    #[test]
    fn the_floating_disc_is_translucent_and_ringed() {
        let skin = Skin::new(Backing::Floating, true);
        // A floating pair sits on the application's pixels, so its disc has to
        // let them through at every state, and it carries the rim hairline.
        for visual in [Visual::Normal, Visual::Hover, Visual::Pressed] {
            let disc = skin.disc[visual.index()];
            assert!(disc[3] < 1.0, "{visual:?} floats opaque");
        }
        assert!(skin.ring.is_some());
        assert!(Skin::new(Backing::Header, true).ring.is_none());
        assert_eq!(Skin::new(Backing::Floating, true).disc, Skin::new(Backing::Floating, false).disc);
    }

    #[test]
    fn the_window_cluster_is_counted_on_whichever_side_the_desktop_chose() {
        let all = WindowManagerCapabilities::MAXIMIZE | WindowManagerCapabilities::MINIMIZE;
        // sheng's layout: the window's own three buttons lead, and the trailing
        // side is the app menu, which sctk-adwaita cannot draw.
        assert_eq!(native_cluster("close,minimize,maximize", all), 3);
        assert_eq!(native_cluster_end(3), 103);
        // The usual GNOME default puts nothing on the leading side.
        assert_eq!(native_cluster("appmenu", all), 0);
        assert_eq!(native_cluster_end(0), 0);
        assert_eq!(native_cluster("", all), 0);
        // Unknown tokens are skipped, and capabilities filter the rest.
        assert_eq!(native_cluster("close,appmenu", all), 1);
        assert_eq!(native_cluster_end(1), 29);
        assert_eq!(
            native_cluster("close,minimize,maximize", WindowManagerCapabilities::empty()),
            1
        );
    }

    #[test]
    fn our_buttons_never_overlap_the_window_cluster() {
        let all = WindowManagerCapabilities::MAXIMIZE | WindowManagerCapabilities::MINIMIZE;
        for layout in ["close,minimize,maximize", "close", "", "appmenu"] {
            let leading = native_cluster(layout, all);
            let first = leading_x(leading);
            let end = first + 2 * BUTTON as i32 + (PERIOD - BUTTON) as i32;
            // The window's own disc ends before our first square starts.
            assert!(first >= native_cluster_end(leading), "{layout}: {first} overlaps");
            assert!(end <= 1280, "{layout} does not fit a 1280 titlebar");
        }
    }

    #[test]
    fn the_titlebar_inset_puts_our_disc_on_the_window_buttons_centre_line() {
        // Both discs are centred in the same 35px band: the window's 24px
        // button inset by its margin, ours `BUTTON` tall inset by `TITLEBAR_INSET`.
        assert_eq!(
            TITLEBAR_INSET + (BUTTON / 2) as i32,
            NATIVE_MARGIN as i32 + (NATIVE_SIZE / 2) as i32
        );
        let header = -TITLEBAR_BAND;
        assert_eq!(
            header + TITLEBAR_INSET + (BUTTON / 2) as i32,
            header + NATIVE_MARGIN as i32 + (NATIVE_SIZE / 2) as i32
        );
        assert!(TITLEBAR_INSET + BUTTON as i32 <= TITLEBAR_BAND);
    }

    #[test]
    fn the_floating_pair_is_centred_on_a_ten_pixel_gap() {
        let centre = 1524 / 2;
        let back = centre - BUTTON as i32 - FLOATING_GAP;
        let fullscreen = centre + FLOATING_GAP;
        // The old geometry's `w / 2 - 33` and `w / 2 + 5`.
        assert_eq!(back, centre - 33);
        assert_eq!(fullscreen, centre + 5);
        // The squares and the gap between them straddle the centre line.
        assert_eq!(fullscreen - (back + BUTTON as i32), 2 * FLOATING_GAP);
        assert_eq!(centre - (back + BUTTON as i32), FLOATING_GAP);
    }
}
