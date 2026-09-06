//! Ordinary Wayland multi-window presentation for Droidloom.

#![deny(unsafe_op_in_unsafe_fn)]

mod clipboard;
mod notifications;
mod insets;
mod text_input;

use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};

use drm_fourcc::DrmFourcc;
use droidloom_denial_endpoint::{
    DenialEndpoint, DenialEndpointListener, EndpointAction, EndpointError, EndpointListenerError,
    ExpectedPeer,
};
use droidloom_denial_ipc::IpcError;
use droidloom_denial_protocol::{
    AcceptedWireFrame, BufferId, BufferMetadata, FormatModifier, FrameId, InputEvent, KeyAction,
    PlaneMetadata, TaskObjectId, TouchAction, Transform, Visibility, presentation_flag,
};
use droidloom_syncobj::{SyncobjDevice, WaylandTimeline};
use droidloom_window_policy::{LogicalSize, SessionMode, WindowPolicyPaths, WindowPolicyStore};
use gbm::{BufferObject, Device as GbmDevice, Modifier};
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
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
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::touch::TouchHandler;
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use thiserror::Error;
use wayland_client::globals::{BindError, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface, wl_touch,
};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, WEnum, backend::WaylandError};
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
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};

const BOOTSTRAP_PACKAGE: &str = "android.droidloom.bootstrap";
const TARGET_POOL_LENGTH: usize = 3;
const POLL_TIMEOUT_MILLIS: i32 = 4;
const LEFT_BUTTON: u32 = 0x110;
const FRACTIONAL_SCALE_DENOMINATOR: u32 = 120;
const MAX_BUFFER_DIMENSION: u32 = 16_384;

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
}

struct RenderTarget {
    metadata: BufferMetadata,
    planes: Vec<OwnedFd>,
    wayland: Option<wl_buffer::WlBuffer>,
    _allocation: Option<BufferObject<()>>,
    host_owned: bool,
    retired: bool,
    busy: bool,
}

struct PendingFrame {
    frame: FrameId,
    buffer: BufferId,
    release_point: u64,
}

struct TaskWindow {
    package: String,
    android_task: Option<u64>,
    window: Option<Window>,
    viewport: Option<WpViewport>,
    fractional_scale: Option<WpFractionalScaleV1>,
    sync_surface: Option<WpLinuxDrmSyncobjSurfaceV1>,
    timeline_proxy: Option<WpLinuxDrmSyncobjTimelineV1>,
    timeline: Option<WaylandTimeline>,
    logical_size: Option<(u32, u32)>,
    buffer_size: (u32, u32),
    preferred_scale_120: u32,
    configure_serial: u32,
    refresh_millihz: u32,
    next_sync_point: u64,
    targets: BTreeMap<BufferId, RenderTarget>,
    pending: VecDeque<PendingFrame>,
    presentation_feedback: Vec<(WpPresentationFeedback, FrameId)>,
    focused: bool,
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

struct App {
    clipboard: clipboard::Clipboard,
    clipboard_inputs: VecDeque<(TaskObjectId, InputEvent)>,
    text_input: text_input::TextInput,
    registry_state: RegistryState,
    output_state: OutputState,
    presentation_time: PresentationTimeState,
    seat_state: SeatState,
    dmabuf_state: DmabufState,
    feedback: Option<DmabufFeedback>,
    compositor: CompositorState,
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
    pointer: Option<wl_pointer::WlPointer>,
    touch: Option<wl_touch::WlTouch>,
    focused: Option<TaskObjectId>,
    pointer_contact: Option<Contact>,
    touch_contacts: BTreeMap<i32, Contact>,
    next_buffer_id: u64,
    next_input_serial: u64,
    socket_path: PathBuf,
    fatal: Option<String>,
}

impl Drop for App {
    fn drop(&mut self) {
        remove_owned_socket(&self.socket_path);
    }
}

impl App {
    fn task_for_surface(&self, surface: &wl_surface::WlSurface) -> Option<TaskObjectId> {
        self.tasks.iter().find_map(|(object, task)| {
            task.surface()
                .is_some_and(|candidate| candidate == surface)
                .then_some(*object)
        })
    }

