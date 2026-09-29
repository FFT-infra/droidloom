//! Ordinary Wayland multi-window presentation for Droidloom.

#![deny(unsafe_op_in_unsafe_fn)]

mod clipboard;
mod activation;
mod layers;
mod fence_wakeup;
mod notifications;
mod insets;
mod presentation_audit;
mod text_input;
mod gesture;

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::num::NonZeroU32;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use drm_fourcc::DrmFourcc;
use droidloom_denial_endpoint::{
    DenialEndpoint, DenialEndpointListener, EndpointAction, EndpointError, EndpointListenerError,
    ExpectedPeer,
};
use droidloom_denial_ipc::IpcError;
use droidloom_denial_protocol::{
    AcceptedWireFrame, BufferId, BufferMetadata, FormatModifier, FrameId, InputEvent, KeyAction,
    MouseAction, PlaneMetadata, TabletAction, TabletToolType, TaskObjectId, TouchAction, Transform,
    Visibility, presentation_flag,
};
use droidloom_syncobj::{SyncobjDevice, WaylandTimeline};
use droidloom_window_policy::{LogicalSize, SessionMode, WindowPolicyPaths, WindowPolicyStore};
use gbm::{BufferObject, Device as GbmDevice, Modifier};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState, Region};
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::presentation_time::{
    PresentTime, PresentationTimeHandler, PresentationTimeState,
};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::seat::keyboard::{
    KeyEvent, KeyboardHandler, Modifiers, RawModifiers, RepeatInfo,
};
use smithay_client_toolkit::seat::pointer::{
    AxisScroll, CursorIcon, PointerData, PointerEvent, PointerEventKind, PointerHandler, ThemeSpec,
    ThemedPointer,
};
use smithay_client_toolkit::seat::pointer_constraints::{
    PointerConstraintsHandler, PointerConstraintsState,
};
use smithay_client_toolkit::seat::touch::TouchHandler;
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::{XdgShell, XdgSurface};
use smithay_client_toolkit::shell::xdg::window::{
    DecorationMode, Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::subcompositor::SubcompositorState;
use smithay_client_toolkit::reexports::csd_frame::{
    DecorationsFrame, FrameAction, FrameClick, ResizeEdge, WindowState,
};
use wayland_protocols::xdg::shell::client::xdg_toplevel::ResizeEdge as XdgResizeEdge;
use sctk_adwaita::{AdwaitaFrame, FrameConfig};
use thiserror::Error;
use wayland_client::globals::{BindError, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_subsurface, wl_surface, wl_touch,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum, backend::WaylandError};
use wayland_protocols::wp::fractional_scale::v1::client::{
    wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1,
    wp_fractional_scale_v1::{self, WpFractionalScaleV1},
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1,
};
use wayland_protocols::wp::linux_drm_syncobj::v1::client::{
    wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1,
    wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1,
    wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1,
};
use wayland_protocols::wp::presentation_time::client::wp_presentation_feedback::{
    self, WpPresentationFeedback,
};
use wayland_protocols::wp::tablet::zv2::client::{
    self as tablet, zwp_tablet_manager_v2::ZwpTabletManagerV2,
    zwp_tablet_pad_dial_v2::ZwpTabletPadDialV2,
    zwp_tablet_pad_group_v2::ZwpTabletPadGroupV2,
    zwp_tablet_pad_ring_v2::ZwpTabletPadRingV2,
    zwp_tablet_pad_strip_v2::ZwpTabletPadStripV2,
    zwp_tablet_pad_v2::ZwpTabletPadV2, zwp_tablet_seat_v2::ZwpTabletSeatV2,
    zwp_tablet_tool_v2::ZwpTabletToolV2, zwp_tablet_v2::ZwpTabletV2,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
    zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
};
use wayland_protocols::wp::pointer_constraints::zv1::client::{
    zwp_confined_pointer_v1::ZwpConfinedPointerV1, zwp_locked_pointer_v1::ZwpLockedPointerV1,
    zwp_pointer_constraints_v1,
};

const BOOTSTRAP_PACKAGE: &str = "android.droidloom.bootstrap";
const TARGET_POOL_LENGTH: usize = 3;
const RELEASE_POLL_FALLBACK: Duration = Duration::from_millis(4);
const LEFT_BUTTON: u32 = 0x110;
/// Linux evdev `BTN_6`: tablet pad button N is routed as `BTN_6 + N`. The
/// bridge maps these to `KEYCODE_BUTTON_7` upwards, the way the pen devices'
/// own key layout does on a tablet that passes them through.
const PAD_BUTTON_BASE: u32 = 0x106;
/// The pen's buttons reach Android as four consecutive generic buttons.
const PAD_BUTTON_COUNT: u32 = 4;
/// Linux evdev key codes of the presenter's own window shortcuts.
/// Set once from the environment to trace routed keys and mouse events.
static INPUT_TRACE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
const KEY_F11: u32 = 87;
const KEY_ESC: u32 = 1;
const KEY_M: u32 = 50;
const KEY_B: u32 = 48;
const KEY_BACK: u32 = 158;
const KEY_LEFTCTRL: u32 = 29;
const KEY_RIGHTCTRL: u32 = 97;
const KEY_LEFTALT: u32 = 56;
const KEY_RIGHTALT: u32 = 100;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTSHIFT: u32 = 54;
const KEY_LEFTMETA: u32 = 125;
const KEY_RIGHTMETA: u32 = 126;
const TITLEBAR_BUTTON_SIZE: u32 = 28;
const DEFAULT_TITLEBAR_HEIGHT: i32 = 36;

fn detect_titlebar_button_y(titlebar_height: i32) -> i32 {
    -(titlebar_height + TITLEBAR_BUTTON_SIZE as i32) / 2
}

fn detect_fullscreen_button_y() -> i32 {
    10
}

fn detect_fullscreen_button_x(kind: TitlebarButtonKind) -> i32 {
    let index = match kind {
        TitlebarButtonKind::Back => 0,
        TitlebarButtonKind::Fullscreen => 1,
    };
    14 + index * 38
}

fn detect_back_button_x() -> i32 {
    static CACHED_X: std::sync::OnceLock<i32> = std::sync::OnceLock::new();
    *CACHED_X.get_or_init(|| {
        let mut left_buttons_count = 0;
        let portal_result = std::process::Command::new("dbus-send")
            .args([
                "--reply-timeout=100",
                "--print-reply=literal",
                "--dest=org.freedesktop.portal.Desktop",
                "/org/freedesktop/portal/desktop",
                "org.freedesktop.portal.Settings.Read",
                "string:org.gnome.desktop.wm.preferences",
                "string:button-layout",
            ])
            .output();

        let raw_layout = if let Ok(out) = portal_result {
            String::from_utf8(out.stdout).ok().and_then(|text| {
                text.split_whitespace().last().map(|s| s.to_owned())
            })
        } else {
            None
        };

        let layout_str = raw_layout.or_else(|| {
            std::process::Command::new("gsettings")
                .args(["get", "org.gnome.desktop.wm.preferences", "button-layout"])
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .map(|s| s.trim().trim_matches('\'').to_owned())
        });

        if let Some(layout) = layout_str {
            if let Some((left, _)) = layout.split_once(':') {
                for button in left.split(',') {
                    match button.trim() {
                        "close" | "minimize" | "maximize" => left_buttons_count += 1,
                        _ => {}
                    }
                }
            }
        }

        if left_buttons_count == 0 {
            4
        } else {
            // Adwaita headerbar: left margin 5, visible border 1 -> first button starts at x=6.
            // Button size 24px, spacing 13px (37px period).
            // N buttons end at 6 + N * 37 - 13.
            // Placing Back button with standard 13px gap starts the next button at 6 + N * 37.
            // Accounting for BackButton's 2px padding (28x28 surface, circle at 2..26),
            // subsurface x is (6 + N * 37) - 2 = 4 + N * 37.
            4 + left_buttons_count * 37
        }
    })
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TitlebarButtonKind {
    Back,
    Fullscreen,
}

fn detect_titlebar_button_x(kind: TitlebarButtonKind) -> i32 {
    let index = match kind {
        TitlebarButtonKind::Back => 0,
        TitlebarButtonKind::Fullscreen => 1,
    };
    detect_back_button_x() + index * 37
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum TitlebarButtonState {
    Normal,
    Hover,
    Pressed,
}

struct TitlebarButton {
    kind: TitlebarButtonKind,
    surface: wl_surface::WlSurface,
    subsurface: wl_subsurface::WlSubsurface,
    state: TitlebarButtonState,
    visible: bool,
    buffer_normal: wl_buffer::WlBuffer,
    buffer_hover: wl_buffer::WlBuffer,
    buffer_pressed: wl_buffer::WlBuffer,
}

impl Drop for TitlebarButton {
    fn drop(&mut self) {
        self.subsurface.destroy();
        self.surface.destroy();
        self.buffer_normal.destroy();
        self.buffer_hover.destroy();
        self.buffer_pressed.destroy();
    }
}

impl TitlebarButton {
    fn new(
        kind: TitlebarButtonKind,
        globals: &layers::Globals,
        compositor: &smithay_client_toolkit::compositor::CompositorState,
        parent: &wl_surface::WlSurface,
        qh: &QueueHandle<App>,
        titlebar_height: i32,
        is_fullscreen: bool,
    ) -> Result<Self, PresenterError> {
        let surface = compositor.create_surface(qh);
        let subsurface = globals.subcompositor.get_subsurface(&surface, parent, qh, ());
        let (x, y) = if is_fullscreen {
            (
                detect_fullscreen_button_x(kind),
                detect_fullscreen_button_y(),
            )
        } else {
            (
                detect_titlebar_button_x(kind),
                detect_titlebar_button_y(titlebar_height),
            )
        };
        subsurface.set_position(x, y);
        subsurface.set_desync();

        let buffer_normal = render_titlebar_button_buffer(globals, qh, kind, TitlebarButtonState::Normal, is_fullscreen)?;
        let buffer_hover = render_titlebar_button_buffer(globals, qh, kind, TitlebarButtonState::Hover, is_fullscreen)?;
        let buffer_pressed = render_titlebar_button_buffer(globals, qh, kind, TitlebarButtonState::Pressed, is_fullscreen)?;

        let visible = !is_fullscreen;
        if visible {
            surface.attach(Some(&buffer_normal), 0, 0);
        } else {
            surface.attach(None, 0, 0);
        }
        surface.commit();

        Ok(Self {
            kind,
            surface,
            subsurface,
            state: TitlebarButtonState::Normal,
            visible,
            buffer_normal,
            buffer_hover,
            buffer_pressed,
        })
    }

    fn set_visible(&mut self, visible: bool) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            let buffer = match self.state {
                TitlebarButtonState::Normal => &self.buffer_normal,
                TitlebarButtonState::Hover => &self.buffer_hover,
                TitlebarButtonState::Pressed => &self.buffer_pressed,
            };
            self.surface.attach(Some(buffer), 0, 0);
            self.surface.damage_buffer(0, 0, TITLEBAR_BUTTON_SIZE as i32, TITLEBAR_BUTTON_SIZE as i32);
        } else {
            self.surface.attach(None, 0, 0);
        }
        self.surface.commit();
    }

    fn set_state(&mut self, state: TitlebarButtonState) {
        if self.state == state {
            return;
        }
        self.state = state;
        if self.visible {
            let buffer = match state {
                TitlebarButtonState::Normal => &self.buffer_normal,
                TitlebarButtonState::Hover => &self.buffer_hover,
                TitlebarButtonState::Pressed => &self.buffer_pressed,
            };
            self.surface.attach(Some(buffer), 0, 0);
            self.surface.damage_buffer(0, 0, TITLEBAR_BUTTON_SIZE as i32, TITLEBAR_BUTTON_SIZE as i32);
            self.surface.commit();
        }
    }

    fn update_position(&mut self, titlebar_height: i32, is_fullscreen: bool) {
        let (x, y) = if is_fullscreen {
            (
                detect_fullscreen_button_x(self.kind),
                detect_fullscreen_button_y(),
            )
        } else {
            (
                detect_titlebar_button_x(self.kind),
                detect_titlebar_button_y(titlebar_height),
            )
        };
        self.subsurface.set_position(x, y);
        self.surface.commit();
    }
}

fn dist_to_segment(px: f32, py: f32, x1: f32, y1: f32, x2: f32, y2: f32) -> f32 {
    let dx = x2 - x1;
    let dy = y2 - y1;
    let l2 = dx * dx + dy * dy;
    if l2 == 0.0 {
        return ((px - x1).powi(2) + (py - y1).powi(2)).sqrt();
    }
    let t = (((px - x1) * dx + (py - y1) * dy) / l2).clamp(0.0, 1.0);
    let proj_x = x1 + t * dx;
    let proj_y = y1 + t * dy;
    ((px - proj_x).powi(2) + (py - proj_y).powi(2)).sqrt()
}

fn render_titlebar_button_buffer(
    globals: &layers::Globals,
    qh: &QueueHandle<App>,
    kind: TitlebarButtonKind,
    state: TitlebarButtonState,
    is_fullscreen: bool,
) -> Result<wl_buffer::WlBuffer, PresenterError> {
    let size = TITLEBAR_BUTTON_SIZE;
    let mut pixels = vec![0u32; (size * size) as usize];

    let center = (size as f32) / 2.0;
    let radius = 12.0f32;

    let (r_bg, g_bg, b_bg, bg_alpha_factor) = if is_fullscreen {
        match state {
            TitlebarButtonState::Normal => (0.15f32, 0.16f32, 0.18f32, 0.70f32),
            TitlebarButtonState::Hover => (0.25f32, 0.27f32, 0.30f32, 0.85f32),
            TitlebarButtonState::Pressed => (0.35f32, 0.38f32, 0.42f32, 0.95f32),
        }
    } else {
        let theme = sctk_adwaita::theme::ColorTheme::auto();
        let colors = theme.active;
        let (r, g, b) = match state {
            TitlebarButtonState::Normal => {
                let c = colors.button_idle;
                (c.red(), c.green(), c.blue())
            }
            TitlebarButtonState::Hover => {
                let c = colors.button_hover;
                (c.red(), c.green(), c.blue())
            }
            TitlebarButtonState::Pressed => {
                let c = colors.button_hover;
                let factor = if colors.button_icon.red() > 0.5 { 1.25f32 } else { 0.85f32 };
                ((c.red() * factor).min(1.0), (c.green() * factor).min(1.0), (c.blue() * factor).min(1.0))
            }
        };
        (r, g, b, 1.0f32)
    };

    let (r_icon, g_icon, b_icon) = if is_fullscreen {
        (0.95f32, 0.95f32, 0.95f32)
    } else {
        let theme = sctk_adwaita::theme::ColorTheme::auto();
        let c = theme.active.button_icon;
        (c.red(), c.green(), c.blue())
    };

    let segments: &[((f32, f32), (f32, f32))] = match kind {
        TitlebarButtonKind::Back => &[
            ((16.0, 9.5), (11.5, 14.0)),
            ((11.5, 14.0), (16.0, 18.5)),
        ],
        TitlebarButtonKind::Fullscreen => {
            if is_fullscreen {
                &[
                    ((12.5, 9.5), (12.5, 12.5)),
                    ((9.5, 12.5), (12.5, 12.5)),
                    ((15.5, 9.5), (15.5, 12.5)),
                    ((18.5, 12.5), (15.5, 12.5)),
                    ((12.5, 18.5), (12.5, 15.5)),
                    ((9.5, 15.5), (12.5, 15.5)),
                    ((15.5, 18.5), (15.5, 15.5)),
                    ((18.5, 15.5), (15.5, 15.5)),
                ]
            } else {
                &[
                    ((9.5, 9.5), (12.5, 9.5)),
                    ((9.5, 9.5), (9.5, 12.5)),
                    ((15.5, 9.5), (18.5, 9.5)),
                    ((18.5, 9.5), (18.5, 12.5)),
                    ((9.5, 18.5), (12.5, 18.5)),
                    ((9.5, 18.5), (9.5, 15.5)),
                    ((18.5, 18.5), (15.5, 18.5)),
                    ((18.5, 18.5), (18.5, 15.5)),
                ]
            }
        }
    };
    let stroke_radius = match kind {
        TitlebarButtonKind::Back => 1.15f32,
        TitlebarButtonKind::Fullscreen => 1.0f32,
    };

    for y in 0..size {
        for x in 0..size {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;

            let dist_center = ((px - center).powi(2) + (py - center).powi(2)).sqrt();
            let base_circle_alpha = (radius + 0.5 - dist_center).clamp(0.0, 1.0);
            let circle_alpha = base_circle_alpha * bg_alpha_factor;

            let mut min_d = f32::MAX;
            for &((x1, y1), (x2, y2)) in segments {
                let d = dist_to_segment(px, py, x1, y1, x2, y2);
                if d < min_d {
                    min_d = d;
                }
            }
            let stroke_alpha = (stroke_radius + 0.5 - min_d).clamp(0.0, 1.0);

            let eff_stroke_a = stroke_alpha.min(base_circle_alpha);

            let out_a = (eff_stroke_a + circle_alpha * (1.0 - eff_stroke_a)).min(1.0);
            let out_r = eff_stroke_a * r_icon + circle_alpha * (1.0 - eff_stroke_a) * r_bg;
            let out_g = eff_stroke_a * g_icon + circle_alpha * (1.0 - eff_stroke_a) * g_bg;
            let out_b = eff_stroke_a * b_icon + circle_alpha * (1.0 - eff_stroke_a) * b_bg;

            let a = (out_a * 255.0).round().min(255.0) as u32;
            let r = (out_r * 255.0).round().min(255.0) as u32;
            let g = (out_g * 255.0).round().min(255.0) as u32;
            let b = (out_b * 255.0).round().min(255.0) as u32;

            pixels[(y * size + x) as usize] = (a << 24) | (r << 16) | (g << 8) | b;
        }
    }

    let bytes = (size * size * 4) as usize;
    let mut file = tempfile::tempfile()?;
    let mut byte_data = Vec::with_capacity(bytes);
    for pixel in pixels {
        byte_data.extend_from_slice(&pixel.to_ne_bytes());
    }
    file.write_all(&byte_data)?;
    let pool = globals.shm.create_pool(file.as_fd(), bytes as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        size as i32,
        size as i32,
        (size * 4) as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    Ok(buffer)
}

/// One scroll step for a continuous (touchpad or finger) Wayland scroll,
/// measured in surface pixels, following common toolkit behaviour.
const CONTINUOUS_SCROLL_PIXELS_PER_STEP: f64 = 10.0;
const FRACTIONAL_SCALE_DENOMINATOR: u32 = 120;
const MAX_BUFFER_DIMENSION: u32 = 16_384;

fn app_display_title(package: &str) -> String {
    let component = package.rsplit('.').next().filter(|part| !part.is_empty()).unwrap_or(package);
    let mut characters = component.chars();
    match characters.next() {
        Some(first) => first.to_uppercase().chain(characters).collect(),
        None => package.to_owned(),
    }
}

fn format_task_title(package: &str, is_immersed: bool, is_fullscreen: bool) -> String {
    let title = app_display_title(package);
    if is_immersed && is_fullscreen {
        format!("{title} [Immersed: Ctrl+Alt+M] [Fullscreen: F11/Esc]")
    } else if is_immersed {
        format!("{title} [Immersed: Ctrl+Alt+M]")
    } else if is_fullscreen {
        format!("{title} [Fullscreen: F11/Esc]")
    } else {
        title
    }
}

#[derive(Debug, Error)]
enum PresenterError {
    #[error("I/O failure: {0}")]
    Io(#[from] io::Error),
    #[error("Wayland connection failure: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("Wayland global enumeration failure: {0}")]
    Global(#[from] wayland_client::globals::GlobalError),
    #[error("Wayland toolkit global failure: {0}")]
    ToolkitGlobal(#[from] smithay_client_toolkit::error::GlobalError),
    #[error("Wayland global bind failure: {0}")]
    Bind(#[from] BindError),
    #[error("Wayland dispatch failure: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("Wayland transport failure: {0}")]
    Wayland(String),
    #[error("Droidloom endpoint listener failure: {0}")]
    Listener(#[from] EndpointListenerError),
    #[error("Droidloom endpoint failure: {0}")]
    Endpoint(#[from] EndpointError),
    #[error("Droidloom IPC failure: {0}")]
    Ipc(#[from] IpcError),
    #[error("DRM syncobj failure: {0}")]
    Syncobj(#[from] droidloom_syncobj::SyncobjError),
    #[error("GBM failure: {0}")]
    Gbm(String),
    #[error("the compositor does not advertise linux-dmabuf v4 feedback")]
    DmabufV4Required,
    #[error("the compositor exposes no linear ARGB8888 or XRGB8888 DMA-BUF format")]
    NoRenderFormat,
    #[error("task {0:?} does not exist")]
    UnknownTask(TaskObjectId),
    #[error("task {object:?} presented unknown buffer {buffer:?}")]
    UnknownBuffer {
        object: TaskObjectId,
        buffer: BufferId,
    },
    #[error("Droidloom configuration is invalid: {0}")]
    Configuration(&'static str),
    #[error("Wayland client-side decoration failure: {0}")]
    WindowDecoration(String),
}

struct RenderTarget {
    metadata: BufferMetadata,
    planes: Vec<OwnedFd>,
    wayland: Option<wl_buffer::WlBuffer>,
    sync: Option<BufferSync>,
    _allocation: Option<BufferObject<()>>,
    host_owned: bool,
    retired: bool,
    busy: bool,
    logical_size: (u32, u32),
}

// Timeline progress implies completion of every earlier point. Each buffer
// therefore owns its timeline: neither a newer buffer's acquire fence nor an
// out-of-order compositor release may satisfy this buffer's release point.
struct BufferSync {
    timeline: WaylandTimeline,
    proxy: WpLinuxDrmSyncobjTimelineV1,
    next_point: u64,
    release_wakeup: Option<OwnedFd>,
}

impl BufferSync {
    fn arm_release_wakeup(&mut self, point: u64) -> Result<(), PresenterError> {
        let Some(event) = self.release_wakeup.as_ref() else { return Ok(()); };
        match self.timeline.notify_when_signalled(point, event.as_fd()) {
            Ok(()) => Ok(()),
            Err(droidloom_syncobj::SyncobjError::Drm { source, .. })
                if matches!(source.raw_os_error(), Some(libc::ENOTTY | libc::EOPNOTSUPP | libc::EINVAL)) =>
            {
                // Older timeline-capable kernels still use the bounded poll
                // fallback. Never replace an unsupported wakeup with a release.
                self.release_wakeup = None;
                Ok(())
            }
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for BufferSync {
    fn drop(&mut self) {
        self.proxy.destroy();
    }
}

struct PendingFrame {
    frame: FrameId,
    buffer: BufferId,
    release_point: u64,
    submitted_nanos: u64,
    trace: Option<presentation_audit::FrameTrace>,
}

// Timing metadata outlives buffer ownership; no buffer or fence is retained here.
struct FrameFeedback {
    proxy: WpPresentationFeedback,
    frame: FrameId,
    buffer: BufferId,
    submitted_nanos: u64,
}

struct TaskWindow {
    layers: Option<layers::Stream>,
    package: String,
    android_task: Option<u64>,
    window: Option<Window>,
    window_frame: Option<AdwaitaFrame<App>>,
    titlebar_buttons: Vec<TitlebarButton>,
    decorations_hidden: bool,
    viewport: Option<WpViewport>,
    fractional_scale: Option<WpFractionalScaleV1>,
    sync_surface: Option<WpLinuxDrmSyncobjSurfaceV1>,
    logical_size: Option<(u32, u32)>,
    buffer_size: (u32, u32),
    preferred_scale_120: u32,
    configure_serial: u32,
    refresh_millihz: u32,
    entered_outputs: Vec<wl_output::WlOutput>,
    presentation_output: Option<wl_output::WlOutput>,
    targets: BTreeMap<BufferId, RenderTarget>,
    pending: VecDeque<PendingFrame>,
    presentation_feedback: Vec<FrameFeedback>,
    presentation_audit: Option<presentation_audit::Audit>,
    content_opaque: bool,
    applied_opaque: Option<bool>,
    focused: bool,
    fullscreen: bool,
    maximized: bool,
    floating_size: Option<LogicalSize>,
    fullscreen_controls_revealed_until: Option<Instant>,
    unmap_requested: bool,
    closing: bool,
}

impl TaskWindow {
    fn headless(&self) -> bool {
        self.package == BOOTSTRAP_PACKAGE
    }

    fn accepts_present(&self) -> bool {
        !self.headless() && self.window.is_some() && !self.unmap_requested && !self.closing
    }

    fn surface(&self) -> Option<&wl_surface::WlSurface> {
        self.window.as_ref().map(Window::wl_surface)
    }
}

#[derive(Clone, Copy)]
struct Contact {
    object: TaskObjectId,
    pointer_id: u32,
    position_fixed: (i32, i32),
}

struct DecorationTouch {
    id: i32,
    touch: wl_touch::WlTouch,
    object: TaskObjectId,
    surface: wl_surface::WlSurface,
    seat: wl_seat::WlSeat,
    down_serial: u32,
    acted_on_down: bool,
}

impl Contact {
    fn event(self, action: TouchAction) -> InputEvent {
        InputEvent::Touch {
            action,
            pointer_id: self.pointer_id,
            x_fixed: self.position_fixed.0,
            y_fixed: self.position_fixed.1,
            pressure: match action {
                TouchAction::Down | TouchAction::Motion => u16::MAX,
                TouchAction::Up | TouchAction::Cancel => 0,
            },
        }
    }
}

struct TabletToolState {
    tool_id: u32,
    tool_type: TabletToolType,
    supported: bool,
    surface: Option<wl_surface::WlSurface>,
    x_fixed: i32,
    y_fixed: i32,
    pressure: u16,
    distance: u16,
    tilt_x_tenths: i16,
    tilt_y_tenths: i16,
    rotation_tenths: i16,
    wheel_clicks: i16,
    slider: i32,
    wheel_degrees_fixed: i32,
    axis_flags: u8,
    dirty_axes: bool,
    pending_actions: Vec<(TabletAction, u32)>,
    down_serial: u32,
    decoration_down: Option<TaskObjectId>,
}

impl TabletToolState {
    fn new(tool_id: u32) -> Self {
        Self {
            tool_id,
            tool_type: TabletToolType::Pen,
            supported: true,
            surface: None,
            x_fixed: 0,
            y_fixed: 0,
            pressure: 0,
            distance: 0,
            tilt_x_tenths: 0,
            tilt_y_tenths: 0,
            rotation_tenths: 0,
            wheel_clicks: 0,
            slider: 0,
            wheel_degrees_fixed: 0,
            axis_flags: 0,
            dirty_axes: false,
            pending_actions: Vec::new(),
            down_serial: 0,
            decoration_down: None,
        }
    }
}

enum TabletPadChild {
    Group(ZwpTabletPadGroupV2),
    Ring(ZwpTabletPadRingV2),
    Strip(ZwpTabletPadStripV2),
    Dial(ZwpTabletPadDialV2),
}

impl TabletPadChild {
    fn destroy(self) {
        match self {
            Self::Group(proxy) => proxy.destroy(),
            Self::Ring(proxy) => proxy.destroy(),
            Self::Strip(proxy) => proxy.destroy(),
            Self::Dial(proxy) => proxy.destroy(),
        }
    }
}

struct TabletPadState {
    tablet_seat_id: u32,
    proxy: ZwpTabletPadV2,
    children: Vec<TabletPadChild>,
    focus: Option<TaskObjectId>,
    pressed: BTreeSet<u32>,
}

/// The pointer confined to one Android window, with compositor shortcuts
/// delivered to the application instead of the desktop.
struct Immersion {
    object: TaskObjectId,
    confined: ZwpConfinedPointerV1,
    inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
}

impl Immersion {
    fn release(self) {
        self.confined.destroy();
        if let Some(inhibitor) = self.inhibitor {
            inhibitor.destroy();
        }
    }
}

struct App {
    activation: activation::Activation,
    layer_globals: layers::Globals,
    clipboard: clipboard::Clipboard,
    text_input: text_input::TextInput,
    registry_state: RegistryState,
    output_state: OutputState,
    presentation_time: PresentationTimeState,
    seat_state: SeatState,
    subcompositor_state: Arc<SubcompositorState>,
    shm_state: Shm,
    tablet_manager: Option<ZwpTabletManagerV2>,
    tablet_seats: Vec<(wl_seat::WlSeat, ZwpTabletSeatV2)>,
    tablet_tools: Vec<TabletToolState>,
    tablet_tool_seats: BTreeMap<u32, u32>,
    tablet_tool_proxies: BTreeMap<u32, ZwpTabletToolV2>,
    tablet_proxies: BTreeMap<u32, (u32, ZwpTabletV2)>,
    tablet_pads: BTreeMap<u32, TabletPadState>,
    tablet_pad_groups: BTreeMap<u32, u32>,
    dmabuf_state: DmabufState,
    feedback: Option<DmabufFeedback>,
    compositor: Arc<CompositorState>,
    xdg_shell: XdgShell,
    insets_manager: Option<insets::DenialInsetsManagerV1>,
    viewporter: WpViewporter,
    fractional_scale_manager: WpFractionalScaleManagerV1,
    sync_manager: WpLinuxDrmSyncobjManagerV1,
    syncobj: SyncobjDevice,
    gbm: GbmDevice<File>,
    listener: DenialEndpointListener,
    window_policy: WindowPolicyStore,
    endpoint: Option<DenialEndpoint>,
    tasks: BTreeMap<TaskObjectId, TaskWindow>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    keyboard_seat: Option<wl_seat::WlSeat>,
    modifiers: Modifiers,
    /// Keys whose press was routed, with the task that must receive the release.
    pressed_keys: BTreeMap<u32, TaskObjectId>,
    /// Shortcut keys consumed by the presenter; their release is consumed too.
    consumed_keys: BTreeSet<u32>,
    pointer: Option<ThemedPointer>,
    pointer_constraints: PointerConstraintsState,
    shortcuts_inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    immersion: Option<Immersion>,
    cursor_icon: Option<CursorIcon>,
    touch: Option<wl_touch::WlTouch>,
    focused: Option<TaskObjectId>,
    /// Task under the mouse for native mouse routing, and its pressed buttons.
    mouse_focus: Option<TaskObjectId>,
    mouse_buttons: BTreeSet<u32>,
    pointer_contact: Option<Contact>,
    touch_contacts: BTreeMap<i32, Contact>,
    edge_gestures: BTreeMap<i32, gesture::EdgeGestureTracker>,
    decoration_touch: Option<DecorationTouch>,
    titlebar_button_pointer: Option<(TaskObjectId, TitlebarButtonKind)>,
    titlebar_button_touch: Option<(i32, TaskObjectId, TitlebarButtonKind)>,
    titlebar_button_tablet: Option<(TaskObjectId, TitlebarButtonKind)>,
    next_buffer_id: u64,
    next_input_serial: u64,
    socket_path: PathBuf,
    start_time: Instant,
    fatal: Option<String>,
}

impl Drop for App {
    fn drop(&mut self) {
        remove_owned_socket(&self.socket_path);
    }
}

impl App {
    fn bind_tablet_seat(&mut self, seat: wl_seat::WlSeat, qh: &QueueHandle<Self>) {
        if self.tablet_manager.is_none()
            || self.tablet_seats.iter().any(|(existing, _)| *existing == seat)
        {
            return;
        }
        let tablet_seat = self.tablet_manager.as_ref().unwrap().get_tablet_seat(&seat, qh, ());
        self.tablet_seats.push((seat, tablet_seat));
        eprintln!(
            "Droidloom tablet trace: stage=seat-bound count={}",
            self.tablet_seats.len()
        );
    }

    fn task_for_surface(&self, surface: &wl_surface::WlSurface) -> Option<TaskObjectId> {
        self.tasks.iter().find_map(|(object, task)| {
            task.surface()
                .is_some_and(|candidate| candidate == surface)
                .then_some(*object)
        })
    }

    fn task_and_button_for_surface(&self, surface: &wl_surface::WlSurface) -> Option<(TaskObjectId, TitlebarButtonKind)> {
        for (&object, task) in &self.tasks {
            for btn in &task.titlebar_buttons {
                if btn.visible && btn.surface == *surface {
                    return Some((object, btn.kind));
                }
            }
        }
        None
    }

    fn update_titlebar_button_state(&mut self, object: TaskObjectId, kind: TitlebarButtonKind, state: TitlebarButtonState) {
        if let Some(task) = self.tasks.get_mut(&object) {
            for btn in &mut task.titlebar_buttons {
                if btn.kind == kind {
                    btn.set_state(state);
                    break;
                }
            }
        }
    }

    fn handle_titlebar_button_click(&mut self, object: TaskObjectId, kind: TitlebarButtonKind) {
        match kind {
            TitlebarButtonKind::Back => {
                self.send_back_key(object);
            }
            TitlebarButtonKind::Fullscreen => {
                self.toggle_fullscreen(object);
            }
        }
    }

    fn clear_titlebar_buttons(&mut self, object: TaskObjectId) {
        if let Some(task) = self.tasks.get_mut(&object) {
            task.titlebar_buttons.clear();
        }
        if self.titlebar_button_pointer.is_some_and(|(obj, _)| obj == object) {
            self.titlebar_button_pointer = None;
        }
        if self.titlebar_button_touch.is_some_and(|(_, obj, _)| obj == object) {
            self.titlebar_button_touch = None;
        }
        if self.titlebar_button_tablet.is_some_and(|(obj, _)| obj == object) {
            self.titlebar_button_tablet = None;
        }
    }

    fn ensure_titlebar_buttons(&mut self, qh: &QueueHandle<Self>, object: TaskObjectId, titlebar_height: i32) {
        let Some(task) = self.tasks.get_mut(&object) else { return };
        let is_fullscreen = task.fullscreen || task.decorations_hidden;
        let should_be_visible = !is_fullscreen || task.fullscreen_controls_revealed_until.is_some();
        if !task.titlebar_buttons.is_empty() {
            for btn in &mut task.titlebar_buttons {
                btn.update_position(titlebar_height, is_fullscreen);
                btn.set_visible(should_be_visible);
            }
            return;
        }
        let Some(window) = task.window.as_ref() else { return };
        let surface = window.wl_surface();
        if let Ok(mut back) = TitlebarButton::new(
            TitlebarButtonKind::Back,
            &self.layer_globals,
            &self.compositor,
            surface,
            qh,
            titlebar_height,
            is_fullscreen,
        ) {
            back.set_visible(should_be_visible);
            task.titlebar_buttons.push(back);
        }
        if let Ok(mut fs) = TitlebarButton::new(
            TitlebarButtonKind::Fullscreen,
            &self.layer_globals,
            &self.compositor,
            surface,
            qh,
            titlebar_height,
            is_fullscreen,
        ) {
            fs.set_visible(should_be_visible);
            task.titlebar_buttons.push(fs);
        }
    }

    fn reveal_fullscreen_controls(&mut self, object: TaskObjectId, duration: Duration) {
        let Some(task) = self.tasks.get_mut(&object) else { return };
        task.fullscreen_controls_revealed_until = Some(Instant::now() + duration);
        for btn in &mut task.titlebar_buttons {
            btn.set_visible(true);
        }
    }

    fn hide_fullscreen_controls(&mut self, object: TaskObjectId) {
        let Some(task) = self.tasks.get_mut(&object) else { return };
        task.fullscreen_controls_revealed_until = None;
        if task.fullscreen || task.decorations_hidden {
            for btn in &mut task.titlebar_buttons {
                btn.set_visible(false);
            }
        }
    }

    fn pump_fullscreen_controls_timeouts(&mut self) {
        let now = Instant::now();
        for task in self.tasks.values_mut() {
            if let Some(until) = task.fullscreen_controls_revealed_until {
                if now >= until {
                    task.fullscreen_controls_revealed_until = None;
                    if task.fullscreen || task.decorations_hidden {
                        for btn in &mut task.titlebar_buttons {
                            btn.set_visible(false);
                        }
                    }
                }
            }
        }
    }

    fn next_controls_timeout_millis(&self) -> Option<i32> {
        let now = Instant::now();
        let mut min_remaining = None;
        for task in self.tasks.values() {
            if let Some(until) = task.fullscreen_controls_revealed_until {
                let remaining = if until > now {
                    until.duration_since(now)
                } else {
                    Duration::ZERO
                };
                let millis = i32::try_from(remaining.as_millis()).unwrap_or(i32::MAX);
                min_remaining = Some(min_remaining.map_or(millis, |cur: i32| cur.min(millis)));
            }
        }
        min_remaining
    }

    fn toggle_decorations(&mut self, qh: &QueueHandle<Self>, object: TaskObjectId) {
        let Some(task) = self.tasks.get_mut(&object) else { return };
        if task.headless() || task.fullscreen {
            return;
        }
        task.decorations_hidden = !task.decorations_hidden;
        let is_hidden = task.decorations_hidden;
        if let Some(frame) = task.window_frame.as_mut() {
            frame.set_hidden(is_hidden);
        }
        self.clear_titlebar_buttons(object);
        self.ensure_titlebar_buttons(qh, object, if is_hidden { 0 } else { DEFAULT_TITLEBAR_HEIGHT });
        let Some(task) = self.tasks.get_mut(&object) else { return };
        let Some((width, height)) = task.logical_size else { return };
        let scale_120 = task.preferred_scale_120;
        if let Some(window) = task.window.as_ref() {
            let (x, y, outer_width, outer_height, _frame_needs_parent_commit) =
                match task.window_frame.as_mut() {
                    Some(frame) if !frame.is_hidden() => {
                        frame.set_scaling_factor(f64::from(scale_120) / 120.0);
                        if let (Some(w), Some(h)) = (NonZeroU32::new(width), NonZeroU32::new(height)) {
                            frame.resize(w, h);
                        }
                        let (x, y) = frame.location();
                        let (w, h) = frame.add_borders(width, height);
                        let needs_parent_commit = frame.draw();
                        (x, y, w, h, needs_parent_commit)
                    }
                    _ => {
                        if let Some(frame) = task.window_frame.as_mut() {
                            let _ = frame.draw();
                        }
                        (0, 0, width, height, true)
                    }
                };
            window.xdg_surface().set_window_geometry(
                x,
                y,
                i32::try_from(outer_width).unwrap_or(i32::MAX),
                i32::try_from(outer_height).unwrap_or(i32::MAX),
            );
            window.wl_surface().commit();
        }
    }

    fn task_refresh_millihz(&self, object: TaskObjectId) -> u32 {
        let Some(task) = self.tasks.get(&object) else { return 60_000; };
        if task.package == BOOTSTRAP_PACKAGE {
            // Android's shared scheduler must accommodate active task outputs,
            // but an unrelated faster monitor must not drive its cadence.
            return self.tasks.iter().filter(|(_, task)| task.package != BOOTSTRAP_PACKAGE
                    && !task.closing && !task.entered_outputs.is_empty())
                .map(|(object, _)| self.task_refresh_millihz(*object))
                .max().unwrap_or(60_000);
        }
        select_surface_refresh(
            &task.entered_outputs,
            task.presentation_output.as_ref(),
            |output| self.output_state.info(output).and_then(|info| {
                info.modes.iter()
                    .find(|mode| mode.current && mode.refresh_rate > 0)
                    .and_then(|mode| u32::try_from(mode.refresh_rate).ok())
            }),
        )
    }

    fn update_task_refresh(&mut self, object: TaskObjectId) -> Result<(), PresenterError> {
        let refresh = self.task_refresh_millihz(object);
        let task = self.tasks.get_mut(&object).ok_or(PresenterError::UnknownTask(object))?;
        if task.configure_serial == 0 || task.refresh_millihz == refresh { return Ok(()); }
        // A cadence change preserves the configure's allocation generation.
        // Existing targets and in-flight frames still satisfy its dimensions.
        self.endpoint.as_mut()
            .ok_or(PresenterError::Configuration("endpoint disappeared"))?
            .send_configure(object, task.configure_serial, task.buffer_size.0,
                task.buffer_size.1, task.preferred_scale_120,
                FRACTIONAL_SCALE_DENOMINATOR, Transform::Normal, refresh)?;
        task.refresh_millihz = refresh;
        if task.package != BOOTSTRAP_PACKAGE {
            let bootstrap = self.tasks.iter()
                .find_map(|(object, task)| (task.package == BOOTSTRAP_PACKAGE).then_some(*object));
            if let Some(bootstrap) = bootstrap { self.update_task_refresh(bootstrap)?; }
        }
        Ok(())
    }

    fn update_output_refreshes(&mut self) -> Result<(), PresenterError> {
        let objects = self.tasks.keys().copied().collect::<Vec<_>>();
        for object in objects { self.update_task_refresh(object)?; }
        Ok(())
    }

    fn host_canvas_size(&self) -> Option<(u32, u32)> {
        self.output_state
            .outputs()
            .filter_map(|output| self.output_state.info(&output))
            .filter_map(|info| {
                let mode = info
                    .modes
                    .iter()
                    .find(|mode| mode.current)
                    .or_else(|| info.modes.iter().find(|mode| mode.preferred))?;
                oriented_output_size(mode.dimensions, info.transform)
            })
            .max_by_key(|(width, height)| u64::from(*width) * u64::from(*height))
    }

    fn host_logical_canvas_size(&self) -> Option<LogicalSize> {
        self.output_state
            .outputs()
            .filter_map(|output| self.output_state.info(&output))
            .filter_map(|info| {
                let logical = info.logical_size.and_then(|(width, height)| {
                    Some(LogicalSize::new(
                        u32::try_from(width).ok()?,
                        u32::try_from(height).ok()?,
                    ))
                });
                logical.transpose().ok().flatten().or_else(|| {
                    let mode = info
                        .modes
                        .iter()
                        .find(|mode| mode.current)
                        .or_else(|| info.modes.iter().find(|mode| mode.preferred))?;
                    let (width, height) = oriented_output_size(mode.dimensions, info.transform)?;
                    let scale = u32::try_from(info.scale_factor.max(1)).ok()?;
                    LogicalSize::new(width.div_ceil(scale), height.div_ceil(scale)).ok()
                })
            })
            .max_by_key(|size| u64::from(size.width) * u64::from(size.height))
    }

    fn reconfigure_bootstrap(&mut self, qh: &QueueHandle<Self>) -> Result<(), PresenterError> {
        let Some(object) = self
            .tasks
            .iter()
            .find_map(|(object, task)| task.headless().then_some(*object))
        else {
            return Ok(());
        };
        let Some((width, height)) = self.host_canvas_size() else {
            return Ok(());
        };
        self.configure_task(qh, object, width, height)
    }

    fn selected_format(&self) -> Result<(DrmFourcc, Vec<Modifier>), PresenterError> {
        let feedback = self
            .feedback
            .as_ref()
            .ok_or(PresenterError::DmabufV4Required)?;
        for fourcc in [DrmFourcc::Argb8888, DrmFourcc::Xrgb8888] {
            let mut modifiers = feedback
                .tranches()
                .iter()
                .flat_map(|tranche| tranche.formats.iter())
                .filter_map(|index| feedback.format_table().get(*index as usize))
                // Host Mesa and Android Mesa can disagree about compressed or
                // tiled layouts even when Wayland advertises them. Linear
                // DMA-BUFs keep the shared render targets importable across
                // that boundary, including laptops with multiple GPUs.
                .filter(|entry| {
                    entry.format == fourcc as u32
                        && entry.modifier == u64::from(Modifier::Linear)
                })
                .map(|entry| Modifier::from(entry.modifier))
                .collect::<Vec<_>>();
            modifiers.sort_unstable_by_key(|modifier| u64::from(*modifier));
            modifiers.dedup();
            if !modifiers.is_empty() {
                return Ok((fourcc, modifiers));
            }
        }
        Err(PresenterError::NoRenderFormat)
    }

    fn published_formats(&self) -> Result<Vec<FormatModifier>, PresenterError> {
        let (fourcc, modifiers) = self.selected_format()?;
        Ok(modifiers
            .into_iter()
            .map(|modifier| FormatModifier {
                fourcc: fourcc as u32,
                modifier: u64::from(modifier),
            })
            .collect())
    }

    fn create_task(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        package: &str,
    ) -> Result<(), PresenterError> {
        let headless = package == BOOTSTRAP_PACKAGE;
        if !headless && let Err(error) = self.window_policy.reload_policy() {
            eprintln!("Droidloom kept the last valid window policy: {error}");
        }
        let (window, viewport, fractional_scale, sync_surface) =
            if headless {
                (None, None, None, None)
            } else {
                let surface = self.compositor.create_surface(qh);
                let window = self
                    .xdg_shell
                    .create_window(surface, WindowDecorations::RequestServer, qh);
                let title = format_task_title(package, false, false);
                window.set_title(&title);
                window.set_app_id(package.to_owned());
                window.set_min_size(Some((1, 1)));
                if let Some(manager) = &self.insets_manager {
                    manager.set_self_managed(window.wl_surface());
                }
                // fractional-scale-v1 keeps the wl_surface at scale 1 and uses a
                // viewport to map the higher-resolution buffer into XDG logical
                // coordinates.
                window.wl_surface().set_buffer_scale(1);
                let viewport = self.viewporter.get_viewport(window.wl_surface(), qh, ());
                let fractional_scale = self.fractional_scale_manager.get_fractional_scale(
                    window.wl_surface(),
                    qh,
                    object,
                );
                let sync_surface = self.sync_manager.get_surface(window.wl_surface(), qh, ());
                window.commit();
                (
                    Some(window),
                    Some(viewport),
                    Some(fractional_scale),
                    Some(sync_surface),
                )
            };
        self.tasks.insert(
            object,
            TaskWindow {
                layers: None,
                package: package.to_owned(),
                android_task: None,
                window,
                window_frame: None,
                titlebar_buttons: Vec::new(),
                decorations_hidden: false,
                viewport,
                fractional_scale,
                sync_surface,
                logical_size: None,
                buffer_size: (0, 0),
                preferred_scale_120: FRACTIONAL_SCALE_DENOMINATOR,
                configure_serial: 0,
                refresh_millihz: 60_000,
            entered_outputs: Vec::new(),
            presentation_output: None,
                targets: BTreeMap::new(),
                pending: VecDeque::new(),
                presentation_feedback: Vec::new(),
                content_opaque: false,
                applied_opaque: None,
                presentation_audit: (env::var_os("DROIDLOOM_PRESENT_AUDIT").as_deref() == Some(std::ffi::OsStr::new("1")))
                    .then(presentation_audit::Audit::default),
                focused: false,
                fullscreen: false,
                maximized: false,
                floating_size: None,
                fullscreen_controls_revealed_until: None,
                unmap_requested: false,
                closing: false,
            },
        );
        let formats = self.published_formats()?;
        self.endpoint
            .as_mut()
            .ok_or(PresenterError::Configuration("endpoint disappeared"))?
            .send_format_feedback(object, 1, formats)?;
        if headless {
            // The hidden bootstrap object defines Android's single built-in
            // desktop canvas. wl_output mode dimensions are already physical
            // pixels, equivalent to logical output size multiplied by its
            // scale, so do not apply the scale a second time here.
            let (width, height) = self
                .host_canvas_size()
                .ok_or(PresenterError::Configuration(
                    "Wayland output has no current pixel mode",
                ))?;
            self.configure_task(qh, object, width, height)?;
        }
        Ok(())
    }

    fn content_configure_size(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        configure: &WindowConfigure,
    ) -> Result<(Option<NonZeroU32>, Option<NonZeroU32>), PresenterError> {
        let requested = configure.new_size;
        let is_immersed = self.immersion.as_ref().is_some_and(|imm| imm.object == object);
        let task = self.tasks.get_mut(&object).ok_or(PresenterError::UnknownTask(object))?;
        if task.headless() {
            return Ok(requested);
        }
        let is_fullscreen = task.fullscreen || configure.state.contains(WindowState::FULLSCREEN);
        let hide_decorations = configure.decoration_mode != DecorationMode::Client
            || is_fullscreen
            || task.decorations_hidden;
        if hide_decorations {
            if let Some(frame) = task.window_frame.as_mut() {
                frame.set_hidden(true);
            }
            if is_fullscreen || task.decorations_hidden {
                self.ensure_titlebar_buttons(qh, object, 0);
            } else {
                self.clear_titlebar_buttons(object);
            }
            return Ok(requested);
        }
        if task.window_frame.is_none() {
            let window = task.window.as_ref()
                .ok_or(PresenterError::Configuration("window frame has no parent"))?;
            let frame = AdwaitaFrame::new(
                window,
                &self.shm_state,
                self.compositor.clone(),
                self.subcompositor_state.clone(),
                qh.clone(),
                FrameConfig::auto(),
            )
            .map_err(|error| PresenterError::WindowDecoration(error.to_string()))?;
            task.window_frame = Some(frame);
        }
        let title = format_task_title(&task.package, is_immersed, is_fullscreen);
        let frame = task.window_frame.as_mut().expect("frame initialized above");
        frame.set_title(&title);
        if let Some(window) = task.window.as_ref() {
            window.set_title(&title);
        }
        frame.set_hidden(false);
        frame.update_state(configure.state);
        frame.update_wm_capabilities(configure.capabilities);

        let titlebar_height = (-frame.location().1).max(DEFAULT_TITLEBAR_HEIGHT);
        let (width, height) = frame.subtract_borders(
            requested.0.unwrap_or(NonZeroU32::MIN),
            requested.1.unwrap_or(NonZeroU32::MIN),
        );

        self.ensure_titlebar_buttons(qh, object, titlebar_height);

        Ok((requested.0.and(width), requested.1.and(height)))
    }

    fn configure_task(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        width: u32,
        height: u32,
    ) -> Result<(), PresenterError> {
        let width = width.max(1);
        let height = height.max(1);
        let (scale_120, buffer_width, buffer_height) = {
            let task = self
                .tasks
                .get(&object)
                .ok_or(PresenterError::UnknownTask(object))?;
            let scale_120 = task.preferred_scale_120;
            (
                scale_120,
                scale_dimension(width, scale_120)?,
                scale_dimension(height, scale_120)?,
            )
        };
        // Geometry must use the resolved content size, including a compositor's
        // initial 0x0 configure and later scale-only updates.
        {
            let task = self.tasks.get_mut(&object)
                .ok_or(PresenterError::UnknownTask(object))?;
            if let Some(window) = task.window.as_ref() {
                let (x, y, outer_width, outer_height, frame_needs_parent_commit) =
                    match task.window_frame.as_mut() {
                        Some(frame) if !frame.is_hidden() => {
                            frame.set_scaling_factor(f64::from(scale_120) / 120.0);
                            frame.resize(NonZeroU32::new(width).unwrap(), NonZeroU32::new(height).unwrap());
                            let (x, y) = frame.location();
                            let (w, h) = frame.add_borders(width, height);
                            let needs_parent_commit = frame.draw();
                            (x, y, w, h, needs_parent_commit)
                        }
                        _ => (0, 0, width, height, false),
                    };
                window.xdg_surface().set_window_geometry(
                    x, y,
                    i32::try_from(outer_width).map_err(|_| PresenterError::Configuration("window width exceeds Wayland"))?,
                    i32::try_from(outer_height).map_err(|_| PresenterError::Configuration("window height exceeds Wayland"))?,
                );
                // Synchronized decoration subsurfaces only become visible after
                // the parent surface is committed. draw() returns true when it
                // synchronized its child commits, so commit even if no buffer
                // pool resize follows this configure.
                if frame_needs_parent_commit {
                    window.wl_surface().commit();
                }
            }
        }
        let needs_pool = {
            let task = self
                .tasks
                .get(&object)
                .ok_or(PresenterError::UnknownTask(object))?;
            task.configure_serial == 0
                || task.logical_size != Some((width, height))
                || task.buffer_size != (buffer_width, buffer_height)
        };
        if !needs_pool {
            return Ok(());
        }

        self.retire_host_targets(object)?;
        let serial = self
            .tasks
            .get(&object)
            .and_then(|task| task.configure_serial.checked_add(1))
            .ok_or(PresenterError::Configuration("configure serial exhausted"))?;
        let mut targets: Vec<RenderTarget> = Vec::with_capacity(TARGET_POOL_LENGTH);
        for _ in 0..TARGET_POOL_LENGTH {
            let id = self.allocate_buffer_id()?;
            let target = self.allocate_target(qh, id, buffer_width, buffer_height, width, height)?;
            let planes = target.planes.iter().map(AsFd::as_fd).collect::<Vec<_>>();
            let result = self
                .endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                .register_render_target(object, serial, &target.metadata, &planes);
            if let Err(error) = result {
                for registered in &targets {
                    let _ = self
                        .endpoint
                        .as_mut()
                        .expect("endpoint existed during target registration")
                        .unregister_render_target(object, registered.metadata.id);
                }
                return Err(error.into());
            }
            targets.push(target);
        }

        let refresh = self.task_refresh_millihz(object);
        {
            let task = self
                .tasks
                .get_mut(&object)
                .ok_or(PresenterError::UnknownTask(object))?;
            task.logical_size = Some((width, height));
            task.applied_opaque = None;
            task.buffer_size = (buffer_width, buffer_height);
            task.configure_serial = serial;
            task.refresh_millihz = refresh;
            task.targets.extend(
                targets
                    .into_iter()
                    .map(|target| (target.metadata.id, target)),
            );
        }
        self.endpoint
            .as_mut()
            .ok_or(PresenterError::Configuration("endpoint disappeared"))?
            .send_configure(
                object,
                serial,
                buffer_width,
                buffer_height,
                scale_120,
                FRACTIONAL_SCALE_DENOMINATOR,
                Transform::Normal,
                refresh,
            )?;
        self.update_output_refreshes()
    }

    fn allocate_buffer_id(&mut self) -> Result<BufferId, PresenterError> {
        self.next_buffer_id = self
            .next_buffer_id
            .checked_add(1)
            .ok_or(PresenterError::Configuration("buffer identity exhausted"))?;
        Ok(BufferId(self.next_buffer_id))
    }

    fn allocate_target(
        &self,
        _qh: &QueueHandle<Self>,
        id: BufferId,
        width: u32,
        height: u32,
        logical_width: u32,
        logical_height: u32,
    ) -> Result<RenderTarget, PresenterError> {
        let (fourcc, modifiers) = self.selected_format()?;
        let allocation = self
            .gbm
            .create_buffer_object_with_modifiers::<()>(
                width,
                height,
                fourcc,
                modifiers.iter().copied(),
            )
            .map_err(|error| PresenterError::Gbm(error.to_string()))?;
        let plane_count = allocation.plane_count();
        if !(1..=4).contains(&plane_count) {
            return Err(PresenterError::Gbm(format!(
                "GBM returned invalid plane count {plane_count}"
            )));
        }
        let actual_modifier = u64::from(allocation.modifier());
        let mut planes = Vec::with_capacity(plane_count as usize);
        let mut plane_metadata = Vec::with_capacity(plane_count as usize);
        for index in 0..plane_count {
            let plane = i32::try_from(index)
                .map_err(|_| PresenterError::Configuration("plane index overflow"))?;
            planes.push(
                allocation
                    .fd_for_plane(plane)
                    .map_err(|error| PresenterError::Gbm(error.to_string()))?,
            );
            plane_metadata.push(PlaneMetadata {
                index,
                offset: allocation.offset(plane),
                stride: allocation.stride_for_plane(plane),
            });
        }
        let metadata = BufferMetadata {
            id,
            width,
            height,
            format: FormatModifier {
                fourcc: fourcc as u32,
                modifier: actual_modifier,
            },
            planes: plane_metadata,
        };
        let wayland = None;
        Ok(RenderTarget {
            metadata,
            planes,
            wayland,
            sync: None,
            _allocation: Some(allocation),
            host_owned: true,
            retired: false,
            busy: false,
            logical_size: (logical_width, logical_height),
        })
    }

    fn create_wayland_buffer(
        &self,
        qh: &QueueHandle<Self>,
        metadata: &BufferMetadata,
        planes: &[OwnedFd],
    ) -> Result<wl_buffer::WlBuffer, PresenterError> {
        if planes.len() != metadata.planes.len() {
            return Err(PresenterError::Configuration(
                "DMA-BUF plane count mismatch",
            ));
        }
        let params = self.dmabuf_state.create_params(qh)?;
        for (plane, fd) in metadata.planes.iter().zip(planes) {
            params.add(
                fd.as_fd(),
                plane.index,
                plane.offset,
                plane.stride,
                metadata.format.modifier,
            );
        }
        let width = i32::try_from(metadata.width)
            .map_err(|_| PresenterError::Configuration("buffer width exceeds Wayland"))?;
        let height = i32::try_from(metadata.height)
            .map_err(|_| PresenterError::Configuration("buffer height exceeds Wayland"))?;
        let (buffer, proxy) = params.create_immed(
            width,
            height,
            metadata.format.fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
        );
        proxy.destroy();
        Ok(buffer)
    }

    fn retire_host_targets(&mut self, object: TaskObjectId) -> Result<(), PresenterError> {
        let idle = {
            let task = self
                .tasks
                .get_mut(&object)
                .ok_or(PresenterError::UnknownTask(object))?;
            task.targets
                .iter_mut()
                .filter_map(|(id, target)| {
                    if target.host_owned {
                        target.retired = true;
                        (!target.busy).then_some(*id)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        for id in idle {
            self.endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                .unregister_render_target(object, id)?;
            if let Some(target) = self
                .tasks
                .get_mut(&object)
                .and_then(|task| task.targets.remove(&id))
            {
                if let Some(buffer) = target.wayland {
                    buffer.destroy();
                }
            }
        }
        Ok(())
    }

    fn register_android_buffer(
        &mut self,
        _qh: &QueueHandle<Self>,
        object: TaskObjectId,
        metadata: BufferMetadata,
        planes: Vec<OwnedFd>,
    ) -> Result<(), PresenterError> {
        let wayland = None;
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        if task.targets.contains_key(&metadata.id) {
            return Err(PresenterError::Configuration(
                "duplicate local buffer identity",
            ));
        }
        let logical_size = task.logical_size.unwrap_or((metadata.width, metadata.height));
        task.targets.insert(
            metadata.id,
            RenderTarget {
                metadata,
                planes,
                wayland,
                sync: None,
                _allocation: None,
                host_owned: false,
                retired: false,
                busy: false,
                logical_size,
            },
        );
        Ok(())
    }

    fn present(
        &mut self,
        qh: &QueueHandle<Self>,
        frame: &AcceptedWireFrame,
        acquire_fence: &OwnedFd,
    ) -> Result<(), PresenterError> {
        let object = frame.object;
        let mut frame_trace = presentation_audit::FrameTrace::new(
            object.0, frame.frame.0, frame.buffer.0, acquire_fence.as_fd());
        if !self
            .tasks
            .get(&object)
            .ok_or(PresenterError::UnknownTask(object))?
            .accepts_present()
        {
            // Host close and Android task destruction are asynchronous. A
            // frame already queued by SurfaceFlinger can legally arrive after
            // the native toplevel has been unmapped. Complete that frame as a
            // deliberate discard; the task record remains until its earlier
            // Wayland releases drain, but the missing surface is not fatal to
            // the presenter or to unrelated Android applications.
            self.endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                .discard_frame(object, frame.frame)?;
            return Ok(());
        }

        // Import only buffers that will actually be attached to a visible
        // surface. Android's hidden bootstrap frames and superseded resize
        // targets must not fill the compositor's pending DMA-BUF import queue.
        let target = self
            .tasks
            .get(&object)
            .and_then(|task| task.targets.get(&frame.buffer))
            .ok_or(PresenterError::UnknownBuffer {
                object,
                buffer: frame.buffer,
            })?;
        if target.wayland.is_none() {
            let buffer = self.create_wayland_buffer(qh, &target.metadata, &target.planes)?;
            let timeline = WaylandTimeline::create(&self.syncobj)?;
            let descriptor = timeline.export_descriptor()?;
            let proxy = self.sync_manager.import_timeline(descriptor.as_fd(), qh, ());
            let target = self.tasks
                .get_mut(&object)
                .and_then(|task| task.targets.get_mut(&frame.buffer))
                .expect("target was just checked");
            target.wayland = Some(buffer);
            // SAFETY: eventfd has no pointer arguments; a nonnegative return
            // value is a new descriptor uniquely owned below.
            let event = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            if event < 0 { return Err(io::Error::last_os_error().into()); }
            target.sync = Some(BufferSync {
                timeline,
                proxy,
                next_point: 0,
                // SAFETY: ownership of the successful eventfd transfers once.
                release_wakeup: Some(unsafe { OwnedFd::from_raw_fd(event) }),
            });
        }
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        let surface = task
            .surface()
            .cloned()
            .expect("accepts_present guaranteed a mapped window");
        if task.applied_opaque != Some(task.content_opaque) {
            if task.content_opaque {
                let (width, height) = task.logical_size
                    .ok_or(PresenterError::Configuration("opaque target has no logical size"))?;
                let region = Region::new(self.compositor.as_ref())?;
                region.add(0, 0, i32::try_from(width).map_err(|_| PresenterError::Configuration("opaque width exceeds Wayland"))?,
                    i32::try_from(height).map_err(|_| PresenterError::Configuration("opaque height exceeds Wayland"))?);
                surface.set_opaque_region(Some(region.wl_region()));
            } else {
                surface.set_opaque_region(None);
            }
            task.applied_opaque = Some(task.content_opaque);
            if droidloom_syncobj::frame_trace::enabled() {
                eprintln!("Droidloom content state: object={} frame={} opaque={} source=android monotonic_ns={}",
                    object.0, frame.frame.0, task.content_opaque, droidloom_syncobj::frame_trace::now_ns());
            }
        }
        let layers_hidden = task.layers.as_mut().is_some_and(|layers| layers.hide());
        if layers_hidden {
            if let Some(viewport) = task.viewport.as_ref() { viewport.set_source(-1.0,-1.0,-1.0,-1.0); }
        }
        let target = task
            .targets
            .get_mut(&frame.buffer)
            .ok_or(PresenterError::UnknownBuffer {
                object,
                buffer: frame.buffer,
            })?;
        if target.busy || target.retired {
            return Err(PresenterError::Configuration(
                "presented buffer is unavailable",
            ));
        }
        let sync = target.sync.as_mut()
            .ok_or(PresenterError::Configuration("buffer timeline is absent"))?;
        let acquire_point =
            sync.next_point
                .checked_add(1)
                .ok_or(PresenterError::Configuration(
                    "Wayland sync point exhausted",
                ))?;
        let release_point = acquire_point
            .checked_add(1)
            .ok_or(PresenterError::Configuration(
                "Wayland sync point exhausted",
            ))?;
        sync.next_point = release_point;
        sync.timeline
            .import_acquire_fence(acquire_point, acquire_fence.as_fd())?;
        sync.arm_release_wakeup(release_point)?;
        if task.sync_surface.is_none() { task.sync_surface=Some(self.sync_manager.get_surface(&surface,qh,())); }
        let sync_surface = task
            .sync_surface
            .as_ref()
            .ok_or(PresenterError::Configuration(
                "window sync surface is absent",
            ))?;
        let (acquire_hi, acquire_lo) = split_point(acquire_point);
        let (release_hi, release_lo) = split_point(release_point);
        sync_surface.set_acquire_point(&sync.proxy, acquire_hi, acquire_lo);
        sync_surface.set_release_point(&sync.proxy, release_hi, release_lo);
        // Timing feedback is optional and can arrive long after the compositor
        // releases a buffer (especially for obscured windows). Bound outstanding
        // callbacks independently of render-target ownership.
        let feedback = if self.endpoint.as_mut()
            .ok_or(PresenterError::Configuration("endpoint disappeared"))?
            .track_presentation(object, frame.frame)? {
            Some(self.presentation_time.feedback(&surface, qh)?)
        } else {
            None
        };
        let (dest_w, dest_h) = target.logical_size;
        if let Some(viewport) = task.viewport.as_ref() {
            let dw = i32::try_from(dest_w).unwrap_or(i32::MAX);
            let dh = i32::try_from(dest_h).unwrap_or(i32::MAX);
            viewport.set_destination(dw, dh);
        }
        surface.attach(target.wayland.as_ref(), 0, 0);
        if layers_hidden {
            // Android damage compares app frames, not our constant root backing.
            // Replacing that root with a composed target changes the whole image.
            surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        }
        for damage in &frame.damage {
            surface.damage_buffer(
                i32::try_from(damage.x).unwrap_or(i32::MAX),
                i32::try_from(damage.y).unwrap_or(i32::MAX),
                i32::try_from(damage.width).unwrap_or(i32::MAX),
                i32::try_from(damage.height).unwrap_or(i32::MAX),
            );
        }
        let submitted_nanos = if task.presentation_audit.is_some() { monotonic_timestamp_nanos()? } else { 0 };
        if let Some(trace) = frame_trace.as_mut() { trace.commit(); }
        surface.commit();
        self.activation.mapped(object, &surface);
        target.busy = true;
        task.pending.push_back(PendingFrame {
            frame: frame.frame,
            buffer: frame.buffer,
            release_point,
            submitted_nanos,
            trace: frame_trace,
        });
        if let Some(feedback) = feedback {
            task.presentation_feedback.push(FrameFeedback {
                proxy: feedback,
                frame: frame.frame,
                buffer: frame.buffer,
                submitted_nanos,
            });
        }
        Ok(())
    }

    fn release_ready_frames(&mut self) -> Result<(), PresenterError> {
        let mut ready = Vec::new();
        for (object, task) in &mut self.tasks {
            // Diagnostic observation only: retain the existing release policy.
            // Inspect sampled frames independently so FIFO head blocking is visible.
            for pending in &mut task.pending {
                if let Some(trace) = pending.trace.as_mut() {
                    if !trace.release_observed {
                        if let Some(sync) = task.targets.get(&pending.buffer).and_then(|t| t.sync.as_ref()) {
                            if let Ok(point) = sync.timeline.signalled_point() {
                                if point >= pending.release_point {
                                    trace.release_ready(task.presentation_feedback.iter()
                                        .any(|feedback| feedback.frame == pending.frame));
                                }
                            }
                        }
                    }
                }
            }
            while let Some(pending) = task.pending.front() {
                let sync = task.targets.get(&pending.buffer)
                    .and_then(|target| target.sync.as_ref())
                    .ok_or(PresenterError::Configuration("pending buffer timeline is absent"))?;
                // Only the compositor's real release fence controls reuse.
                // Presentation feedback retains its own bounded frame identity.
                if pending.release_point > sync.timeline.signalled_point()? {
                    break;
                }
                let pending = task.pending.pop_front().expect("front was present");
                if let Some(audit) = task.presentation_audit.as_mut() {
                    audit.released(pending.submitted_nanos, monotonic_timestamp_nanos()?);
                }
                ready.push((*object, pending));
            }
        }
        for (object, pending) in ready {
            self.endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                .finish_release(object, pending.frame)?;
            if let Some(mut trace) = pending.trace { trace.returned(); }
            let (retired, closing) = {
                let task = self
                    .tasks
                    .get_mut(&object)
                    .ok_or(PresenterError::UnknownTask(object))?;
                let target =
                    task.targets
                        .get_mut(&pending.buffer)
                        .ok_or(PresenterError::UnknownBuffer {
                            object,
                            buffer: pending.buffer,
                        })?;
                target.busy = false;
                (target.retired && target.host_owned, task.closing)
            };
            if retired && !closing {
                self.endpoint
                    .as_mut()
                    .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                    .unregister_render_target(object, pending.buffer)?;
                if let Some(target) = self
                    .tasks
                    .get_mut(&object)
                    .and_then(|task| task.targets.remove(&pending.buffer))
                {
                    if let Some(buffer) = target.wayland {
                        buffer.destroy();
                    }
                }
            }
        }
        let finished = self
            .tasks
            .iter()
            .filter_map(|(object, task)| {
                (task.closing && task.pending.is_empty() && task.layers.as_ref().is_none_or(|s|!s.busy())).then_some(*object)
            })
            .collect::<Vec<_>>();
        for object in finished {
            self.remove_task(object);
        }
        Ok(())
    }

    fn remove_task(&mut self, object: TaskObjectId) {
        self.clear_titlebar_buttons(object);
        self.forget_task_input(object);
        self.activation.remove(object);
        if let Some(mut task) = self.tasks.remove(&object) {
            task.titlebar_buttons.clear();
            if let Some(surface) = task.surface() {
                for tool in &mut self.tablet_tools {
                    if tool.surface.as_ref() == Some(surface) {
                        tool.surface = None;
                    }
                }
            }
            if let Some(fractional_scale) = task.fractional_scale.take() {
                fractional_scale.destroy();
            }
            if let Some(viewport) = task.viewport.take() {
                viewport.destroy();
            }
            if let Some(surface) = task.sync_surface.take() {
                surface.destroy();
            }
            for (_, target) in task.targets {
                if let Some(buffer) = target.wayland {
                    buffer.destroy();
                }
            }
        }
        if self.focused == Some(object) {
            self.focused = None;
        }
        self.pointer_contact = self
            .pointer_contact
            .filter(|contact| contact.object != object);
        self.touch_contacts
            .retain(|_, contact| contact.object != object);
        if let Some((obj, _)) = self.titlebar_button_pointer && obj == object {
            self.titlebar_button_pointer = None;
        }
        if let Some((_, obj, _)) = self.titlebar_button_touch && obj == object {
            self.titlebar_button_touch = None;
        }
        if let Some((obj, _)) = self.titlebar_button_tablet && obj == object {
            self.titlebar_button_tablet = None;
        }
        for tool in &mut self.tablet_tools {
            if tool.decoration_down == Some(object) {
                tool.decoration_down = None;
            }
        }
        if self.endpoint.is_some() {
            if let Err(error) = self.update_output_refreshes() { self.fail(&error); }
        }
    }

    fn unmap_task_window(&mut self, object: TaskObjectId) -> Result<(), PresenterError> {
        self.release_task_input(object);
        self.activation.remove(object);
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        let Some(surface) = task.surface().cloned() else {
            return Ok(());
        };

        // An xdg_toplevel close request is terminal for the native window.
        // Unmap it immediately, like any ordinary Wayland client, while the
        // retained timeline/target bookkeeping drains buffers Android had
        // already submitted before its task removal completed.
        surface.attach(None, 0, 0);
        surface.commit();
        for feedback in task.presentation_feedback.drain(..) {
            if let Some(endpoint) = self.endpoint.as_mut() {
                endpoint.forget_presentation(object, feedback.frame);
            }
        }
        if let Some(stream)=task.layers.as_mut() { stream.hide(); }
        if let Some(fractional_scale) = task.fractional_scale.take() {
            fractional_scale.destroy();
        }
        if let Some(viewport) = task.viewport.take() {
            viewport.destroy();
        }
        if let Some(sync_surface) = task.sync_surface.take() {
            sync_surface.destroy();
        }
        task.titlebar_buttons.clear();
        if let Some((obj, _)) = self.titlebar_button_pointer && obj == object {
            self.titlebar_button_pointer = None;
        }
        if let Some((_, obj, _)) = self.titlebar_button_touch && obj == object {
            self.titlebar_button_touch = None;
        }
        if let Some((obj, _)) = self.titlebar_button_tablet && obj == object {
            self.titlebar_button_tablet = None;
        }
        for tool in &mut self.tablet_tools {
            if tool.decoration_down == Some(object) {
                tool.decoration_down = None;
            }
            if let Some(surf) = &tool.surface {
                if surf == &surface {
                    tool.surface = None;
                }
            }
        }
        // Window is reference counted by SCTK. Taking our handle below does
        // not guarantee that WindowInner is dropped here, so relying on Drop
        // can leave the compositor-side toplevel mapped until some unrelated
        // callback releases the final clone. The close request is terminal:
        // destroy the role explicitly and let the remaining surface/buffer
        // objects drain independently.
        if let Some(window) = task.window.as_ref() {
            window.xdg_toplevel().destroy();
        }
        task.window.take();
        task.unmap_requested = false;

        if self.focused == Some(object) {
            self.focused = None;
        }
        self.pointer_contact = self
            .pointer_contact
            .filter(|contact| contact.object != object);
        self.touch_contacts
            .retain(|_, contact| contact.object != object);
        eprintln!(
            "Droidloom destroyed terminal Android xdg_toplevel object={}",
            object.0
        );
        Ok(())
    }

    fn unmap_requested_tasks(&mut self) -> Result<(), PresenterError> {
        let requested = self
            .tasks
            .iter()
            .filter_map(|(object, task)| task.unmap_requested.then_some(*object))
            .collect::<Vec<_>>();
        for object in requested {
            self.unmap_task_window(object)?;
            let finished = self
                .tasks
                .get(&object)
                .is_some_and(|task| task.closing && task.pending.is_empty() && task.layers.as_ref().is_none_or(|s|!s.busy()));
            if finished {
                self.remove_task(object);
            }
        }
        Ok(())
    }

    fn destroy_task(&mut self, object: TaskObjectId) -> Result<(), PresenterError> {
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        task.closing = true;
        task.unmap_requested = true;
        Ok(())
    }

    fn process_action(
        &mut self,
        qh: &QueueHandle<Self>,
        action: EndpointAction,
    ) -> Result<(), PresenterError> {
        match action {
            EndpointAction::RequestActivation { object } => {
                if self.focused != Some(object)
                    && let Some(task) = self.tasks.get(&object).filter(|task| task.accepts_present()) {
                    self.activation.request(qh, object, &task.package);
                }
            }
            EndpointAction::BindLayerStream { object, sockets } => self.bind_layers(object, sockets)?,
            EndpointAction::ClientReady
            | EndpointAction::TimelinesBound { .. }
            | EndpointAction::ConfigureAcknowledged { .. }
            | EndpointAction::SupersededPresentDiscarded { .. }
            | EndpointAction::Pong { .. }
            | EndpointAction::SetFrameRate { .. } => {}
            EndpointAction::SetContentState { object, flags } => {
                self.tasks.get_mut(&object).ok_or(PresenterError::UnknownTask(object))?
                    .content_opaque = flags & droidloom_denial_protocol::content_state::OPAQUE != 0;
            }
            EndpointAction::BindTask { object, task } => {
                if let Some(window) = self.tasks.get_mut(&object) {
                    window.android_task = Some(task.0);
                }
            }
            EndpointAction::CreateTask {
                object,
                display: _,
                package,
            } => self.create_task(qh, object, &package)?,
            EndpointAction::DestroyTask { object } => self.destroy_task(object)?,
            EndpointAction::RegisterBuffer {
                object,
                buffer,
                planes,
            } => self.register_android_buffer(qh, object, buffer, planes)?,
            EndpointAction::UnregisterBuffer { object, buffer } => {
                let target = self
                    .tasks
                    .get_mut(&object)
                    .ok_or(PresenterError::UnknownTask(object))?
                    .targets
                    .remove(&buffer)
                    .ok_or(PresenterError::UnknownBuffer { object, buffer })?;
                if let Some(buffer) = target.wayland {
                    buffer.destroy();
                }
            }
            EndpointAction::Present {
                frame,
                acquire_fence,
            } => self.present(qh, &frame, &acquire_fence)?,
        }
        Ok(())
    }

    fn accept_endpoint(&mut self) -> Result<(), PresenterError> {
        if self.endpoint.is_some() {
            return Ok(());
        }
        match self.listener.accept() {
            Ok(endpoint) => {
                let endpoint = endpoint.with_tablet_input().with_mouse_input();
                let endpoint = if self.activation.supported() {
                    endpoint.with_activation_requests()
                } else { endpoint };
                endpoint.socket().socket().set_nonblocking(true)?;
                self.endpoint = Some(endpoint);
            }
            Err(error) if listener_would_block(&error) => {}
            Err(EndpointListenerError::UnexpectedPeer { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn drain_endpoint(&mut self, qh: &QueueHandle<Self>) -> Result<(), PresenterError> {
        loop {
            let action = match self.endpoint.as_mut() {
                Some(endpoint) => match endpoint.receive_action() {
                    Ok(action) => action,
                    Err(error) if endpoint_would_block(&error) => return Ok(()),
                    Err(error) => {
                        eprintln!("droidloom-wayland: Android endpoint disconnected: {error}");
                        self.disconnect_endpoint();
                        return Ok(());
                    }
                },
                None => return Ok(()),
            };
            self.process_action(qh, action)?;
        }
    }

    fn disconnect_endpoint(&mut self) {
        if let Some(immersion) = self.immersion.take() {
            immersion.release();
        }
        self.pressed_keys.clear();
        self.consumed_keys.clear();
        self.mouse_focus = None;
        self.mouse_buttons.clear();
        self.activation.clear();
        self.text_input.disconnect();
        self.endpoint = None;
        for object in self.tasks.keys().copied().collect::<Vec<_>>() {
            self.remove_task(object);
        }
        self.focused = None;
        self.pointer_contact = None;
        self.touch_contacts.clear();
    }

    fn send_input(
        &mut self,
        object: TaskObjectId,
        event: InputEvent,
    ) -> Result<(), PresenterError> {
        let tablet_trace = match event {
            InputEvent::Tablet { action, tool_id, .. }
                if !matches!(action, TabletAction::Motion | TabletAction::Wheel) =>
            {
                Some((action, tool_id))
            }
            _ => None,
        };
        self.next_input_serial = self
            .next_input_serial
            .checked_add(1)
            .ok_or(PresenterError::Configuration("input serial exhausted"))?;
        let timestamp = monotonic_timestamp_nanos()?;
        if *INPUT_TRACE.get_or_init(|| {
            env::var_os("DROIDLOOM_INPUT_TRACE").as_deref() == Some(std::ffi::OsStr::new("1"))
        })
            && let InputEvent::Key {
                action,
                keycode,
                repeat,
            } = &event
        {
            eprintln!(
                "Droidloom key trace: stage=wayland serial={} object={} action={action:?} scan_code={keycode} repeat={repeat}",
                self.next_input_serial, object.0
            );
        }
        self.endpoint
            .as_ref()
            .ok_or(PresenterError::Configuration("endpoint is absent"))?
            .send_input(object, self.next_input_serial, timestamp, event)?;
        if let Some((action, tool_id)) = tablet_trace {
            eprintln!(
                "Droidloom tablet trace: stage=denial-sent action={action:?} tool={tool_id} task={}",
                object.0
            );
        }
        Ok(())
    }

    /// Release every key and button this task still holds, as a physical
    /// keyboard or mouse would when its window stops receiving input.
    fn release_task_input(&mut self, object: TaskObjectId) {
        let held = self
            .pressed_keys
            .iter()
            .filter_map(|(key, owner)| (*owner == object).then_some(*key))
            .collect::<Vec<_>>();
        for keycode in held {
            self.pressed_keys.remove(&keycode);
            if let Err(error) = self.send_input(
                object,
                InputEvent::Key {
                    action: KeyAction::Up,
                    keycode,
                    repeat: 0,
                },
            ) {
                self.fail(&error);
            }
        }
        if self.mouse_focus == Some(object) {
            self.mouse_focus = None;
            self.mouse_buttons.clear();
            if let Err(error) = self.send_mouse(object, MouseAction::Cancel, (0.0, 0.0), 0, (0.0, 0.0))
            {
                self.fail(&error);
            }
        }
        if self.immersion.as_ref().is_some_and(|immersion| immersion.object == object) {
            self.end_immersion();
        }
    }

    /// Drop routing state for a task that no longer accepts input at all.
    fn forget_task_input(&mut self, object: TaskObjectId) {
        self.pressed_keys.retain(|_, owner| *owner != object);
        if self.mouse_focus == Some(object) {
            self.mouse_focus = None;
            self.mouse_buttons.clear();
        }
        for pad in self.tablet_pads.values_mut() {
            if pad.focus == Some(object) {
                pad.focus = None;
                pad.pressed.clear();
            }
        }
        if self.immersion.as_ref().is_some_and(|immersion| immersion.object == object) {
            self.end_immersion();
        }
    }

    fn send_mouse(
        &mut self,
        object: TaskObjectId,
        action: MouseAction,
        position: (f64, f64),
        button: u32,
        scroll: (f64, f64),
    ) -> Result<(), PresenterError> {
        let (x_fixed, y_fixed) = self.fixed_position(object, position);
        self.send_input(
            object,
            InputEvent::Mouse {
                action,
                x_fixed,
                y_fixed,
                button,
                scroll_x_fixed: fixed_16_16(scroll.0),
                scroll_y_fixed: fixed_16_16(scroll.1),
            },
        )
    }

    fn mouse_input_supported(&self) -> bool {
        self.endpoint.as_ref().is_some_and(DenialEndpoint::supports_mouse_input)
    }

    /// Handle the presenter's own window shortcuts before routing a key.
    /// Returns whether the key was consumed.
    fn window_shortcut(&mut self, qh: &QueueHandle<Self>, keycode: u32) -> bool {
        let Some(object) = self.focused else { return false };
        let modifiers = self.modifiers;
        if keycode == KEY_F11 && !modifiers.logo {
            self.toggle_fullscreen(object);
            return true;
        }
        if keycode == KEY_ESC && !modifiers.logo && !modifiers.ctrl && !modifiers.alt {
            let is_fullscreen = self.tasks.get(&object).is_some_and(|t| t.fullscreen);
            if is_fullscreen {
                self.toggle_fullscreen(object);
                return true;
            }
        }
        if keycode == KEY_B && modifiers.ctrl && modifiers.alt && !modifiers.logo {
            self.toggle_decorations(qh, object);
            return true;
        }
        if keycode == KEY_M && modifiers.ctrl && modifiers.alt && !modifiers.logo {
            if self.immersion.is_some() {
                self.end_immersion();
            } else {
                self.begin_immersion(qh, object);
            }
            return true;
        }
        false
    }

    fn toggle_fullscreen(&mut self, object: TaskObjectId) {
        let Some(task) = self.tasks.get(&object) else { return };
        let Some(window) = task.window.as_ref() else { return };
        if task.fullscreen {
            window.unset_fullscreen();
        } else {
            window.set_fullscreen(None);
        }
    }

    /// Confine the pointer to one Android window and let it receive the
    /// desktop's own shortcuts, as remote-desktop and game clients expect.
    fn begin_immersion(&mut self, qh: &QueueHandle<Self>, object: TaskObjectId) {
        let Some(surface) = self.tasks.get(&object).and_then(TaskWindow::surface).cloned() else {
            return;
        };
        let Some(pointer) = self.pointer.as_ref().map(|themed| themed.pointer().clone()) else {
            eprintln!("Droidloom mouse immersion needs a pointer device");
            return;
        };
        let confined = match self.pointer_constraints.confine_pointer(
            &surface,
            &pointer,
            None,
            zwp_pointer_constraints_v1::Lifetime::Persistent,
            qh,
        ) {
            Ok(confined) => confined,
            Err(error) => {
                eprintln!("Droidloom mouse immersion is unavailable: {error}");
                return;
            }
        };
        let inhibitor = self
            .shortcuts_inhibit_manager
            .as_ref()
            .zip(self.keyboard_seat.as_ref())
            .map(|(manager, seat)| manager.inhibit_shortcuts(&surface, seat, qh, ()));
        eprintln!(
            "Droidloom mouse immersion started object={} shortcuts-inhibited={}",
            object.0,
            inhibitor.is_some()
        );
        self.immersion = Some(Immersion {
            object,
            confined,
            inhibitor,
        });
        self.update_task_title(object);
    }

    fn end_immersion(&mut self) {
        if let Some(immersion) = self.immersion.take() {
            let object = immersion.object;
            eprintln!("Droidloom mouse immersion ended object={}", object.0);
            immersion.release();
            self.update_task_title(object);
        }
    }

    fn send_back_key(&mut self, object: TaskObjectId) {
        if let Err(error) = self.send_input(
            object,
            InputEvent::Key {
                action: KeyAction::Down,
                keycode: KEY_BACK,
                repeat: 0,
            },
        ) {
            self.fail(&error);
            return;
        }
        if let Err(error) = self.send_input(
            object,
            InputEvent::Key {
                action: KeyAction::Up,
                keycode: KEY_BACK,
                repeat: 0,
            },
        ) {
            self.fail(&error);
        }
    }

    fn update_task_title(&mut self, object: TaskObjectId) {
        let is_immersed = self.immersion.as_ref().is_some_and(|imm| imm.object == object);
        let Some(task) = self.tasks.get_mut(&object) else { return; };
        let is_fullscreen = task.fullscreen;
        let title = format_task_title(&task.package, is_immersed, is_fullscreen);
        if let Some(window) = task.window.as_ref() {
            window.set_title(&title);
        }
        if let Some(frame) = task.window_frame.as_mut() {
            frame.set_title(&title);
            if frame.is_dirty() && frame.draw() {
                if let Some(window) = task.window.as_ref() {
                    window.wl_surface().commit();
                }
            }
        }
    }

    fn scroll_steps(axis: &AxisScroll) -> f64 {
        if axis.value120 != 0 {
            f64::from(axis.value120) / 120.0
        } else if axis.discrete != 0 {
            f64::from(axis.discrete)
        } else {
            axis.absolute / CONTINUOUS_SCROLL_PIXELS_PER_STEP
        }
    }

    /// Route a mouse event natively when the peer accepts mouse input.
    fn route_mouse(&mut self, event: &PointerEvent) {
        let object = self.task_for_surface(&event.surface);
        let result = match event.kind {
            PointerEventKind::Enter { .. } => match object {
                Some(object) => {
                    self.mouse_focus = Some(object);
                    self.send_mouse(object, MouseAction::Enter, event.position, 0, (0.0, 0.0))
                }
                None => Ok(()),
            },
            PointerEventKind::Leave { .. } => {
                let Some(focus) = self.mouse_focus.take() else { return };
                if object.is_some_and(|object| object != focus) {
                    self.mouse_focus = Some(focus);
                    return;
                }
                // A window that loses the pointer while a button is down has
                // no way to receive the release, so hand Android a cancel.
                if self.mouse_buttons.is_empty() {
                    self.send_mouse(focus, MouseAction::Leave, event.position, 0, (0.0, 0.0))
                } else {
                    self.mouse_buttons.clear();
                    self.send_mouse(focus, MouseAction::Cancel, event.position, 0, (0.0, 0.0))
                }
            }
            PointerEventKind::Motion { .. } => match self.mouse_focus {
                Some(focus) => {
                    if event.position.1 <= 4.0 {
                        let is_fs = self.tasks.get(&focus).is_some_and(|t| t.fullscreen || t.decorations_hidden);
                        if is_fs {
                            self.reveal_fullscreen_controls(focus, gesture::FULLSCREEN_REVEAL_DURATION);
                        }
                    }
                    self.send_mouse(focus, MouseAction::Motion, event.position, 0, (0.0, 0.0))
                }
                None => Ok(()),
            },
            PointerEventKind::Press { button, .. } => {
                let Some(focus) = self.mouse_focus.or(object) else { return };
                self.mouse_focus = Some(focus);
                // The stream can miss a release while the compositor owns the
                // grab; resynchronize instead of swallowing the new press.
                if self.mouse_buttons.insert(button) {
                    self.text_input.note_touch();
                    self.send_mouse(focus, MouseAction::ButtonPress, event.position, button,
                        (0.0, 0.0))
                } else {
                    self.send_mouse(focus, MouseAction::Cancel, event.position, 0, (0.0, 0.0))
                        .and_then(|()| {
                            self.mouse_buttons.insert(button);
                            self.send_mouse(focus, MouseAction::ButtonPress, event.position,
                                button, (0.0, 0.0))
                        })
                }
            }
            PointerEventKind::Release { button, .. } => {
                // Clear the tracked press whether or not a target is known, so
                // a lost focus cannot wedge the button down forever.
                let pressed = self.mouse_buttons.remove(&button);
                match self.mouse_focus.or(object) {
                    Some(focus) if pressed => self.send_mouse(
                        focus,
                        MouseAction::ButtonRelease,
                        event.position,
                        button,
                        (0.0, 0.0),
                    ),
                    _ => Ok(()),
                }
            }
            PointerEventKind::Axis { ref horizontal, ref vertical, .. } => match self.mouse_focus {
                Some(focus) => {
                    let scroll = (Self::scroll_steps(horizontal), Self::scroll_steps(vertical));
                    if scroll == (0.0, 0.0) {
                        Ok(())
                    } else {
                        self.send_mouse(focus, MouseAction::Scroll, event.position, 0, scroll)
                    }
                }
                None => Ok(()),
            },
        };
        if *INPUT_TRACE.get_or_init(|| {
            env::var_os("DROIDLOOM_INPUT_TRACE").as_deref() == Some(std::ffi::OsStr::new("1"))
        }) {
            eprintln!(
                "Droidloom mouse trace: object={:?} focus={:?} buttons={:?} event={:?}",
                object.map(|object| object.0), self.mouse_focus.map(|object| object.0),
                self.mouse_buttons, event.kind
            );
        }
        if let Err(error) = result {
            self.fail(&error);
        }
    }

    /// Handle a tablet tool acting on a window decoration instead of task
    /// content, so a pen moves, resizes and closes windows like a pointer.
    /// Returns whether the action was consumed.
    fn tablet_decoration(&mut self, tool_id: u32, action: TabletAction) -> bool {
        let Some(tool) = self.tablet_tools.iter().find(|tool| tool.tool_id == tool_id) else {
            return false;
        };
        let Some(surface) = tool.surface.clone() else { return false };
        if self.task_for_surface(&surface).is_some() {
            return false;
        }
        if let Some((object, kind)) = self.task_and_button_for_surface(&surface) {
            match action {
                TabletAction::Down => {
                    self.titlebar_button_tablet = Some((object, kind));
                    self.update_titlebar_button_state(object, kind, TitlebarButtonState::Pressed);
                    return true;
                }
                TabletAction::Up => {
                    if let Some((down_object, down_kind)) = self.titlebar_button_tablet.take() {
                        self.update_titlebar_button_state(down_object, down_kind, TitlebarButtonState::Normal);
                        if down_object == object && down_kind == kind {
                            self.handle_titlebar_button_click(object, kind);
                        }
                        return true;
                    }
                }
                TabletAction::ProximityOut | TabletAction::Cancel => {
                    if let Some((down_object, down_kind)) = self.titlebar_button_tablet.take() {
                        self.update_titlebar_button_state(down_object, down_kind, TitlebarButtonState::Normal);
                    }
                }
                _ => {}
            }
            return true;
        }
        let position = (f64::from(tool.x_fixed) / 65_536.0, f64::from(tool.y_fixed) / 65_536.0);
        let serial = tool.down_serial;
        let seat = self
            .tablet_tool_seats
            .get(&tool_id)
            .and_then(|tablet_seat| {
                self.tablet_seats
                    .iter()
                    .find(|(_, candidate)| candidate.id().protocol_id() == *tablet_seat)
            })
            .map(|(seat, _)| seat.clone());
        let elapsed = self.start_time.elapsed();
        match action {
            TabletAction::Down => {
                let Some((object, _)) =
                    self.decoration_pointer_task(&surface, position.0, position.1)
                else {
                    return false;
                };
                self.tablet_tool_mut(tool_id).decoration_down = Some(object);
                let frame_action = self
                    .tasks
                    .get_mut(&object)
                    .and_then(|task| task.window_frame.as_mut())
                    .and_then(|frame| frame.on_click(elapsed, FrameClick::Normal, true));
                if let (Some(frame_action), Some(seat)) = (frame_action, seat) {
                    self.frame_action(&seat, object, serial, frame_action);
                }
            }
            TabletAction::Up => {
                let Some(object) = self.tablet_tool_mut(tool_id).decoration_down.take() else {
                    return true;
                };
                let frame_action = self
                    .tasks
                    .get_mut(&object)
                    .and_then(|task| task.window_frame.as_mut())
                    .and_then(|frame| frame.on_click(elapsed, FrameClick::Normal, false));
                // Resize is started by the press; the release only finishes it.
                if let (Some(frame_action), Some(seat)) = (frame_action, seat)
                    && !matches!(frame_action, FrameAction::Resize(_))
                {
                    self.frame_action(&seat, object, serial, frame_action);
                }
            }
            TabletAction::ProximityOut | TabletAction::Cancel => {
                self.tablet_tool_mut(tool_id).decoration_down = None;
                self.decoration_pointer_left();
            }
            _ => {
                let _ = self.decoration_pointer_task(&surface, position.0, position.1);
            }
        }
        true
    }

    fn update_text_input(&mut self) {
        let task = self
            .focused
            .filter(|object| Some(*object) == self.text_input.entered)
            .and_then(|object| self.tasks.get(&object))
            .filter(|task| !task.closing && task.window.is_some())
            .and_then(|task| task.android_task);
        self.text_input.update(task);
    }

    fn pump_text_input(&mut self) {
        if let Err(error) = self.text_input.pump() {
            eprintln!("Droidloom text-input disconnected: {error}");
            self.text_input.disconnect();
        }
        self.update_text_input();
    }

    fn set_focus(&mut self, object: Option<TaskObjectId>) -> Result<(), PresenterError> {
        if self.focused == object {
            return Ok(());
        }
        if let Some(previous) = self.focused
            && let Some(task) = self.tasks.get_mut(&previous)
        {
            task.focused = false;
            if let Some(endpoint) = self.endpoint.as_ref() {
                endpoint.send_visibility(previous, Visibility::Visible, false)?;
            }
        }
        self.focused = object;
        self.update_text_input();
        if let Some(object) = object
            && let Some(task) = self.tasks.get_mut(&object)
        {
            task.focused = true;
            if let Some(endpoint) = self.endpoint.as_ref() {
                endpoint.send_visibility(object, Visibility::Visible, true)?;
            }
        }
        Ok(())
    }

    fn fixed_position(&self, object: TaskObjectId, position: (f64, f64)) -> (i32, i32) {
        let Some(task) = self.tasks.get(&object) else {
            return (0, 0);
        };
        let Some((logical_width, logical_height)) = task.logical_size else {
            return (0, 0);
        };
        let x = position
            .0
            .clamp(0.0, f64::from(logical_width).max(1.0) - 0.001);
        let y = position
            .1
            .clamp(0.0, f64::from(logical_height).max(1.0) - 0.001);
        (fixed_16_16(x), fixed_16_16(y))
    }

    fn decoration_pointer_task(
        &mut self,
        surface: &wl_surface::WlSurface,
        x: f64,
        y: f64,
    ) -> Option<(TaskObjectId, CursorIcon)> {
        let elapsed = self.start_time.elapsed();
        self.tasks.iter_mut().find_map(|(object, task)| {
            if task.decorations_hidden || task.fullscreen {
                return None;
            }
            let frame = task.window_frame.as_mut()?;
            let cursor = frame.click_point_moved(elapsed, &surface.id(), x, y)?;
            if frame.is_dirty() && frame.draw() {
                if let Some(window) = task.window.as_ref() {
                    window.wl_surface().commit();
                }
            }
            Some((*object, cursor))
        })
    }

    fn decoration_pointer_left(&mut self) {
        for task in self.tasks.values_mut() {
            if let Some(frame) = task.window_frame.as_mut() {
                frame.click_point_left();
            }
        }
    }

    fn frame_action(
        &mut self,
        seat: &wl_seat::WlSeat,
        object: TaskObjectId,
        serial: u32,
        action: FrameAction,
    ) {
        match action {
            FrameAction::Close => {
                if let Some(endpoint) = self.endpoint.as_ref() {
                    match endpoint.send_close(object) {
                        Ok(()) => {
                            if let Some(task) = self.tasks.get_mut(&object) {
                                task.unmap_requested = true;
                            }
                        }
                        Err(error) => self.fail(&error),
                    }
                }
            }
            FrameAction::Minimize => {
                if let Some(window) = self.tasks.get(&object).and_then(|task| task.window.as_ref()) {
                    window.set_minimized();
                }
            }
            FrameAction::Maximize => {
                if let Some(window) = self.tasks.get(&object).and_then(|task| task.window.as_ref()) {
                    window.set_maximized();
                }
            }
            FrameAction::UnMaximize => {
                if let Some(window) = self.tasks.get(&object).and_then(|task| task.window.as_ref()) {
                    window.unset_maximized();
                }
            }
            FrameAction::ShowMenu(x, y) => {
                if let Some(window) = self.tasks.get(&object).and_then(|task| task.window.as_ref()) {
                    window.show_window_menu(seat, serial, (x, y));
                }
            }
            FrameAction::Move => {
                if let Some(window) = self.tasks.get(&object).and_then(|task| task.window.as_ref()) {
                    window.move_(seat, serial);
                }
            }
            FrameAction::Resize(edge) => {
                let edge = match edge {
                    ResizeEdge::None => XdgResizeEdge::None,
                    ResizeEdge::Top => XdgResizeEdge::Top,
                    ResizeEdge::Bottom => XdgResizeEdge::Bottom,
                    ResizeEdge::Left => XdgResizeEdge::Left,
                    ResizeEdge::TopLeft => XdgResizeEdge::TopLeft,
                    ResizeEdge::BottomLeft => XdgResizeEdge::BottomLeft,
                    ResizeEdge::Right => XdgResizeEdge::Right,
                    ResizeEdge::TopRight => XdgResizeEdge::TopRight,
                    ResizeEdge::BottomRight => XdgResizeEdge::BottomRight,
                    _ => return,
                };
                if let Some(window) = self.tasks.get(&object).and_then(|task| task.window.as_ref()) {
                    window.resize(seat, serial, edge);
                }
            }
            _ => {}
        }
    }

    fn fail(&mut self, error: &dyn ToString) {
        self.fatal = Some(error.to_string());
    }

    /// Route one pad button to the task under the pad's focus as a generic
    /// gamepad-style button key, so Android applications and key layouts can
    /// bind pen-side and pad buttons the way they bind them on a tablet.
    fn pad_button(&mut self, pad_id: u32, button: u32, pressed: bool) {
        if button >= PAD_BUTTON_COUNT {
            return;
        }
        let Some(pad) = self.tablet_pads.get_mut(&pad_id) else { return };
        let target = pad.focus.or(self.focused);
        let Some(object) = target else { return };
        let changed = if pressed { pad.pressed.insert(button) } else { pad.pressed.remove(&button) };
        if !changed {
            return;
        }
        if pressed {
            pad.focus = Some(object);
        }
        eprintln!(
            "Droidloom tablet trace: stage=pad-button pad={pad_id} button={button} pressed={pressed} task={}",
            object.0
        );
        if let Err(error) = self.send_input(
            object,
            InputEvent::Key {
                action: if pressed { KeyAction::Down } else { KeyAction::Up },
                keycode: PAD_BUTTON_BASE + button,
                repeat: 0,
            },
        ) {
            self.fail(&error);
        }
    }

    fn release_pad_buttons(&mut self, pad_id: u32) {
        let Some(pad) = self.tablet_pads.get(&pad_id) else { return };
        for button in pad.pressed.iter().copied().collect::<Vec<_>>() {
            self.pad_button(pad_id, button, false);
        }
        if let Some(pad) = self.tablet_pads.get_mut(&pad_id) {
            pad.focus = None;
        }
    }

    fn destroy_tablet_pad(&mut self, pad_id: u32) {
        if let Some(pad) = self.tablet_pads.remove(&pad_id) {
            for child in pad.children.into_iter().rev() {
                child.destroy();
            }
            pad.proxy.destroy();
        }
        self.tablet_pad_groups.retain(|_, owner| *owner != pad_id);
    }

    fn tablet_tool_mut(&mut self, tool_id: u32) -> &mut TabletToolState {
        if let Some(index) = self.tablet_tools.iter().position(|tool| tool.tool_id == tool_id) {
            return &mut self.tablet_tools[index];
        }
        self.tablet_tools.push(TabletToolState::new(tool_id));
        self.tablet_tools.last_mut().expect("tablet tool was inserted")
    }

    fn tablet_tool_snapshot(
        &self,
        tool_id: u32,
    ) -> Option<(
        Option<wl_surface::WlSurface>,
        TabletToolType,
        i32,
        i32,
        u16,
        u16,
        i16,
        i16,
        i16,
        i16,
        i32,
        i32,
        u8,
    )> {
        let tool = self.tablet_tools.iter().find(|tool| tool.tool_id == tool_id)?;
        if !tool.supported {
            return None;
        }
        Some((
            tool.surface.clone(),
            tool.tool_type,
            tool.x_fixed,
            tool.y_fixed,
            tool.pressure,
            tool.distance,
            tool.tilt_x_tenths,
            tool.tilt_y_tenths,
            tool.rotation_tenths,
            tool.wheel_clicks,
            tool.slider,
            tool.wheel_degrees_fixed,
            tool.axis_flags,
        ))
    }

    fn send_tablet_event(
        &mut self,
        tool_id: u32,
        action: TabletAction,
        button: u32,
    ) -> Result<(), PresenterError> {
        let Some((
            surface,
            tool_type,
            x_fixed,
            y_fixed,
            pressure,
            distance,
            tilt_x_tenths,
            tilt_y_tenths,
            rotation_tenths,
            wheel_clicks,
            slider,
            wheel_degrees_fixed,
            axis_flags,
        )) = self.tablet_tool_snapshot(tool_id)
        else {
            if !matches!(action, TabletAction::Motion | TabletAction::Wheel) {
                eprintln!(
                    "Droidloom tablet trace: stage=drop reason=unknown-tool action={action:?} tool={tool_id}"
                );
            }
            return Ok(());
        };
        if !self.endpoint.as_ref().is_some_and(|endpoint| endpoint.supports_tablet_input()) {
            if !matches!(action, TabletAction::Motion | TabletAction::Wheel) {
                eprintln!(
                    "Droidloom tablet trace: stage=drop reason=peer-capability action={action:?} tool={tool_id}"
                );
            }
            return Ok(());
        }
        let target_object = surface
            .as_ref()
            .and_then(|s| self.task_for_surface(s))
            .or_else(|| {
                if matches!(action, TabletAction::ButtonPress | TabletAction::ButtonRelease) {
                    self.focused
                } else {
                    None
                }
            });
        let Some(object) = target_object else {
            if !matches!(action, TabletAction::Motion | TabletAction::Wheel) {
                eprintln!(
                    "Droidloom tablet trace: stage=drop reason=no-target action={action:?} tool={tool_id}"
                );
            }
            return Ok(());
        };
        if !self.tasks.contains_key(&object) {
            return Ok(());
        }
        if !matches!(action, TabletAction::Motion | TabletAction::Wheel) {
            eprintln!(
                "Droidloom tablet trace: stage=route action={action:?} tool={tool_id} task={}",
                object.0
            );
        }
        self.send_input(
            object,
            InputEvent::Tablet {
                action,
                tool_id,
                tool_type,
                x_fixed,
                y_fixed,
                pressure,
                distance,
                tilt_x_tenths,
                tilt_y_tenths,
                rotation_tenths,
                wheel_clicks,
                slider,
                wheel_degrees_fixed,
                button,
                axis_flags,
            },
        )
    }

    fn flush_tablet_axes(&mut self, tool_id: u32) -> Result<(), PresenterError> {
        let tool = self.tablet_tool_mut(tool_id);
        let actions = std::mem::take(&mut tool.pending_actions);
        let dirty = std::mem::take(&mut tool.dirty_axes);
        // All axes in a tablet frame describe the same sample, including down/up.
        if actions.is_empty() && dirty && !self.tablet_decoration(tool_id, TabletAction::Motion) {
            self.send_tablet_event(tool_id, TabletAction::Motion, 0)?;
        }
        for (action, button) in actions {
            if !self.tablet_decoration(tool_id, action) {
                self.send_tablet_event(tool_id, action, button)?;
            }
            if matches!(action, TabletAction::ProximityOut | TabletAction::Cancel) {
                self.tablet_tool_mut(tool_id).surface = None;
            }
        }
        let tool = self.tablet_tool_mut(tool_id);
        tool.wheel_clicks = 0;
        tool.wheel_degrees_fixed = 0;
        Ok(())
    }

    fn fixed_angle_tenths(value: f64) -> i16 {
        (value * 10.0).round().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        surface.set_buffer_scale(1);
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
    }

    fn surface_enter(
        &mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface, output: &wl_output::WlOutput,
    ) {
        let Some(object) = self.task_for_surface(surface) else { return; };
        let task = self.tasks.get_mut(&object).expect("surface has a task");
        if !task.entered_outputs.contains(output) { task.entered_outputs.push(output.clone()); }
        if let Err(error) = self.update_task_refresh(object) { self.fail(&error); }
    }

    fn surface_leave(
        &mut self, _conn: &Connection, _qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface, output: &wl_output::WlOutput,
    ) {
        let Some(object) = self.task_for_surface(surface) else { return; };
        let task = self.tasks.get_mut(&object).expect("surface has a task");
        task.entered_outputs.retain(|entered| entered != output);
        if task.presentation_output.as_ref() == Some(output) { task.presentation_output = None; }
        if let Err(error) = self.update_task_refresh(object) { self.fail(&error); }
    }
}

impl Dispatch<WpFractionalScaleV1, TaskObjectId> for App {
    fn event(
        state: &mut Self,
        _proxy: &WpFractionalScaleV1,
        event: wp_fractional_scale_v1::Event,
        object: &TaskObjectId,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wp_fractional_scale_v1::Event::PreferredScale { scale } = event else {
            return;
        };
        if scale == 0 {
            state.fail(&"compositor advertised a zero fractional scale");
            return;
        }
        let Some(task) = state.tasks.get_mut(object) else {
            return;
        };
        if task.preferred_scale_120 == scale {
            return;
        }
        task.preferred_scale_120 = scale;
        let Some((width, height)) = task.logical_size else {
            // Scale is part of resolving the first buffer extent. Do not
            // allocate an observable provisional target before XDG configures
            // the logical window.
            return;
        };
        if let Err(error) = state.configure_task(qh, *object, width, height) {
            state.fail(&error);
        }
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, window: &Window) {
        let object = self.tasks.iter().find_map(|(object, task)| {
            task.window
                .as_ref()
                .is_some_and(|candidate| candidate.wl_surface() == window.wl_surface())
                .then_some(*object)
        });
        if let Some(object) = object
            && let Some(endpoint) = self.endpoint.as_ref()
        {
            match endpoint.send_close(object) {
                Ok(()) => {
                    if let Some(task) = self.tasks.get_mut(&object) {
                        task.unmap_requested = true;
                    }
                }
                Err(error) => self.fail(&error),
            }
        }
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        let object = self.tasks.iter().find_map(|(object, task)| {
            task.window
                .as_ref()
                .is_some_and(|candidate| candidate.wl_surface() == window.wl_surface())
                .then_some(*object)
        });
        let Some(object) = object else {
            return;
        };
        let (package, previous, headless, was_fullscreen, was_maximized, saved_floating) =
            self.tasks.get(&object).map_or_else(
                || (String::new(), None, false, false, false, None),
                |task| {
                    (
                        task.package.clone(),
                        task.logical_size,
                        task.headless(),
                        task.fullscreen,
                        task.maximized,
                        task.floating_size,
                    )
                },
            );

        let is_fullscreen = configure.is_fullscreen();
        let is_maximized = configure.is_maximized();
        let was_floating = !was_fullscreen && !was_maximized;
        let is_floating = !is_fullscreen && !is_maximized;

        let mut floating_size = if was_floating && (is_fullscreen || is_maximized) {
            previous
                .and_then(|(w, h)| LogicalSize::new(w, h).ok())
                .or(saved_floating)
        } else {
            saved_floating
        };

        if let Some(task) = self.tasks.get_mut(&object) {
            task.fullscreen = is_fullscreen;
            task.maximized = is_maximized;
            task.floating_size = floating_size;
        }

        if was_fullscreen != is_fullscreen {
            self.clear_titlebar_buttons(object);
        }

        let (requested_width, requested_height) =
            match self.content_configure_size(qh, object, &configure) {
                Ok((width, height)) => (width.map(NonZeroU32::get), height.map(NonZeroU32::get)),
                Err(error) => {
                    self.fail(&error);
                    return;
                }
            };

        let bounds = configure
            .suggested_bounds
            .and_then(|(width, height)| LogicalSize::new(width, height).ok());

        let size = if is_floating {
            match (requested_width, requested_height) {
                (Some(w), Some(h)) => {
                    let s = LogicalSize { width: w, height: h };
                    floating_size = Some(s);
                    s
                }
                _ => floating_size
                    .map(|saved| saved.clamped_to(bounds))
                    .unwrap_or_else(|| {
                        let s = self.window_policy.resolve_initial(
                            &package,
                            requested_width,
                            requested_height,
                            bounds,
                            self.host_logical_canvas_size(),
                        );
                        floating_size = Some(s);
                        s
                    }),
            }
        } else {
            previous.map_or_else(
                || {
                    self.window_policy.resolve_initial(
                        &package,
                        requested_width,
                        requested_height,
                        bounds,
                        self.host_logical_canvas_size(),
                    )
                },
                |(width, height)| LogicalSize {
                    width: requested_width.unwrap_or(width),
                    height: requested_height.unwrap_or(height),
                },
            )
        };

        if let Some(task) = self.tasks.get_mut(&object) {
            task.floating_size = floating_size;
        }

        if let Err(error) = self.configure_task(qh, object, size.width, size.height) {
            self.fail(&error);
        } else if !headless
            && previous.is_some()
            && is_floating
            && was_floating
            && !configure.is_resizing()
            && let Err(error) = self.window_policy.remember(&package, size)
        {
            // Persistence failure is non-fatal to an otherwise valid frame.
            eprintln!("Droidloom could not remember {package} window size: {error}");
        }
    }
}

impl DmabufHandler for App {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _proxy: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        feedback: DmabufFeedback,
    ) {
        self.feedback = Some(feedback);
    }

    fn created(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        buffer: wl_buffer::WlBuffer,
    ) {
        self.layer_imported(params, Some(buffer));
    }

    fn failed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    ) {
        if self.layer_imported(params, None) { return; }
        self.fail(&"compositor rejected a DMA-BUF import");
    }

    fn released(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _buffer: &wl_buffer::WlBuffer,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        if let Err(error) = self.reconfigure_bootstrap(qh).and_then(|()| self.update_output_refreshes()) {
            self.fail(&error);
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        if let Err(error) = self.reconfigure_bootstrap(qh).and_then(|()| self.update_output_refreshes()) {
            self.fail(&error);
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        for task in self.tasks.values_mut() {
            task.entered_outputs.retain(|entered| entered != &output);
            if task.presentation_output.as_ref() == Some(&output) { task.presentation_output = None; }
        }
        if let Err(error) = self.reconfigure_bootstrap(qh).and_then(|()| self.update_output_refreshes()) {
            self.fail(&error);
        }
    }
}

impl PresentationTimeHandler for App {
    fn presentation_time_state(&mut self) -> &mut PresentationTimeState {
        &mut self.presentation_time
    }

    fn presented(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        feedback: &WpPresentationFeedback,
        surface: &wl_surface::WlSurface,
        outputs: Vec<wl_output::WlOutput>,
        time: PresentTime,
        refresh: u32,
        sequence: u64,
        flags: WEnum<wp_presentation_feedback::Kind>,
    ) {
        let Some(object) = self.task_for_surface(surface) else {
            return;
        };
        let task = self.tasks.get_mut(&object).expect("surface has a task");
        // Presentation feedback identifies the timing output for overlapping
        // surfaces. Ignore late feedback from an output already left.
        if let Some(output) = outputs.into_iter().find(|output| task.entered_outputs.contains(output)) {
            if task.presentation_output.as_ref() != Some(&output) {
                task.presentation_output = Some(output);
                if let Err(error) = self.update_task_refresh(object) {
                    self.fail(&error);
                    return;
                }
            }
        }
        let result = (|| {
            if time.clk_id != u32::try_from(libc::CLOCK_MONOTONIC).unwrap_or_default() {
                return Err(PresenterError::Configuration(
                    "presentation clock is not CLOCK_MONOTONIC",
                ));
            }
            let (completed, advertised_refresh) = {
                let task = self
                    .tasks
                    .get_mut(&object)
                    .ok_or(PresenterError::UnknownTask(object))?;
                let index = task
                    .presentation_feedback
                    .iter()
                    .position(|candidate| candidate.proxy == *feedback)
                    .ok_or(PresenterError::Configuration(
                        "presentation feedback has no submitted frame",
                    ))?;
                let completed = task.presentation_feedback.swap_remove(index);
                (completed, task.refresh_millihz)
            };
            let frame = completed.frame;
            let timestamp_nanos = presentation_timestamp_nanos(time.tv_sec, time.tv_nsec)?;
            let task = self.tasks.get_mut(&object).expect("task was just checked");
            if droidloom_syncobj::frame_trace::sampled(frame.0) {
                droidloom_syncobj::frame_trace::event("presented", object.0, frame.0,
                    completed.buffer.0, &[("display_ns", timestamp_nanos),
                        ("refresh_ns", u64::from(refresh)), ("display_sequence", sequence)]);
            }
            if let Some(audit) = task.presentation_audit.as_mut() {
                audit.presented(object.0, completed.submitted_nanos, timestamp_nanos,
                    monotonic_timestamp_nanos()?, task.pending.len());
            }
            let refresh_period_nanos = presentation_refresh_period(refresh, advertised_refresh)?;
            let protocol_flags = if matches!(
                flags,
                WEnum::Value(kind) if kind.contains(wp_presentation_feedback::Kind::ZeroCopy)
            ) {
                presentation_flag::DISPLAYED | presentation_flag::DIRECT_SCANOUT
            } else {
                presentation_flag::DISPLAYED | presentation_flag::COMPOSITED
            };
            self.endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint is absent"))?
                .send_presented(
                    object,
                    frame,
                    timestamp_nanos,
                    refresh_period_nanos,
                    sequence,
                    protocol_flags,
                )?;
            Ok(())
        })();
        if let Err(error) = result {
            self.fail(&error);
        }
    }

    fn discarded(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        feedback: &WpPresentationFeedback,
        surface: &wl_surface::WlSurface,
    ) {
        let Some(object) = self.task_for_surface(surface) else {
            return;
        };
        let Some(task) = self.tasks.get_mut(&object) else {
            return;
        };
        if let Some(index) = task
            .presentation_feedback
            .iter()
            .position(|candidate| candidate.proxy == *feedback)
        {
            let completed = task.presentation_feedback.swap_remove(index);
            let frame = completed.frame;
            if droidloom_syncobj::frame_trace::sampled(frame.0) {
                droidloom_syncobj::frame_trace::event("discarded", object.0, frame.0,
                    completed.buffer.0, &[]);
            }
            if let Some(endpoint) = self.endpoint.as_mut() {
                endpoint.forget_presentation(object, frame);
            }
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.bind_tablet_seat(seat, qh);
    }

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.clipboard.seat(&seat, qh);
            if let Some(manager) = &self.text_input.manager {
                self.text_input.resource = Some(manager.get_text_input(&seat, qh, ()));
                self.text_input.attach_panel_feedback(qh);
            }
        }
        let result = match capability {
            Capability::Keyboard if self.keyboard.is_none() => self
                .seat_state
                .get_keyboard(qh, &seat, None)
                .map(|keyboard| {
                    self.keyboard = Some(keyboard);
                    self.keyboard_seat = Some(seat.clone());
                })
                .map_err(|error| error.to_string()),
            Capability::Pointer if self.pointer.is_none() => {
                let cursor_surface = self.compositor.create_surface(qh);
                self.seat_state
                    .get_pointer_with_theme(
                        qh, &seat, self.shm_state.wl_shm(), cursor_surface,
                        ThemeSpec::default(),
                    )
                    .map(|pointer| self.pointer = Some(pointer))
                    .map_err(|error| error.to_string())
            },
            Capability::Touch if self.touch.is_none() => self
                .seat_state
                .get_touch(qh, &seat)
                .map(|touch| self.touch = Some(touch))
                .map_err(|error| error.to_string()),
            _ => Ok(()),
        };
        if let Err(error) = result {
            self.fail(&error);
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard => {
                self.keyboard = None;
                self.keyboard_seat = None;
                self.text_input.remove_resource();
            }
            Capability::Pointer => {
                self.pointer = None;
                self.cursor_icon = None;
                self.pointer_contact = None;
            }
            Capability::Touch => {
                self.touch = None;
                self.touch_contacts.clear();
                self.decoration_touch = None;
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.text_input.remove_resource();
        self.keyboard = None;
        self.keyboard_seat = None;
        self.pointer = None;
        self.cursor_icon = None;
        self.touch = None;
        self.pointer_contact = None;
        self.touch_contacts.clear();
        self.decoration_touch = None;

        let Some(index) = self.tablet_seats.iter().position(|(candidate, _)| *candidate == seat) else {
            return;
        };
        let (_, tablet_seat) = self.tablet_seats.remove(index);
        let tablet_seat_id = tablet_seat.id().protocol_id();
        let tool_ids = self.tablet_tool_seats.iter()
            .filter_map(|(tool_id, owner)| (*owner == tablet_seat_id).then_some(*tool_id))
            .collect::<Vec<_>>();
        let tablet_ids = self.tablet_proxies.iter()
            .filter_map(|(id, (owner, _))| (*owner == tablet_seat_id).then_some(*id))
            .collect::<Vec<_>>();
        let pad_ids = self.tablet_pads.iter()
            .filter_map(|(id, pad)| (pad.tablet_seat_id == tablet_seat_id).then_some(*id))
            .collect::<Vec<_>>();
        for tool_id in tool_ids {
            if let Err(error) = self.flush_tablet_axes(tool_id) {
                self.fail(&error);
            }
            // Cancel while the last proximity surface is still known, so the
            // Android side releases any active tool route before it is dropped.
            if self.tablet_tool_snapshot(tool_id).is_some_and(|snapshot| snapshot.0.is_some()) {
                if let Err(error) = self.send_tablet_event(tool_id, TabletAction::Cancel, 0) {
                    self.fail(&error);
                }
            }
            if let Some(proxy) = self.tablet_tool_proxies.remove(&tool_id) {
                proxy.destroy();
            }
            self.tablet_tool_seats.remove(&tool_id);
            self.tablet_tools.retain(|tool| tool.tool_id != tool_id);
        }
        for pad_id in pad_ids {
            self.destroy_tablet_pad(pad_id);
        }
        for tablet_id in tablet_ids {
            if let Some((_, proxy)) = self.tablet_proxies.remove(&tablet_id) {
                proxy.destroy();
            }
        }
        tablet_seat.destroy();
    }
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
        _raw: &[u32],
        _keysyms: &[smithay_client_toolkit::seat::keyboard::Keysym],
    ) {
        self.clipboard.focus(true);
        self.clipboard.serial(serial);
        let object = self.task_for_surface(surface);
        if let Err(error) = self.set_focus(object) {
            self.fail(&error);
        }
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
        self.clipboard.focus(false);
        if let Some(object) = self.task_for_surface(surface) {
            // wl_keyboard.leave ends every press this surface received, so
            // Android must see the matching releases.
            let held = self
                .pressed_keys
                .iter()
                .filter_map(|(key, owner)| (*owner == object).then_some(*key))
                .collect::<Vec<_>>();
            for keycode in held {
                self.pressed_keys.remove(&keycode);
                if let Err(error) = self.send_input(
                    object,
                    InputEvent::Key {
                        action: KeyAction::Up,
                        keycode,
                        repeat: 0,
                    },
                ) {
                    self.fail(&error);
                }
            }
            if self.immersion.as_ref().is_some_and(|immersion| immersion.object == object) {
                self.end_immersion();
            }
        }
        if self.focused == self.task_for_surface(surface)
            && let Err(error) = self.set_focus(None)
        {
            self.fail(&error);
        }
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.clipboard.serial(serial);
        if let Some(data) = _keyboard.data::<smithay_client_toolkit::seat::keyboard::KeyboardData<App>>()
            && let Some(surface) = self.focused.and_then(|id| self.tasks.get(&id)).and_then(TaskWindow::surface) {
            self.activation.input(serial, data.seat(), surface);
        }
        if self.window_shortcut(qh, event.raw_code) {
            self.consumed_keys.insert(event.raw_code);
            return;
        }
        if let Some(object) = self.focused {
            // A key already held by another task is released there first, so
            // every Android task sees balanced transitions.
            if let Some(previous) = self.pressed_keys.insert(event.raw_code, object)
                && previous != object
                && let Err(error) = self.send_input(
                    previous,
                    InputEvent::Key {
                        action: KeyAction::Up,
                        keycode: event.raw_code,
                        repeat: 0,
                    },
                )
            {
                self.fail(&error);
            }
            if let Err(error) = self.send_input(
                object,
                InputEvent::Key {
                    action: KeyAction::Down,
                    keycode: event.raw_code,
                    repeat: 0,
                },
            ) {
                self.fail(&error);
            }
        }
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.clipboard.serial(serial);
        if let Some(object) = self.focused
            && self.pressed_keys.get(&event.raw_code) == Some(&object)
            && let Err(error) = self.send_input(
                object,
                InputEvent::Key {
                    action: KeyAction::Down,
                    keycode: event.raw_code,
                    repeat: 1,
                },
            )
        {
            self.fail(&error);
        }
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.clipboard.serial(serial);
        let consumed = self.consumed_keys.remove(&event.raw_code);
        let previous = self.pressed_keys.remove(&event.raw_code);
        if matches!(event.raw_code, KEY_LEFTCTRL | KEY_RIGHTCTRL)
            && !self.pressed_keys.contains_key(&KEY_LEFTCTRL)
            && !self.pressed_keys.contains_key(&KEY_RIGHTCTRL)
        {
            self.modifiers.ctrl = false;
        }
        if matches!(event.raw_code, KEY_LEFTALT | KEY_RIGHTALT)
            && !self.pressed_keys.contains_key(&KEY_LEFTALT)
            && !self.pressed_keys.contains_key(&KEY_RIGHTALT)
        {
            self.modifiers.alt = false;
        }
        if matches!(event.raw_code, KEY_LEFTSHIFT | KEY_RIGHTSHIFT)
            && !self.pressed_keys.contains_key(&KEY_LEFTSHIFT)
            && !self.pressed_keys.contains_key(&KEY_RIGHTSHIFT)
        {
            self.modifiers.shift = false;
        }
        if matches!(event.raw_code, KEY_LEFTMETA | KEY_RIGHTMETA)
            && !self.pressed_keys.contains_key(&KEY_LEFTMETA)
            && !self.pressed_keys.contains_key(&KEY_RIGHTMETA)
        {
            self.modifiers.logo = false;
        }
        if consumed {
            return;
        }
        // The release goes to the task that received the press, even if the
        // routed focus moved in between; a release without a routed press is
        // not forwarded.
        if let Some(object) = previous
            && let Err(error) = self.send_input(
                object,
                InputEvent::Key {
                    action: KeyAction::Up,
                    keycode: event.raw_code,
                    repeat: 0,
                },
            )
        {
            self.fail(&error);
        }
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
        self.modifiers = modifiers;
    }

    fn update_repeat_info(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _info: RepeatInfo,
    ) {
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if let PointerEventKind::Press { serial, .. } = event.kind
                && let Some(data) = pointer.data::<PointerData>()
            {
                self.activation.input(serial, data.seat(), &event.surface);
            }
            if let PointerEventKind::Press { serial, .. }
            | PointerEventKind::Release { serial, .. } = event.kind
            {
                self.clipboard.serial(serial);
            }

            if let Some((object, kind)) = self.task_and_button_for_surface(&event.surface) {
                match event.kind {
                    PointerEventKind::Enter { .. } => {
                        self.update_titlebar_button_state(object, kind, TitlebarButtonState::Hover);
                        if let Some(themed) = self.pointer.as_ref()
                            && let Err(error) = themed.set_cursor(_conn, CursorIcon::Pointer)
                        {
                            eprintln!("Droidloom cursor update failed: {error}");
                        }
                        self.cursor_icon = Some(CursorIcon::Pointer);
                    }
                    PointerEventKind::Leave { .. } => {
                        self.update_titlebar_button_state(object, kind, TitlebarButtonState::Normal);
                        self.titlebar_button_pointer = None;
                        self.cursor_icon = None;
                    }
                    PointerEventKind::Press { button, .. } if button == LEFT_BUTTON => {
                        self.update_titlebar_button_state(object, kind, TitlebarButtonState::Pressed);
                        self.titlebar_button_pointer = Some((object, kind));
                    }
                    PointerEventKind::Release { button, .. } if button == LEFT_BUTTON => {
                        let was_pressed = self.titlebar_button_pointer.take() == Some((object, kind));
                        self.update_titlebar_button_state(object, kind, TitlebarButtonState::Hover);
                        if was_pressed {
                            self.handle_titlebar_button_click(object, kind);
                        }
                    }
                    _ => {}
                }
                continue;
            }

            // The frame API keeps one hover/click state. A touch gesture owns it
            // until release, so concurrent mouse motion cannot change its action.
            let decoration_object = if self.decoration_touch.is_some() {
                None
            } else {
                match event.kind {
                    PointerEventKind::Enter { .. }
                    | PointerEventKind::Motion { .. }
                    | PointerEventKind::Press { .. }
                    | PointerEventKind::Release { .. } => self.decoration_pointer_task(
                        &event.surface, event.position.0, event.position.1
                    ),
                    PointerEventKind::Leave { .. } => {
                        self.decoration_pointer_left();
                        None
                    }
                    PointerEventKind::Axis { .. } => None,
                }
            };
            let cursor = decoration_object.as_ref().map_or(CursorIcon::Default, |(_, icon)| *icon);
            if matches!(event.kind, PointerEventKind::Enter { .. })
                || (matches!(event.kind, PointerEventKind::Motion { .. })
                    && self.cursor_icon != Some(cursor))
            {
                if let Some(themed) = self.pointer.as_ref()
                    && let Err(error) = themed.set_cursor(_conn, cursor)
                {
                    eprintln!("Droidloom cursor update failed: {error}");
                }
                self.cursor_icon = Some(cursor);
            }
            if matches!(event.kind, PointerEventKind::Leave { .. }) {
                self.cursor_icon = None;
            }
            if let Some((object, _)) = decoration_object {
                match event.kind {
                    PointerEventKind::Press { button, serial, time }
                    | PointerEventKind::Release { button, serial, time } => {
                        let click = match button {
                            0x110 => FrameClick::Normal,
                            0x111 => FrameClick::Alternate,
                            _ => continue,
                        };
                        let pressed = matches!(event.kind, PointerEventKind::Press { .. });
                        let action = self
                            .tasks
                            .get_mut(&object)
                            .and_then(|task| task.window_frame.as_mut())
                            .and_then(|frame| {
                                frame.on_click(
                                    Duration::from_millis(time as u64),
                                    click,
                                    pressed,
                                )
                            });
                        // AdwaitaFrame returns Resize for both press and release.
                        // xdg_toplevel.resize needs only the initiating press serial.
                        if let Some(action) = action
                            && (pressed || !matches!(action, FrameAction::Resize(_)))
                            && let Some(data) = pointer.data::<PointerData>()
                        {
                            self.frame_action(data.seat(), object, serial, action);
                        }
                    }
                    PointerEventKind::Enter { .. } | PointerEventKind::Motion { .. } => {}
                    _ => {}
                }
                continue;
            }

            if self.mouse_input_supported() {
                self.route_mouse(event);
                continue;
            }
            // A peer without mouse input receives the left button as touch.
            let object = self.task_for_surface(&event.surface);
            match event.kind {
                PointerEventKind::Press { button, .. }
                    if button == LEFT_BUTTON && self.pointer_contact.is_none() =>
                {
                    let Some(object) = object else { continue };
                    let (x_fixed, y_fixed) = self.fixed_position(object, event.position);
                    let contact = Contact {
                        object,
                        pointer_id: u32::MAX,
                        position_fixed: (x_fixed, y_fixed),
                    };
                    if let Err(error) = self.send_input(
                        object,
                        InputEvent::Touch {
                            action: TouchAction::Down,
                            pointer_id: contact.pointer_id,
                            x_fixed,
                            y_fixed,
                            pressure: u16::MAX,
                        },
                    ) {
                        self.fail(&error);
                    } else {
                        self.pointer_contact = Some(contact);
                    }
                }
                PointerEventKind::Motion { .. } => {
                    let Some(contact) = self.pointer_contact else {
                        continue;
                    };
                    let (x_fixed, y_fixed) = self.fixed_position(contact.object, event.position);
                    if let Err(error) = self.send_input(
                        contact.object,
                        InputEvent::Touch {
                            action: TouchAction::Motion,
                            pointer_id: contact.pointer_id,
                            x_fixed,
                            y_fixed,
                            pressure: u16::MAX,
                        },
                    ) {
                        self.fail(&error);
                    }
                }
                PointerEventKind::Release { button, .. } if button == LEFT_BUTTON => {
                    let Some(contact) = self.pointer_contact.take() else {
                        continue;
                    };
                    let (x_fixed, y_fixed) = self.fixed_position(contact.object, event.position);
                    if let Err(error) = self.send_input(
                        contact.object,
                        InputEvent::Touch {
                            action: TouchAction::Up,
                            pointer_id: contact.pointer_id,
                            x_fixed,
                            y_fixed,
                            pressure: 0,
                        },
                    ) {
                        self.fail(&error);
                    }
                }
                _ => {}
            }
        }
    }
}

impl TouchHandler for App {
    fn down(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &wl_touch::WlTouch,
        serial: u32,
        time: u32,
        surface: wl_surface::WlSurface,
        id: i32,
        position: (f64, f64),
    ) {
        if let Some((object, kind)) = self.task_and_button_for_surface(&surface) {
            self.titlebar_button_touch = Some((id, object, kind));
            self.update_titlebar_button_state(object, kind, TitlebarButtonState::Pressed);
            return;
        }
        if self.decoration_touch.is_none()
            && let Some((object, _)) =
                self.decoration_pointer_task(&surface, position.0, position.1)
        {
            if let Some(data) = _touch.data::<smithay_client_toolkit::seat::touch::TouchData>() {
                let seat = data.seat().clone();
                let action = self.tasks.get_mut(&object)
                    .and_then(|task| task.window_frame.as_mut())
                    .and_then(|frame| frame.on_click(
                        Duration::from_millis(time as u64), FrameClick::Normal, true
                    ));
                let acted_on_down = action.is_some();
                self.decoration_touch = Some(DecorationTouch {
                    id, touch: _touch.clone(), object, surface,
                    seat: seat.clone(), down_serial: serial, acted_on_down,
                });
                if let Some(action) = action {
                    self.frame_action(&seat, object, serial, action);
                }
            }
            return;
        }
        let Some(object) = self.task_for_surface(&surface) else {
            return;
        };
        let Ok(pointer_id) = u32::try_from(id) else {
            return;
        };
        self.text_input.note_touch();
        if let Some(data) = _touch.data::<smithay_client_toolkit::seat::touch::TouchData>() {
            self.activation.input(serial, data.seat(), &surface);
        }

        let (logical_size, is_fullscreen) = self.tasks.get(&object).map_or(
            ((1920, 1080), false),
            |t| (t.logical_size.unwrap_or((1920, 1080)), t.fullscreen || t.decorations_hidden),
        );

        if let Some(candidate) = gesture::EdgeGestureTracker::new_candidate(
            id, object, serial, pointer_id, position, logical_size, is_fullscreen,
        ) {
            self.edge_gestures.insert(id, candidate);
            return;
        }

        let (x_fixed, y_fixed) = self.fixed_position(object, position);
        let contact = Contact {
            object,
            pointer_id,
            position_fixed: (x_fixed, y_fixed),
        };
        if let Err(error) = self.send_input(object, contact.event(TouchAction::Down)) {
            self.fail(&error);
        } else {
            self.touch_contacts.insert(id, contact);
        }
    }

    fn up(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &wl_touch::WlTouch,
        _serial: u32,
        time: u32,
        id: i32,
    ) {
        if let Some((touch_id, object, kind)) = self.titlebar_button_touch {
            if touch_id == id {
                self.titlebar_button_touch = None;
                self.update_titlebar_button_state(object, kind, TitlebarButtonState::Normal);
                self.handle_titlebar_button_click(object, kind);
                return;
            }
        }
        if self.decoration_touch.as_ref()
            .is_some_and(|touch| touch.id == id && touch.touch == *_touch)
        {
            let touch = self.decoration_touch.take().expect("matched decoration touch");
            let action = self.tasks.get_mut(&touch.object)
                .and_then(|task| task.window_frame.as_mut())
                .and_then(|frame| {
                    let action = (!touch.acted_on_down).then(|| frame.on_click(
                        Duration::from_millis(time as u64), FrameClick::Normal, false
                    )).flatten();
                    frame.click_point_left();
                    action
                });
            if let Some(action) = action {
                self.frame_action(&touch.seat, touch.object, touch.down_serial, action);
            }
            return;
        }

        if let Some(tracker) = self.edge_gestures.remove(&id) {
            match tracker.on_up() {
                gesture::EdgeUpResult::TriggerBack => {
                    self.send_back_key(tracker.object);
                }
                gesture::EdgeUpResult::TriggerTopReveal => {
                    self.reveal_fullscreen_controls(tracker.object, gesture::FULLSCREEN_REVEAL_DURATION);
                }
                gesture::EdgeUpResult::TapAtEdge { pos } => {
                    let (x_fixed, y_fixed) = self.fixed_position(tracker.object, pos);
                    let contact = Contact {
                        object: tracker.object,
                        pointer_id: tracker.pointer_id,
                        position_fixed: (x_fixed, y_fixed),
                    };
                    if let Err(error) = self.send_input(tracker.object, contact.event(TouchAction::Down)) {
                        self.fail(&error);
                    }
                    if let Err(error) = self.send_input(tracker.object, contact.event(TouchAction::Up)) {
                        self.fail(&error);
                    }
                }
                gesture::EdgeUpResult::None => {}
            }
            return;
        }

        let Some(contact) = self.touch_contacts.remove(&id) else {
            return;
        };
        // wl_touch.up carries no coordinates. Android still needs the last
        // position; substituting (0, 0) turns a button tap into a release outside it.
        if let Err(error) = self.send_input(contact.object, contact.event(TouchAction::Up)) {
            self.fail(&error);
        }
    }

    fn motion(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &wl_touch::WlTouch,
        _time: u32,
        id: i32,
        position: (f64, f64),
    ) {
        if let Some(surface) = self.decoration_touch.as_ref()
            .filter(|touch| touch.id == id && touch.touch == *_touch)
            .map(|touch| touch.surface.clone())
        {
            self.decoration_pointer_task(&surface, position.0, position.1);
            return;
        }

        if let Some(tracker) = self.edge_gestures.get_mut(&id) {
            match tracker.on_motion(position) {
                gesture::EdgeMotionResult::StayPending | gesture::EdgeMotionResult::ConfirmedBack => {
                    return;
                }
                gesture::EdgeMotionResult::ConfirmedTopReveal => {
                    self.reveal_fullscreen_controls(tracker.object, gesture::FULLSCREEN_REVEAL_DURATION);
                    return;
                }
                gesture::EdgeMotionResult::CancelScroll => {
                    let trk = self.edge_gestures.remove(&id).unwrap();
                    let (start_xf, start_yf) = self.fixed_position(trk.object, trk.start_pos);
                    let (cur_xf, cur_yf) = self.fixed_position(trk.object, position);
                    let contact = Contact {
                        object: trk.object,
                        pointer_id: trk.pointer_id,
                        position_fixed: (cur_xf, cur_yf),
                    };
                    if let Err(error) = self.send_input(
                        trk.object,
                        InputEvent::Touch {
                            action: TouchAction::Down,
                            pointer_id: trk.pointer_id,
                            x_fixed: start_xf,
                            y_fixed: start_yf,
                            pressure: u16::MAX,
                        },
                    ) {
                        self.fail(&error);
                    }
                    if let Err(error) = self.send_input(
                        trk.object,
                        InputEvent::Touch {
                            action: TouchAction::Motion,
                            pointer_id: trk.pointer_id,
                            x_fixed: cur_xf,
                            y_fixed: cur_yf,
                            pressure: u16::MAX,
                        },
                    ) {
                        self.fail(&error);
                    }
                    self.touch_contacts.insert(id, contact);
                    return;
                }
            }
        }

        let Some(mut contact) = self.touch_contacts.get(&id).copied() else {
            return;
        };
        contact.position_fixed = self.fixed_position(contact.object, position);
        if let Err(error) = self.send_input(contact.object, contact.event(TouchAction::Motion)) {
            self.fail(&error);
        } else {
            self.touch_contacts.insert(id, contact);
        }
    }

    fn shape(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &wl_touch::WlTouch,
        _id: i32,
        _major: f64,
        _minor: f64,
    ) {
    }

    fn orientation(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _touch: &wl_touch::WlTouch,
        _id: i32,
        _orientation: f64,
    ) {
    }

    fn cancel(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _touch: &wl_touch::WlTouch) {
        if let Some((_, object, kind)) = self.titlebar_button_touch.take() {
            self.update_titlebar_button_state(object, kind, TitlebarButtonState::Normal);
        }
        if self.decoration_touch.as_ref().is_some_and(|touch| touch.touch == *_touch)
            && let Some(touch) = self.decoration_touch.take()
            && let Some(frame) = self.tasks.get_mut(&touch.object)
                .and_then(|task| task.window_frame.as_mut())
        {
            frame.click_point_left();
        }
        self.edge_gestures.clear();
        let contacts = std::mem::take(&mut self.touch_contacts);
        for (_, contact) in contacts {
            if let Err(error) = self.send_input(
                contact.object,
                InputEvent::Touch {
                    action: TouchAction::Cancel,
                    pointer_id: contact.pointer_id,
                    x_fixed: 0,
                    y_fixed: 0,
                    pressure: 0,
                },
            ) {
                self.fail(&error);
                break;
            }
        }
    }
}

impl Dispatch<ZwpTabletSeatV2, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwpTabletSeatV2,
        event: tablet::zwp_tablet_seat_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let seat_id = proxy.id().protocol_id();
        match &event {
            tablet::zwp_tablet_seat_v2::Event::TabletAdded { id } => {
                state.tablet_proxies.insert(id.id().protocol_id(), (seat_id, id.clone()));
            }
            tablet::zwp_tablet_seat_v2::Event::ToolAdded { id } => {
                let tool_id = id.id().protocol_id();
                state.tablet_tool_seats.insert(tool_id, seat_id);
                state.tablet_tool_proxies.insert(tool_id, id.clone());
            }
            tablet::zwp_tablet_seat_v2::Event::PadAdded { id } => {
                state.tablet_pads.insert(id.id().protocol_id(), TabletPadState {
                    tablet_seat_id: seat_id,
                    proxy: id.clone(),
                    children: Vec::new(),
                    focus: None,
                    pressed: BTreeSet::new(),
                });
            }
            _ => {}
        }
        eprintln!("Droidloom tablet trace: stage=tablet-seat event={event:?}");
    }

    wayland_client::event_created_child!(
        App,
        ZwpTabletSeatV2,
        [
            0 => (ZwpTabletV2, ()),
            1 => (ZwpTabletToolV2, ()),
            2 => (ZwpTabletPadV2, ())
        ]
    );
}

impl Dispatch<ZwpTabletToolV2, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwpTabletToolV2,
        event: tablet::zwp_tablet_tool_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let tool_id = proxy.id().protocol_id();
        match event {
            tablet::zwp_tablet_tool_v2::Event::Type { tool_type } => {
                let tool = state.tablet_tool_mut(tool_id);
                if let WEnum::Value(tool_type) = tool_type {
                    tool.supported = match tool_type {
                        tablet::zwp_tablet_tool_v2::Type::Pen => {
                            tool.tool_type = TabletToolType::Pen;
                            true
                        }
                        tablet::zwp_tablet_tool_v2::Type::Eraser => {
                            tool.tool_type = TabletToolType::Eraser;
                            true
                        }
                        tablet::zwp_tablet_tool_v2::Type::Brush => {
                            tool.tool_type = TabletToolType::Brush;
                            true
                        }
                        tablet::zwp_tablet_tool_v2::Type::Pencil => {
                            tool.tool_type = TabletToolType::Pencil;
                            true
                        }
                        tablet::zwp_tablet_tool_v2::Type::Airbrush => {
                            tool.tool_type = TabletToolType::Airbrush;
                            true
                        }
                        _ => false,
                    };
                } else {
                    tool.supported = false;
                }
                eprintln!(
                    "Droidloom tablet trace: stage=tool-type tool={tool_id} type={:?} supported={}",
                    tool.tool_type, tool.supported
                );
            }
            tablet::zwp_tablet_tool_v2::Event::ProximityIn { surface, .. } => {
                eprintln!("Droidloom tablet trace: stage=wayland-event action=ProximityIn tool={tool_id}");
                let tool = state.tablet_tool_mut(tool_id);
                tool.surface = Some(surface);
                tool.axis_flags = 0;
                tool.pressure = 0;
                tool.pending_actions.push((TabletAction::ProximityIn, 0));
            }
            tablet::zwp_tablet_tool_v2::Event::ProximityOut => {
                eprintln!("Droidloom tablet trace: stage=wayland-event action=ProximityOut tool={tool_id}");
                state.tablet_tool_mut(tool_id).pending_actions.push((TabletAction::ProximityOut, 0));
            }
            tablet::zwp_tablet_tool_v2::Event::Down { serial } => {
                eprintln!("Droidloom tablet trace: stage=wayland-event action=Down tool={tool_id}");
                let tool = state.tablet_tool_mut(tool_id);
                tool.down_serial = serial;
                tool.pending_actions.push((TabletAction::Down, 0));
            }
            tablet::zwp_tablet_tool_v2::Event::Up => {
                eprintln!("Droidloom tablet trace: stage=wayland-event action=Up tool={tool_id}");
                state.tablet_tool_mut(tool_id).pending_actions.push((TabletAction::Up, 0));
            }
            tablet::zwp_tablet_tool_v2::Event::Motion { x, y } => {
                let tool = state.tablet_tool_mut(tool_id);
                tool.x_fixed = fixed_16_16(x);
                tool.y_fixed = fixed_16_16(y);
                tool.dirty_axes = true;
            }
            tablet::zwp_tablet_tool_v2::Event::Pressure { pressure } => {
                let tool = state.tablet_tool_mut(tool_id);
                tool.pressure = u16::try_from(pressure.min(u32::from(u16::MAX))).unwrap_or(u16::MAX);
                tool.axis_flags |= 1 << 0;
                tool.dirty_axes = true;
            }
            tablet::zwp_tablet_tool_v2::Event::Distance { distance } => {
                let tool = state.tablet_tool_mut(tool_id);
                tool.distance = u16::try_from(distance.min(u32::from(u16::MAX))).unwrap_or(u16::MAX);
                tool.axis_flags |= 1 << 1;
                tool.dirty_axes = true;
            }
            tablet::zwp_tablet_tool_v2::Event::Tilt { tilt_x, tilt_y } => {
                let tool = state.tablet_tool_mut(tool_id);
                tool.tilt_x_tenths = App::fixed_angle_tenths(tilt_x);
                tool.tilt_y_tenths = App::fixed_angle_tenths(tilt_y);
                tool.axis_flags |= 1 << 2;
                tool.dirty_axes = true;
            }
            tablet::zwp_tablet_tool_v2::Event::Rotation { degrees } => {
                let tool = state.tablet_tool_mut(tool_id);
                tool.rotation_tenths = App::fixed_angle_tenths(degrees);
                tool.axis_flags |= 1 << 3;
                tool.dirty_axes = true;
            }
            tablet::zwp_tablet_tool_v2::Event::Slider { position } => {
                let tool = state.tablet_tool_mut(tool_id);
                tool.slider = position;
                tool.axis_flags |= 1 << 4;
                tool.dirty_axes = true;
            }
            tablet::zwp_tablet_tool_v2::Event::Wheel { degrees, clicks } => {
                let tool = state.tablet_tool_mut(tool_id);
                tool.wheel_degrees_fixed = fixed_16_16(degrees);
                tool.wheel_clicks = clicks.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
                tool.axis_flags |= 1 << 5;
                tool.pending_actions.push((TabletAction::Wheel, 0));
            }
            tablet::zwp_tablet_tool_v2::Event::Button { button, state: button_state, .. } => {
                let action = match button_state {
                    WEnum::Value(tablet::zwp_tablet_tool_v2::ButtonState::Pressed) => {
                        TabletAction::ButtonPress
                    }
                    WEnum::Value(tablet::zwp_tablet_tool_v2::ButtonState::Released) => {
                        TabletAction::ButtonRelease
                    }
                    WEnum::Unknown(_) | WEnum::Value(_) => return,
                };
                state.tablet_tool_mut(tool_id).pending_actions.push((action, button));
            }
            tablet::zwp_tablet_tool_v2::Event::Frame { .. } => {
                if let Err(error) = state.flush_tablet_axes(tool_id) {
                    state.fail(&error);
                }
            }
            tablet::zwp_tablet_tool_v2::Event::Removed => {
                if let Err(error) = state.flush_tablet_axes(tool_id)
                    .and_then(|()| state.send_tablet_event(tool_id, TabletAction::Cancel, 0))
                {
                    state.fail(&error);
                }
                state.tablet_tools.retain(|tool| tool.tool_id != tool_id);
                state.tablet_tool_seats.remove(&tool_id);
                state.tablet_tool_proxies.remove(&tool_id);
                proxy.destroy();
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwpTabletV2, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwpTabletV2,
        event: tablet::zwp_tablet_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if matches!(event, tablet::zwp_tablet_v2::Event::Removed) {
            state.tablet_proxies.remove(&proxy.id().protocol_id());
            proxy.destroy();
        }
    }
}

impl Dispatch<ZwpTabletPadV2, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwpTabletPadV2,
        event: tablet::zwp_tablet_pad_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let pad_id = proxy.id().protocol_id();
        match event {
            tablet::zwp_tablet_pad_v2::Event::Group { pad_group } => {
                let group_id = pad_group.id().protocol_id();
                state.tablet_pad_groups.insert(group_id, pad_id);
                if let Some(pad) = state.tablet_pads.get_mut(&pad_id) {
                    pad.children.push(TabletPadChild::Group(pad_group));
                }
            }
            tablet::zwp_tablet_pad_v2::Event::Enter { surface, .. } => {
                let object = state.task_for_surface(&surface);
                if let Some(pad) = state.tablet_pads.get_mut(&pad_id) {
                    pad.focus = object;
                }
            }
            tablet::zwp_tablet_pad_v2::Event::Leave { .. } => state.release_pad_buttons(pad_id),
            tablet::zwp_tablet_pad_v2::Event::Button { button, state: button_state, .. } => {
                let pressed = match button_state {
                    WEnum::Value(tablet::zwp_tablet_pad_v2::ButtonState::Pressed) => true,
                    WEnum::Value(tablet::zwp_tablet_pad_v2::ButtonState::Released) => false,
                    _ => return,
                };
                state.pad_button(pad_id, button, pressed);
            }
            tablet::zwp_tablet_pad_v2::Event::Removed => {
                state.release_pad_buttons(pad_id);
                state.destroy_tablet_pad(pad_id);
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(
        App,
        ZwpTabletPadV2,
        [0 => (ZwpTabletPadGroupV2, ())]
    );
}

impl Dispatch<ZwpTabletPadGroupV2, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &ZwpTabletPadGroupV2,
        event: tablet::zwp_tablet_pad_group_v2::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        let group_id = proxy.id().protocol_id();
        let Some(pad_id) = state.tablet_pad_groups.get(&group_id).copied() else {
            return;
        };
        let child = match event {
            tablet::zwp_tablet_pad_group_v2::Event::Ring { ring } => Some(TabletPadChild::Ring(ring)),
            tablet::zwp_tablet_pad_group_v2::Event::Strip { strip } => Some(TabletPadChild::Strip(strip)),
            tablet::zwp_tablet_pad_group_v2::Event::Dial { dial } => Some(TabletPadChild::Dial(dial)),
            _ => None,
        };
        if let Some(child) = child {
            if let Some(pad) = state.tablet_pads.get_mut(&pad_id) {
                pad.children.push(child);
            }
        }
    }

    wayland_client::event_created_child!(
        App,
        ZwpTabletPadGroupV2,
        [
            1 => (ZwpTabletPadRingV2, ()),
            2 => (ZwpTabletPadStripV2, ()),
            6 => (ZwpTabletPadDialV2, ())
        ]
    );
}

wayland_client::delegate_noop!(App: ignore ZwpTabletManagerV2);
wayland_client::delegate_noop!(App: ignore ZwpTabletPadRingV2);
wayland_client::delegate_noop!(App: ignore ZwpTabletPadStripV2);
wayland_client::delegate_noop!(App: ignore ZwpTabletPadDialV2);

impl PointerConstraintsHandler for App {
    fn confined(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _confined_pointer: &ZwpConfinedPointerV1,
        _surface: &wl_surface::WlSurface,
        _pointer: &wl_pointer::WlPointer,
    ) {
    }

    fn unconfined(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        confined_pointer: &ZwpConfinedPointerV1,
        _surface: &wl_surface::WlSurface,
        _pointer: &wl_pointer::WlPointer,
    ) {
        // A persistent confinement is only suspended while the window lacks
        // focus; the compositor reactivates it when focus returns.
        let _ = confined_pointer;
    }

    fn locked(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _locked_pointer: &ZwpLockedPointerV1,
        _surface: &wl_surface::WlSurface,
        _pointer: &wl_pointer::WlPointer,
    ) {
    }

    fn unlocked(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _locked_pointer: &ZwpLockedPointerV1,
        _surface: &wl_surface::WlSurface,
        _pointer: &wl_pointer::WlPointer,
    ) {
    }
}

wayland_client::delegate_noop!(App: ignore ZwpKeyboardShortcutsInhibitManagerV1);
wayland_client::delegate_noop!(App: ignore ZwpKeyboardShortcutsInhibitorV1);

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm_state
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

smithay_client_toolkit::delegate_compositor!(App);
smithay_client_toolkit::delegate_output!(App);
smithay_client_toolkit::delegate_xdg_shell!(App);
smithay_client_toolkit::delegate_xdg_window!(App);
smithay_client_toolkit::delegate_dmabuf!(App);
smithay_client_toolkit::delegate_presentation_time!(App);
smithay_client_toolkit::delegate_seat!(App);
smithay_client_toolkit::delegate_keyboard!(App);
smithay_client_toolkit::delegate_pointer!(App);
smithay_client_toolkit::delegate_pointer_constraints!(App);
smithay_client_toolkit::delegate_touch!(App);
smithay_client_toolkit::delegate_subcompositor!(App);
smithay_client_toolkit::delegate_shm!(App);
smithay_client_toolkit::delegate_registry!(App);

wayland_client::delegate_noop!(App: ignore WpLinuxDrmSyncobjManagerV1);
wayland_client::delegate_noop!(App: ignore WpLinuxDrmSyncobjSurfaceV1);
wayland_client::delegate_noop!(App: ignore WpLinuxDrmSyncobjTimelineV1);
wayland_client::delegate_noop!(App: ignore WpFractionalScaleManagerV1);
wayland_client::delegate_noop!(App: ignore WpViewporter);
wayland_client::delegate_noop!(App: ignore WpViewport);

fn main() {
    // SAFETY: the process is still single-threaded. Mesa consumes this optional
    // worker policy after its own full-affinity reset, before executing jobs.
    if let Some(groups) = droidloom_cpu_placement::initialize(droidloom_cpu_placement::Role::Graphics) {
        unsafe { env::set_var("MESA_BACKGROUND_CPUS", groups.background.list()); }
    }
    if let Err(error) = run() {
        eprintln!("droidloom-wayland: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), PresenterError> {
    let runtime_root = PathBuf::from(
        env::var("XDG_RUNTIME_DIR")
            .map_err(|_| PresenterError::Configuration("XDG_RUNTIME_DIR is missing"))?,
    );
    let socket_path = env::var_os("DROIDLOOM_HOST_SOCKET").map_or_else(
        || runtime_root.join("droidloom/native-bridge.sock"),
        PathBuf::from,
    );
    let render_node = env::var_os("DROIDLOOM_RENDER_NODE")
        .map_or_else(|| PathBuf::from("/dev/dri/renderD128"), PathBuf::from);
    validate_render_node(&render_node)?;
    let identity = fs::metadata("/proc/self")?;
    prepare_socket_path(&socket_path, &runtime_root, identity.uid())?;
    if let Some(parent) = socket_path.parent() {
        prepare_dbus_send_shim(parent);
    }
    // The async reactor may be created synchronously by Bridge::start.
    droidloom_cpu_placement::current(droidloom_cpu_placement::Role::Background);
    let notifications = notifications::Bridge::start(socket_path.with_file_name("notifications.sock"));
    droidloom_cpu_placement::current(droidloom_cpu_placement::Role::Graphics);
    let _notifications = notifications?;

    let render_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&render_node)?;
    let gbm = GbmDevice::new(render_file.try_clone()?)?;
    let syncobj = SyncobjDevice::from_file(render_file);
    let expected_peer = ExpectedPeer {
        pid: parse_env_u32("DROIDLOOM_HOST_PEER_PID", 0)?,
        uid: parse_env_u32("DROIDLOOM_HOST_PEER_UID", identity.uid())?,
        gid: parse_env_u32("DROIDLOOM_HOST_PEER_GID", identity.gid())?,
    };
    let listener = DenialEndpointListener::bind(&socket_path, syncobj.clone(), expected_peer)?;
    fs::set_permissions(&socket_path, fs::Permissions::from_mode(0o666))?;
    listener.set_nonblocking(true)?;
    let session_mode = env::var("DROIDLOOM_MODE")
        .unwrap_or_else(|_| "desktop".into())
        .parse::<SessionMode>()
        .map_err(|_| PresenterError::Configuration("DROIDLOOM_MODE must be desktop or mobile"))?;
    eprintln!("Droidloom session mode: {session_mode}");
    let window_policy = WindowPolicyPaths::from_environment(
        env::var_os("XDG_CONFIG_HOME"),
        env::var_os("XDG_STATE_HOME"),
        env::var_os("HOME"),
    )
    .and_then(WindowPolicyStore::load)
    .unwrap_or_else(|error| {
        eprintln!("Droidloom is using an in-memory window policy: {error}");
        WindowPolicyStore::ephemeral()
    })
    .with_session_mode(session_mode);

    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();
    let compositor = Arc::new(CompositorState::bind(&globals, &qh)?);
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let viewporter: WpViewporter = globals.bind(&qh, 1..=1, ())?;
    let fractional_scale_manager: WpFractionalScaleManagerV1 = globals.bind(&qh, 1..=1, ())?;
    let tablet_manager: Option<ZwpTabletManagerV2> = globals.bind(&qh, 1..=2, ()).ok();
    eprintln!(
        "Droidloom tablet trace: stage=wayland-global tablet-v2={}",
        tablet_manager.is_some()
    );
    let dmabuf_state = DmabufState::new(&globals, &qh);
    let presentation_time = PresentationTimeState::bind(&globals, &qh);
    if !matches!(dmabuf_state.version(), Some(4..)) {
        return Err(PresenterError::DmabufV4Required);
    }
    let sync_manager: WpLinuxDrmSyncobjManagerV1 = globals.bind(&qh, 1..=1, ())?;
    let seat_state = SeatState::new(&globals, &qh);
    let mut tablet_seats = Vec::new();
    if let Some(manager) = &tablet_manager {
        for seat in seat_state.seats() {
            if !tablet_seats.iter().any(|(existing, _)| *existing == seat) {
                tablet_seats.push((seat.clone(), manager.get_tablet_seat(&seat, &qh, ())));
            }
        }
    }
    eprintln!(
        "Droidloom tablet trace: stage=initial-seats-bound count={}",
        tablet_seats.len()
    );
    let mut app = App {
        activation: activation::Activation::new(&globals, &qh),
        layer_globals: layers::Globals { subcompositor: globals.bind(&qh, 1..=1, ())?, shm: globals.bind(&qh, 1..=1, ())?, alpha: globals.bind(&qh, 1..=1, ()).ok() },
        clipboard: clipboard::Clipboard::new(socket_path.with_file_name("clipboard.sock"), &globals, &qh)?,
        text_input: text_input::TextInput::new(
            socket_path.with_file_name("text-input.sock"),
            globals.bind(&qh, 1..=1, ()).ok(),
        )?,
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        presentation_time,
        seat_state,
        subcompositor_state: Arc::new(SubcompositorState::bind(
            compositor.wl_compositor().clone(),
            &globals,
            &qh,
        )?),
        shm_state: Shm::bind(&globals, &qh)?,
        tablet_manager,
        tablet_seats,
        tablet_tools: Vec::new(),
        tablet_tool_seats: BTreeMap::new(),
        tablet_tool_proxies: BTreeMap::new(),
        tablet_proxies: BTreeMap::new(),
        tablet_pads: BTreeMap::new(),
        tablet_pad_groups: BTreeMap::new(),
        dmabuf_state,
        feedback: None,
        compositor,
        xdg_shell,
        insets_manager: globals.bind(&qh, 1..=1, ()).ok(),
        viewporter,
        fractional_scale_manager,
        sync_manager,
        syncobj,
        gbm,
        listener,
        window_policy,
        endpoint: None,
        tasks: BTreeMap::new(),
        keyboard: None,
        keyboard_seat: None,
        modifiers: Modifiers::default(),
        pressed_keys: BTreeMap::new(),
        consumed_keys: BTreeSet::new(),
        pointer: None,
        pointer_constraints: PointerConstraintsState::bind(&globals, &qh),
        shortcuts_inhibit_manager: globals.bind(&qh, 1..=1, ()).ok(),
        immersion: None,
        cursor_icon: None,
        touch: None,
        focused: None,
        mouse_focus: None,
        mouse_buttons: BTreeSet::new(),
        pointer_contact: None,
        touch_contacts: BTreeMap::new(),
        edge_gestures: BTreeMap::new(),
        decoration_touch: None,
        titlebar_button_pointer: None,
        titlebar_button_touch: None,
        titlebar_button_tablet: None,
        next_buffer_id: 0,
        next_input_serial: 0,
        socket_path,
        start_time: Instant::now(),
        fatal: None,
    };
    app.text_input.bind_panel_feedback(&globals, &qh);
    app.dmabuf_state.get_default_feedback(&qh)?;
    while app.feedback.is_none() || app.host_canvas_size().is_none() {
        event_queue.blocking_dispatch(&mut app)?;
    }
    notify_ready()?;

    run_presenter_loop(&conn, event_queue, app)
}

fn run_presenter_loop(
    conn: &Connection,
    mut event_queue: EventQueue<App>,
    mut app: App,
) -> Result<(), PresenterError> {
    let qh = event_queue.handle();
    let mut poll_descriptors = Vec::new();
    let mut layer_tasks = Vec::new();
    loop {
        event_queue.dispatch_pending(&mut app)?;
        app.unmap_requested_tasks()?;
        app.pump_fullscreen_controls_timeouts();
        if let Some(error) = app.fatal.take() {
            return Err(PresenterError::Wayland(error));
        }
        app.accept_endpoint()?;
        app.drain_endpoint(&qh)?;
        app.drain_layers(&qh, &mut layer_tasks)?;
        app.pump_text_input();
        app.clipboard.pump(&qh);
        app.unmap_requested_tasks()?;
        app.release_ready_frames()?;
        poll_sources(conn, &mut event_queue, &mut app, &mut poll_descriptors)?;
    }
}

fn notify_ready() -> Result<(), PresenterError> {
    let Some(address) = env::var_os("NOTIFY_SOCKET") else {
        return Ok(());
    };
    let socket = UnixDatagram::unbound()?;
    let payload = b"READY=1\nSTATUS=Connected to Wayland; ready to start Android";
    if let Some(name) = address.as_bytes().strip_prefix(b"@") {
        let address = SocketAddr::from_abstract_name(name)?;
        socket.send_to_addr(payload, &address)?;
    } else {
        socket.send_to(payload, Path::new(&address))?;
    }
    Ok(())
}

fn poll_sources(
    conn: &Connection,
    event_queue: &mut EventQueue<App>,
    app: &mut App,
    descriptors: &mut Vec<libc::pollfd>,
) -> Result<(), PresenterError> {
    let write_blocked = match event_queue.flush() {
        Ok(()) => {
            for pending in app.tasks.values_mut().flat_map(|task| task.pending.iter_mut()) {
                if let Some(trace) = pending.trace.as_mut() { trace.flushed(); }
            }
            false
        }
        Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => true,
        Err(error) => return Err(PresenterError::Wayland(error.to_string())),
    };
    // Pending Wayland events must be dispatched before sleeping. The previous
    // fixed timeout masked a missed wakeup here when the socket was drained
    // by another reader but our event queue still contained callbacks.
    let Some(read_guard) = event_queue.prepare_read() else { return Ok(()); };
    descriptors.clear();
    descriptors.extend([
        libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN | if write_blocked { libc::POLLOUT } else { 0 },
            revents: 0,
        },
        libc::pollfd {
            fd: app.listener.listener().as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ]);
    if let Some(endpoint) = app.endpoint.as_ref() {
        descriptors.push(libc::pollfd {
            fd: endpoint.socket().socket().as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }
    for (fd, events) in app.text_input.fds().chain(app.clipboard.fds()) {
        descriptors.push(libc::pollfd {
            fd,
            events,
            revents: 0,
        });
    }
    for stream in app.tasks.values().filter_map(|t|t.layers.as_ref()) {
        for (fd,events) in stream.fds() { descriptors.push(libc::pollfd { fd, events, revents:0 }); }
    }
    let wakeups_start = descriptors.len();
    let mut release_fallback = app.tasks.values().filter_map(|t|t.layers.as_ref()).any(|s|s.poll_fallback());
    for target in app.tasks.values().flat_map(|task| task.targets.values()) {
        if target.busy {
            if let Some(event) = target.sync.as_ref().and_then(|sync| sync.release_wakeup.as_ref()) {
                descriptors.push(libc::pollfd {
                    fd: event.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            } else {
                // Only kernels without timeline eventfd support need periodic
                // checks, and only while an actual release is outstanding.
                release_fallback = true;
            }
        }
    }
    let fallback_timeout = poll_timeout_millis(release_fallback);
    let controls_timeout = app.next_controls_timeout_millis();
    let timeout = match (fallback_timeout, controls_timeout) {
        (t, Some(ct)) if t < 0 => ct,
        (t, Some(ct)) => t.min(ct),
        (t, None) => t,
    };
    // SAFETY: `descriptors` is live writable storage for exactly its length;
    // poll retains no pointer after returning.
    let result = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            libc::nfds_t::try_from(descriptors.len()).unwrap_or(libc::nfds_t::MAX),
            timeout,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
    app.clipboard.notify_ready(descriptors);
    for event in &descriptors[wakeups_start..] {
        if event.revents & libc::POLLIN != 0 {
            let mut count = 0_u64;
            // SAFETY: all entries in this slice borrow live nonblocking
            // eventfds; count is writable storage for the required eight bytes.
            let result = unsafe {
                libc::read(event.fd, std::ptr::from_mut(&mut count).cast(), size_of::<u64>())
            };
            if result < 0 {
                let error = io::Error::last_os_error();
                if !matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) {
                    return Err(error.into());
                }
            }
            // The next loop checks the real timeline and presentation feedback.
            // Draining here prevents spinning while feedback is still pending.
        }
    }
    if descriptors[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
        match read_guard.read() {
            Ok(_) => {}
            Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(PresenterError::Wayland(error.to_string())),
        }
    } else {
        drop(read_guard);
    }
    Ok(())
}

fn poll_timeout_millis(release_fallback: bool) -> i32 {
    if release_fallback {
        i32::try_from(RELEASE_POLL_FALLBACK.as_nanos().div_ceil(1_000_000))
            .unwrap_or(i32::MAX)
    } else {
        -1
    }
}

fn split_point(point: u64) -> (u32, u32) {
    (
        u32::try_from(point >> 32).unwrap_or_default(),
        u32::try_from(point & u64::from(u32::MAX)).unwrap_or_default(),
    )
}

fn monotonic_timestamp_nanos() -> Result<u64, PresenterError> {
    let mut timestamp = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `timestamp` is valid writable storage and CLOCK_MONOTONIC needs
    // no additional lifetime or ownership guarantees.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &raw mut timestamp) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    let seconds = u64::try_from(timestamp.tv_sec)
        .map_err(|_| PresenterError::Configuration("monotonic clock returned negative seconds"))?;
    let nanos = u64::try_from(timestamp.tv_nsec)
        .map_err(|_| PresenterError::Configuration("monotonic clock returned negative nanos"))?;
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(nanos))
        .ok_or(PresenterError::Configuration(
            "monotonic timestamp overflowed nanoseconds",
        ))
}

fn presentation_timestamp_nanos(seconds: u64, nanos: u32) -> Result<u64, PresenterError> {
    if nanos >= 1_000_000_000 {
        return Err(PresenterError::Configuration(
            "presentation timestamp nanoseconds are invalid",
        ));
    }
    seconds
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(u64::from(nanos)))
        .ok_or(PresenterError::Configuration(
            "presentation timestamp overflowed nanoseconds",
        ))
}

fn presentation_refresh_period(
    presentation_refresh_nanos: u32,
    advertised_refresh_millihz: u32,
) -> Result<u64, PresenterError> {
    if presentation_refresh_nanos != 0 {
        return Ok(u64::from(presentation_refresh_nanos));
    }
    if advertised_refresh_millihz == 0 {
        return Err(PresenterError::Configuration(
            "presentation feedback omitted refresh period",
        ));
    }
    Ok(1_000_000_000_000_u64 / u64::from(advertised_refresh_millihz))
}

#[allow(clippy::cast_possible_truncation)]
fn fixed_16_16(value: f64) -> i32 {
    let scaled = (value * 65_536.0).round();
    if scaled <= f64::from(i32::MIN) {
        i32::MIN
    } else if scaled >= f64::from(i32::MAX) {
        i32::MAX
    } else {
        scaled as i32
    }
}

fn scale_dimension(logical: u32, scale_120: u32) -> Result<u32, PresenterError> {
    if scale_120 == 0 {
        return Err(PresenterError::Configuration(
            "fractional scale numerator is zero",
        ));
    }
    let scaled = (u64::from(logical) * u64::from(scale_120)
        + u64::from(FRACTIONAL_SCALE_DENOMINATOR / 2))
        / u64::from(FRACTIONAL_SCALE_DENOMINATOR);
    let scaled = u32::try_from(scaled)
        .map_err(|_| PresenterError::Configuration("scaled window dimension overflowed"))?
        .max(1);
    if scaled > MAX_BUFFER_DIMENSION {
        return Err(PresenterError::Configuration(
            "scaled window dimension exceeds protocol limit",
        ));
    }
    Ok(scaled)
}

fn oriented_output_size(
    dimensions: (i32, i32),
    transform: wl_output::Transform,
) -> Option<(u32, u32)> {
    let width = u32::try_from(dimensions.0)
        .ok()
        .filter(|value| *value != 0)?;
    let height = u32::try_from(dimensions.1)
        .ok()
        .filter(|value| *value != 0)?;
    let (width, height) = match transform {
        wl_output::Transform::_90
        | wl_output::Transform::_270
        | wl_output::Transform::Flipped90
        | wl_output::Transform::Flipped270 => (height, width),
        _ => (width, height),
    };
    (width <= MAX_BUFFER_DIMENSION && height <= MAX_BUFFER_DIMENSION).then_some((width, height))
}

fn parse_env_u32(name: &str, default: u32) -> Result<u32, PresenterError> {
    env::var(name).map_or(Ok(default), |value| {
        value
            .parse()
            .map_err(|_| PresenterError::Configuration("peer identity is not an integer"))
    })
}

fn validate_render_node(path: &Path) -> Result<(), PresenterError> {
    let valid_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("renderD"))
        .is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        });
    if path.parent() == Some(Path::new("/dev/dri")) && valid_name {
        Ok(())
    } else {
        Err(PresenterError::Configuration(
            "render node must be /dev/dri/renderD<number>",
        ))
    }
}

fn prepare_socket_path(
    socket: &Path,
    runtime_root: &Path,
    effective_uid: u32,
) -> Result<(), PresenterError> {
    if !socket.is_absolute() || !runtime_root.is_absolute() {
        return Err(PresenterError::Configuration(
            "runtime and endpoint paths must be absolute",
        ));
    }
    let Some(parent) = socket.parent() else {
        return Err(PresenterError::Configuration("endpoint has no parent"));
    };
    if parent.parent() != Some(runtime_root) {
        return Err(PresenterError::Configuration(
            "endpoint must be one directory below XDG_RUNTIME_DIR",
        ));
    }
    match fs::create_dir(parent) {
        Ok(()) => fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.file_type().is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != effective_uid
        || metadata.mode() & 0o077 != 0
    {
        return Err(PresenterError::Configuration(
            "runtime directory is not private and session-owned",
        ));
    }
    match fs::symlink_metadata(socket) {
        Ok(metadata) if metadata.file_type().is_socket() => fs::remove_file(socket)?,
        Ok(_) => {
            return Err(PresenterError::Configuration(
                "refusing to replace a non-socket endpoint",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn prepare_dbus_send_shim(runtime_dir: &Path) {
    let shim_dir = runtime_dir.join("shims");
    if let Err(error) = fs::create_dir_all(&shim_dir) {
        eprintln!("droidloom-wayland: failed to create shim directory: {error}");
        return;
    }
    let shim_path = shim_dir.join("dbus-send");
    let script = r#"#!/bin/sh
CACHE_DIR="${XDG_RUNTIME_DIR:-/tmp}/droidloom"
mkdir -p "$CACHE_DIR" 2>/dev/null

REAL_DBUS=$(which -a dbus-send 2>/dev/null | grep -v "/droidloom/shims" | head -n1)
[ -z "$REAL_DBUS" ] && REAL_DBUS="/usr/bin/dbus-send"

case "$*" in
  *"string:org.gnome.desktop.wm.preferences"*"string:button-layout"*)
    CACHE_FILE="$CACHE_DIR/button-layout.cache"
    OUT=$("$REAL_DBUS" --reply-timeout=2000 "$@" 2>/dev/null)
    if [ -n "$OUT" ]; then
      printf '%s\n' "$OUT" > "$CACHE_FILE"
      printf '%s\n' "$OUT"
      exit 0
    fi
    GSET=$(gsettings get org.gnome.desktop.wm.preferences button-layout 2>/dev/null)
    if [ -n "$GSET" ]; then
      CLEAN=$(printf '%s' "$GSET" | tr -d "'\"")
      OUT="   variant       variant          $CLEAN"
      printf '%s\n' "$OUT" > "$CACHE_FILE"
      printf '%s\n' "$OUT"
      exit 0
    fi
    if [ -s "$CACHE_FILE" ]; then
      cat "$CACHE_FILE"
      exit 0
    fi
    printf '   variant       variant          close,minimize,maximize:appmenu\n'
    exit 0
    ;;
  *"string:org.freedesktop.appearance"*"string:color-scheme"*)
    CACHE_FILE="$CACHE_DIR/color-scheme.cache"
    OUT=$("$REAL_DBUS" --reply-timeout=2000 "$@" 2>/dev/null)
    if [ -n "$OUT" ]; then
      printf '%s\n' "$OUT" > "$CACHE_FILE"
      printf '%s\n' "$OUT"
      exit 0
    fi
    GSET=$(gsettings get org.gnome.desktop.interface color-scheme 2>/dev/null)
    case "$GSET" in
      *dark*)
        OUT="   variant       variant          uint32 1"
        ;;
      *)
        OUT="   variant       variant          uint32 0"
        ;;
    esac
    printf '%s\n' "$OUT" > "$CACHE_FILE"
    printf '%s\n' "$OUT"
    exit 0
    ;;
  *)
    exec "$REAL_DBUS" "$@"
    ;;
esac
"#;
    if let Ok(()) = fs::write(&shim_path, script) {
        let _ = fs::set_permissions(&shim_path, fs::Permissions::from_mode(0o755));
        if let Some(old_path) = env::var_os("PATH") {
            let mut new_path = shim_dir.into_os_string();
            new_path.push(":");
            new_path.push(old_path);
            // SAFETY: this process is still in its single-threaded initialization phase.
            unsafe {
                env::set_var("PATH", new_path);
            }
        }
    }
}

fn remove_owned_socket(socket: &Path) {
    if let Ok(metadata) = fs::symlink_metadata(socket)
        && metadata.file_type().is_socket()
    {
        let _ = fs::remove_file(socket);
    }
}

fn endpoint_would_block(error: &EndpointError) -> bool {
    matches!(
        error,
        EndpointError::Ipc(IpcError::Io(source)) if source.kind() == io::ErrorKind::WouldBlock
    )
}

fn listener_would_block(error: &EndpointListenerError) -> bool {
    matches!(
        error,
        EndpointListenerError::Ipc(IpcError::Io(source))
            if source.kind() == io::ErrorKind::WouldBlock
    )
}

// Use the presentation clock's output when known. Until feedback arrives,
// the most recently entered output is deterministic; unmapped tasks use 60 Hz.
fn select_surface_refresh<T: PartialEq>(
    entered: &[T], presented: Option<&T>, mut refresh: impl FnMut(&T) -> Option<u32>,
) -> u32 {
    presented.filter(|output| entered.contains(output))
        .and_then(&mut refresh)
        .or_else(|| entered.iter().rev().find_map(refresh))
        .unwrap_or(60_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_tracks_surface_membership_presentation_moves_and_mode_changes() {
        let mode = |output: &u8| match output { 1 => Some(240_000), 2 => Some(143_973), _ => None };
        assert_eq!(select_surface_refresh(&[2], None, mode), 143_973);
        assert_eq!(select_surface_refresh(&[1, 2], Some(&1), mode), 240_000);
        assert_eq!(select_surface_refresh(&[1, 2], Some(&2), mode), 143_973);
        assert_eq!(select_surface_refresh(&[2], Some(&1), mode), 143_973);
        assert_eq!(select_surface_refresh(&[1, 2], None, mode), 143_973);
        assert_eq!(select_surface_refresh(&[2], Some(&2), |_| Some(120_000)), 120_000);
        assert_eq!(select_surface_refresh(&[2], Some(&2), |_| None), 60_000);
        assert_eq!(select_surface_refresh(&[], Some(&1), mode), 60_000);
    }

    #[test]
    fn idle_wait_has_no_clipboard_deadline_but_release_fallback_remains_bounded() {
        assert_eq!(poll_timeout_millis(false), -1);
        assert_eq!(poll_timeout_millis(true), 4);
    }

    #[test]
    fn touch_tap_releases_at_down_position_without_motion() {
        let contact = Contact {
            object: TaskObjectId(1),
            pointer_id: 7,
            position_fixed: (123 * 256 + 64, 456 * 256 + 128),
        };
        for action in [TouchAction::Down, TouchAction::Up] {
            assert_eq!(
                contact.event(action),
                InputEvent::Touch {
                    action,
                    pointer_id: 7,
                    x_fixed: 123 * 256 + 64,
                    y_fixed: 456 * 256 + 128,
                    pressure: if action == TouchAction::Up {
                        0
                    } else {
                        u16::MAX
                    },
                }
            );
        }
    }

    #[test]
    fn touch_drag_releases_at_latest_position_independently_per_finger() {
        let mut first = Contact {
            object: TaskObjectId(1),
            pointer_id: 0,
            position_fixed: (100 * 256, 200 * 256),
        };
        let second = Contact {
            pointer_id: 1,
            position_fixed: (300 * 256, 400 * 256),
            ..first
        };
        first.position_fixed = (-10 * 256, 500 * 256);
        for (contact, pointer_id, x_fixed, y_fixed) in [
            (first, 0, -10 * 256, 500 * 256),
            (second, 1, 300 * 256, 400 * 256),
        ] {
            assert_eq!(
                contact.event(TouchAction::Up),
                InputEvent::Touch {
                    action: TouchAction::Up,
                    pointer_id,
                    x_fixed,
                    y_fixed,
                    pressure: 0,
                }
            );
        }
    }

    #[test]
    fn fractional_scale_rounds_logical_dimensions_to_physical_pixels() {
        assert_eq!(scale_dimension(480, 132).unwrap(), 528);
        assert_eq!(scale_dimension(800, 132).unwrap(), 880);
        assert_eq!(scale_dimension(481, 120).unwrap(), 481);
        assert_eq!(scale_dimension(1, 180).unwrap(), 2);
    }

    #[test]
    fn fractional_scale_rejects_invalid_or_oversized_dimensions() {
        assert!(scale_dimension(480, 0).is_err());
        assert!(scale_dimension(MAX_BUFFER_DIMENSION, 240).is_err());
    }

    #[test]
    fn android_canvas_uses_oriented_physical_output_pixels() {
        assert_eq!(
            oriented_output_size((1264, 2780), wl_output::Transform::Normal),
            Some((1264, 2780))
        );
        assert_eq!(
            oriented_output_size((2780, 1264), wl_output::Transform::_90),
            Some((1264, 2780))
        );
        assert_eq!(
            oriented_output_size((0, 2780), wl_output::Transform::Normal),
            None
        );
    }

    #[test]
    fn presentation_timing_uses_host_values_and_refresh_fallback() {
        assert_eq!(
            presentation_timestamp_nanos(42, 123_456_789).unwrap(),
            42_123_456_789
        );
        assert_eq!(
            presentation_refresh_period(8_333_333, 120_000).unwrap(),
            8_333_333
        );
        assert_eq!(presentation_refresh_period(0, 120_000).unwrap(), 8_333_333);
        assert!(presentation_timestamp_nanos(0, 1_000_000_000).is_err());
        assert!(presentation_refresh_period(0, 0).is_err());
    }

    #[test]
    fn mouse_scroll_prefers_named_wheel_steps_over_pixels() {
        // A named wheel reports detents; touchpads report pixels or fractions
        // of a step.
        let scroll = |value120: i32, discrete: i32, absolute: f64| AxisScroll {
            value120,
            discrete,
            absolute,
            ..AxisScroll::default()
        };
        assert_eq!(App::scroll_steps(&scroll(360, 3, 500.0)), 3.0);
        assert_eq!(App::scroll_steps(&scroll(0, -2, 500.0)), -2.0);
        assert_eq!(App::scroll_steps(&scroll(0, 0, -25.0)), -2.5);
        assert_eq!(App::scroll_steps(&AxisScroll::default()), 0.0);
    }

    #[test]
    fn pad_buttons_stay_inside_android_generic_button_codes() {
        // BTN_6 through BTN_9 are what the bridge maps to BUTTON_7..10, the
        // codes the pen's key layouts use on a tablet that passes the pen
        // devices through to Android.
        assert_eq!(PAD_BUTTON_BASE, 0x106);
        assert_eq!(PAD_BUTTON_COUNT, 4);
        assert_eq!(PAD_BUTTON_BASE + PAD_BUTTON_COUNT - 1, 0x109);
    }
}