    fn refresh_millihz(&self) -> u32 {
        self.output_state
            .outputs()
            .filter_map(|output| self.output_state.info(&output))
            .flat_map(|info| info.modes)
            .filter(|mode| mode.current && mode.refresh_rate > 0)
            .filter_map(|mode| u32::try_from(mode.refresh_rate).ok())
            .max()
            .unwrap_or(60_000)
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
        let (window, viewport, fractional_scale, sync_surface, timeline_proxy, timeline) =
            if headless {
                (None, None, None, None, None, None)
            } else {
                let surface = self.compositor.create_surface(qh);
                let window = self
                    .xdg_shell
                    .create_window(surface, WindowDecorations::None, qh);
                window.set_title(package.to_owned());
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
                let timeline = WaylandTimeline::create(&self.syncobj)?;
                let descriptor = timeline.export_descriptor()?;
                let timeline_proxy = self
                    .sync_manager
                    .import_timeline(descriptor.as_fd(), qh, ());
                window.commit();
                (
                    Some(window),
                    Some(viewport),
                    Some(fractional_scale),
                    Some(sync_surface),
                    Some(timeline_proxy),
                    Some(timeline),
                )
            };
        self.tasks.insert(
            object,
            TaskWindow {
                package: package.to_owned(),
                android_task: None,
                window,
                viewport,
                fractional_scale,
                sync_surface,
                timeline_proxy,
                timeline,
                logical_size: None,
                buffer_size: (0, 0),
                preferred_scale_120: FRACTIONAL_SCALE_DENOMINATOR,
                configure_serial: 0,
                refresh_millihz: self.refresh_millihz(),
                next_sync_point: 0,
                targets: BTreeMap::new(),
                pending: VecDeque::new(),
                presentation_feedback: Vec::new(),
                focused: false,
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
            let target = self.allocate_target(qh, id, buffer_width, buffer_height)?;
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

        let refresh = self.refresh_millihz();
        {
            let task = self
                .tasks
                .get_mut(&object)
                .ok_or(PresenterError::UnknownTask(object))?;
            task.logical_size = Some((width, height));
            task.buffer_size = (buffer_width, buffer_height);
            task.configure_serial = serial;
            task.refresh_millihz = refresh;
            task.targets.extend(
                targets
                    .into_iter()
                    .map(|target| (target.metadata.id, target)),
            );
            if let Some(viewport) = task.viewport.as_ref() {
                let destination_width = i32::try_from(width)
                    .map_err(|_| PresenterError::Configuration("logical width exceeds Wayland"))?;
                let destination_height = i32::try_from(height)
                    .map_err(|_| PresenterError::Configuration("logical height exceeds Wayland"))?;
                viewport.set_destination(destination_width, destination_height);
            }
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
        Ok(())
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
            _allocation: Some(allocation),
            host_owned: true,
            retired: false,
            busy: false,
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
        task.targets.insert(
            metadata.id,
            RenderTarget {
                metadata,
                planes,
                wayland,
                _allocation: None,
                host_owned: false,
                retired: false,
                busy: false,
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
            self.tasks
                .get_mut(&object)
                .and_then(|task| task.targets.get_mut(&frame.buffer))
                .expect("target was just checked")
                .wayland = Some(buffer);
        }
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        let surface = task
            .surface()
            .cloned()
            .expect("accepts_present guaranteed a mapped window");
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
        let acquire_point =
            task.next_sync_point
                .checked_add(1)
                .ok_or(PresenterError::Configuration(
                    "Wayland sync point exhausted",
                ))?;
        let release_point = acquire_point
            .checked_add(1)
            .ok_or(PresenterError::Configuration(
                "Wayland sync point exhausted",
            ))?;
        task.next_sync_point = release_point;
        task.timeline
            .as_mut()
            .ok_or(PresenterError::Configuration("window timeline is absent"))?
            .import_acquire_fence(acquire_point, acquire_fence.as_fd())?;
        let sync_surface = task
            .sync_surface
            .as_ref()
            .ok_or(PresenterError::Configuration(
                "window sync surface is absent",
            ))?;
        let timeline_proxy = task
            .timeline_proxy
            .as_ref()
            .ok_or(PresenterError::Configuration(
                "window timeline proxy is absent",
            ))?;
        let (acquire_hi, acquire_lo) = split_point(acquire_point);
        let (release_hi, release_lo) = split_point(release_point);
        sync_surface.set_acquire_point(timeline_proxy, acquire_hi, acquire_lo);
        sync_surface.set_release_point(timeline_proxy, release_hi, release_lo);
        let feedback = self.presentation_time.feedback(&surface, qh)?;
        surface.attach(target.wayland.as_ref(), 0, 0);
        for damage in &frame.damage {
            surface.damage_buffer(
                i32::try_from(damage.x).unwrap_or(i32::MAX),
                i32::try_from(damage.y).unwrap_or(i32::MAX),
                i32::try_from(damage.width).unwrap_or(i32::MAX),
                i32::try_from(damage.height).unwrap_or(i32::MAX),
            );
        }
        surface.commit();
        target.busy = true;
        task.pending.push_back(PendingFrame {
            frame: frame.frame,
            buffer: frame.buffer,
            release_point,
        });
        task.presentation_feedback.push((feedback, frame.frame));
        Ok(())
    }

    fn release_ready_frames(&mut self) -> Result<(), PresenterError> {
        let mut ready = Vec::new();
        for (object, task) in &mut self.tasks {
            let Some(timeline) = task.timeline.as_ref() else {
                continue;
            };
            let signalled = timeline.signalled_point()?;
            while task.pending.front().is_some_and(|pending| {
                pending.release_point <= signalled
                    && !task
                        .presentation_feedback
                        .iter()
                        .any(|(_, frame)| *frame == pending.frame)
            }) {
                let pending = task.pending.pop_front().expect("front was present");
                ready.push((*object, pending));
            }
        }
        for (object, pending) in ready {
            self.endpoint
                .as_mut()
                .ok_or(PresenterError::Configuration("endpoint disappeared"))?
                .finish_release(object, pending.frame)?;
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
                (task.closing && task.pending.is_empty()).then_some(*object)
            })
            .collect::<Vec<_>>();
        for object in finished {
            self.remove_task(object);
        }
        Ok(())
    }

    fn remove_task(&mut self, object: TaskObjectId) {
        if let Some(mut task) = self.tasks.remove(&object) {
            if let Some(fractional_scale) = task.fractional_scale.take() {
                fractional_scale.destroy();
            }
            if let Some(viewport) = task.viewport.take() {
                viewport.destroy();
            }
            if let Some(surface) = task.sync_surface.take() {
                surface.destroy();
            }
            if let Some(timeline) = task.timeline_proxy.take() {
                timeline.destroy();
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
    }

    fn unmap_task_window(&mut self, object: TaskObjectId) -> Result<(), PresenterError> {
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
        task.presentation_feedback.clear();
        if let Some(fractional_scale) = task.fractional_scale.take() {
            fractional_scale.destroy();
        }
        if let Some(viewport) = task.viewport.take() {
            viewport.destroy();
        }
        if let Some(sync_surface) = task.sync_surface.take() {
            sync_surface.destroy();
        }
        if let Some(timeline) = task.timeline_proxy.take() {
            timeline.destroy();
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
                .is_some_and(|task| task.closing && task.pending.is_empty());
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
            EndpointAction::ClientReady
            | EndpointAction::TimelinesBound { .. }
            | EndpointAction::ConfigureAcknowledged { .. }
            | EndpointAction::SupersededPresentDiscarded { .. }
            | EndpointAction::Pong { .. }
            | EndpointAction::SetContentState { .. }
            | EndpointAction::SetFrameRate { .. } => {}
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
        if self.clipboard.blocked() && self.clipboard_inputs.len() < 512 {
            self.clipboard_inputs.push_back((object, event));
            return Ok(());
        }
        self.next_input_serial = self
            .next_input_serial
            .checked_add(1)
            .ok_or(PresenterError::Configuration("input serial exhausted"))?;
        let timestamp = monotonic_timestamp_nanos()?;
        if let InputEvent::Key {
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
        Ok(())
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

    fn fail(&mut self, error: &dyn ToString) {
        self.fatal = Some(error.to_string());
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
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
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
        let (package, previous, headless) = self.tasks.get(&object).map_or_else(
            || (String::new(), None, false),
            |task| (task.package.clone(), task.logical_size, task.headless()),
        );
        let requested_width = configure.new_size.0.map(NonZeroU32::get);
        let requested_height = configure.new_size.1.map(NonZeroU32::get);
        let size = previous.map_or_else(
            || {
                let bounds = configure
                    .suggested_bounds
                    .and_then(|(width, height)| LogicalSize::new(width, height).ok());
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
        );
        if let Err(error) = self.configure_task(qh, object, size.width, size.height) {
            self.fail(&error);
        } else if !headless
            && previous.is_some()
            && !configure.is_resizing()
            && !configure.is_maximized()
            && !configure.is_fullscreen()
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
        _params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        _buffer: wl_buffer::WlBuffer,
    ) {
    }

    fn failed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    ) {
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
        if let Err(error) = self.reconfigure_bootstrap(qh) {
            self.fail(&error);
        }
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        if let Err(error) = self.reconfigure_bootstrap(qh) {
            self.fail(&error);
        }
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
        if let Err(error) = self.reconfigure_bootstrap(qh) {
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
        _outputs: Vec<wl_output::WlOutput>,
        time: PresentTime,
        refresh: u32,
        sequence: u64,
        flags: WEnum<wp_presentation_feedback::Kind>,
    ) {
        let Some(object) = self.task_for_surface(surface) else {
            return;
        };
        let result = (|| {
            if time.clk_id != u32::try_from(libc::CLOCK_MONOTONIC).unwrap_or_default() {
                return Err(PresenterError::Configuration(
                    "presentation clock is not CLOCK_MONOTONIC",
                ));
            }
            let (frame, advertised_refresh) = {
                let task = self
                    .tasks
                    .get_mut(&object)
                    .ok_or(PresenterError::UnknownTask(object))?;
                let index = task
                    .presentation_feedback
                    .iter()
                    .position(|(candidate, _)| candidate == feedback)
                    .ok_or(PresenterError::Configuration(
                        "presentation feedback has no submitted frame",
                    ))?;
                let (_, frame) = task.presentation_feedback.swap_remove(index);
                (frame, task.refresh_millihz)
            };
            let timestamp_nanos = presentation_timestamp_nanos(time.tv_sec, time.tv_nsec)?;
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
                .as_ref()
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
            .position(|(candidate, _)| candidate == feedback)
        {
            task.presentation_feedback.swap_remove(index);
        }
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

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
                .map(|keyboard| self.keyboard = Some(keyboard))
                .map_err(|error| error.to_string()),
            Capability::Pointer if self.pointer.is_none() => self
                .seat_state
                .get_pointer(qh, &seat)
                .map(|pointer| self.pointer = Some(pointer))
                .map_err(|error| error.to_string()),
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
                self.text_input.remove_resource();
            }
            Capability::Pointer => {
                self.pointer = None;
                self.pointer_contact = None;
            }
            Capability::Touch => {
                self.touch = None;
                self.touch_contacts.clear();
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
        self.text_input.remove_resource();
        self.keyboard = None;
        self.pointer = None;
        self.touch = None;
        self.pointer_contact = None;
        self.touch_contacts.clear();
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
        if self.focused == self.task_for_surface(surface)
            && let Err(error) = self.set_focus(None)
        {
            self.fail(&error);
        }
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        serial: u32,
        event: KeyEvent,
    ) {
        self.clipboard.serial(serial);
        if let Some(object) = self.focused
            && let Err(error) = self.send_input(
                object,
                InputEvent::Key {
                    action: KeyAction::Down,
                    keycode: event.raw_code,
                    repeat: 0,
                },
            )
        {
            self.fail(&error);
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
        if let Some(object) = self.focused
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
        _modifiers: Modifiers,
        _raw_modifiers: RawModifiers,
        _layout: u32,
    ) {
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
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if let PointerEventKind::Press { serial, .. } | PointerEventKind::Release { serial, .. } = event.kind { self.clipboard.serial(serial); }
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
        _serial: u32,
        _time: u32,
        surface: wl_surface::WlSurface,
        id: i32,
        position: (f64, f64),
    ) {
        let Some(object) = self.task_for_surface(&surface) else {
            return;
        };
        let Ok(pointer_id) = u32::try_from(id) else {
            return;
        };
        self.text_input.note_touch();
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
        _time: u32,
        id: i32,
    ) {
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
smithay_client_toolkit::delegate_touch!(App);
smithay_client_toolkit::delegate_registry!(App);

wayland_client::delegate_noop!(App: ignore WpLinuxDrmSyncobjManagerV1);
wayland_client::delegate_noop!(App: ignore WpLinuxDrmSyncobjSurfaceV1);
wayland_client::delegate_noop!(App: ignore WpLinuxDrmSyncobjTimelineV1);
wayland_client::delegate_noop!(App: ignore WpFractionalScaleManagerV1);
wayland_client::delegate_noop!(App: ignore WpViewporter);
wayland_client::delegate_noop!(App: ignore WpViewport);

fn main() {
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
    let _notifications = notifications::Bridge::start(socket_path.with_file_name("notifications.sock"))?;

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
    let compositor = CompositorState::bind(&globals, &qh)?;
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let viewporter: WpViewporter = globals.bind(&qh, 1..=1, ())?;
    let fractional_scale_manager: WpFractionalScaleManagerV1 = globals.bind(&qh, 1..=1, ())?;
    let dmabuf_state = DmabufState::new(&globals, &qh);
    let presentation_time = PresentationTimeState::bind(&globals, &qh);
    if !matches!(dmabuf_state.version(), Some(4..)) {
        return Err(PresenterError::DmabufV4Required);
    }
    let sync_manager: WpLinuxDrmSyncobjManagerV1 = globals.bind(&qh, 1..=1, ())?;
    let mut app = App {
        clipboard: clipboard::Clipboard::new(socket_path.with_file_name("clipboard.sock"), &globals, &qh)?,
        clipboard_inputs: VecDeque::new(),
        text_input: text_input::TextInput::new(
            socket_path.with_file_name("text-input.sock"),
            globals.bind(&qh, 1..=1, ()).ok(),
        )?,
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        presentation_time,
        seat_state: SeatState::new(&globals, &qh),
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
        pointer: None,
        touch: None,
        focused: None,
        pointer_contact: None,
        touch_contacts: BTreeMap::new(),
        next_buffer_id: 0,
        next_input_serial: 0,
        socket_path,
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
    loop {
        event_queue.dispatch_pending(&mut app)?;
        app.unmap_requested_tasks()?;
        if let Some(error) = app.fatal.take() {
            return Err(PresenterError::Wayland(error));
        }
        app.accept_endpoint()?;
        app.drain_endpoint(&qh)?;
        app.pump_text_input();
        app.clipboard.pump(&qh);
        if !app.clipboard.blocked() {
            while let Some((object, input)) = app.clipboard_inputs.pop_front() {
                if app.tasks.contains_key(&object) { app.send_input(object, input)?; }
            }
        }
        app.unmap_requested_tasks()?;
        app.release_ready_frames()?;
        poll_sources(conn, &mut event_queue, &mut app)?;
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
) -> Result<(), PresenterError> {
    match event_queue.flush() {
        Ok(()) => {}
        Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
        Err(error) => return Err(PresenterError::Wayland(error.to_string())),
    }
    let read_guard = event_queue.prepare_read();
    let mut descriptors = vec![
        libc::pollfd {
            fd: conn.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
        libc::pollfd {
            fd: app.listener.listener().as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    if let Some(endpoint) = app.endpoint.as_ref() {
        descriptors.push(libc::pollfd {
            fd: endpoint.socket().socket().as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }
    for (fd, events) in app.text_input.fds().into_iter().chain(app.clipboard.fds()) {
        descriptors.push(libc::pollfd {
            fd,
            events,
            revents: 0,
        });
    }
    // SAFETY: `descriptors` is live writable storage for exactly its length;
    // poll retains no pointer after returning.
    let result = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            libc::nfds_t::try_from(descriptors.len()).unwrap_or(libc::nfds_t::MAX),
            POLL_TIMEOUT_MILLIS,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
    }
    app.clipboard.notify_ready(&descriptors);
    if descriptors[0].revents & libc::POLLIN != 0 {
        if let Some(read_guard) = read_guard {
            match read_guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(PresenterError::Wayland(error.to_string())),
            }
        }
    } else {
        drop(read_guard);
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
