//! End-to-end GLES → DMA-BUF → Denial DRM-syncobj presentation probe.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use clap::Parser;
use drm::control::{Device as ControlDevice, syncobj};
use drm_fourcc::{DrmFourcc, DrmModifier};
use droidloom_transport::{
    BufferId, BufferMetadata, Configure, Damage, FormatModifier, FrameId, PlaneMetadata,
    PresentationState,
};
use gbm::{BufferObject, Device as GbmDevice, Modifier};
use serde::Serialize;
use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::registry_handlers;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use thiserror::Error;
use wayland_client::globals::{BindError, registry_queue_init};
use wayland_client::protocol::{wl_buffer, wl_output, wl_surface};
use wayland_client::{Connection, QueueHandle};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1,
};
use wayland_protocols::wp::linux_drm_syncobj::v1::client::{
    wp_linux_drm_syncobj_manager_v1::WpLinuxDrmSyncobjManagerV1,
    wp_linux_drm_syncobj_surface_v1::WpLinuxDrmSyncobjSurfaceV1,
    wp_linux_drm_syncobj_timeline_v1::WpLinuxDrmSyncobjTimelineV1,
};

const ACQUIRE_POINT: u64 = 1;
const RELEASE_POINT: u64 = 2;
const VISIBLE_FRAME_CALLBACKS: u32 = 120;

#[derive(Debug, Parser)]
#[command(name = "droidloom-dmabuf-probe", version, about)]
struct Cli {
    /// DRM render node; card/KMS nodes are rejected.
    #[arg(long, default_value = "/dev/dri/renderD128")]
    render_node: PathBuf,
    /// Number of presentation-paced callbacks before cleanly detaching.
    #[arg(long, default_value_t = VISIBLE_FRAME_CALLBACKS)]
    frame_callbacks: u32,
}

#[derive(Debug)]
struct DrmNode(File);

impl AsFd for DrmNode {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl drm::Device for DrmNode {}
impl ControlDevice for DrmNode {}

impl DrmNode {
    fn try_clone(&self) -> std::io::Result<Self> {
        self.0.try_clone().map(Self)
    }
}

#[repr(C)]
struct NativeEglRenderer {
    _opaque: [u8; 0],
}

unsafe extern "C" {
    fn droidloom_egl_renderer_create(
        gbm_device: *mut c_void,
        error: *mut c_char,
        error_size: usize,
    ) -> *mut NativeEglRenderer;
    fn droidloom_egl_renderer_name(renderer: *const NativeEglRenderer) -> *const c_char;
    fn droidloom_egl_render_dmabuf(
        renderer: *mut NativeEglRenderer,
        width: c_int,
        height: c_int,
        fourcc: u32,
        modifier: u64,
        dma_buf_fd: c_int,
        offset: u32,
        stride: u32,
        fence_fd: *mut c_int,
        error: *mut c_char,
        error_size: usize,
    ) -> c_int;
    fn droidloom_egl_renderer_destroy(renderer: *mut NativeEglRenderer);
    fn droidloom_import_sync_file(
        drm_fd: c_int,
        syncobj_handle: u32,
        sync_file_fd: c_int,
        error: *mut c_char,
        error_size: usize,
    ) -> c_int;
}

#[derive(Debug)]
struct EglRenderer(NonNull<NativeEglRenderer>);

#[derive(Clone, Copy)]
struct EglTarget<'a> {
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
    dma_buf: &'a OwnedFd,
    offset: u32,
    stride: u32,
}

impl EglRenderer {
    fn new(gbm_device: *mut c_void) -> Result<Self, ProbeError> {
        let mut error = [0_i8; 512];
        // SAFETY: the GBM device outlives this EGL wrapper and the error array is writable.
        let renderer =
            unsafe { droidloom_egl_renderer_create(gbm_device, error.as_mut_ptr(), error.len()) };
        NonNull::new(renderer)
            .map(Self)
            .ok_or_else(|| ProbeError::Egl(c_error(&error)))
    }

    fn renderer_name(&self) -> String {
        // SAFETY: the C renderer owns a NUL-terminated fixed-size name for its lifetime.
        let name = unsafe { droidloom_egl_renderer_name(self.0.as_ptr()) };
        if name.is_null() {
            "unknown".to_owned()
        } else {
            // SAFETY: a non-null name is NUL-terminated by the C renderer.
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned()
        }
    }

    fn render(&mut self, target: EglTarget<'_>) -> Result<OwnedFd, ProbeError> {
        let width = i32::try_from(target.width).map_err(|_| ProbeError::Dimension)?;
        let height = i32::try_from(target.height).map_err(|_| ProbeError::Dimension)?;
        let mut fence = -1;
        let mut error = [0_i8; 512];
        // SAFETY: all pointers are valid for the call; C borrows the DMA-BUF and returns
        // ownership of a newly duplicated native-fence FD on success.
        let result = unsafe {
            droidloom_egl_render_dmabuf(
                self.0.as_ptr(),
                width,
                height,
                target.fourcc,
                target.modifier,
                target.dma_buf.as_raw_fd(),
                target.offset,
                target.stride,
                &raw mut fence,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if result != 0 || fence < 0 {
            Err(ProbeError::Egl(c_error(&error)))
        } else {
            // SAFETY: successful C return transfers one valid, newly duplicated FD.
            Ok(unsafe { OwnedFd::from_raw_fd(fence) })
        }
    }
}

impl Drop for EglRenderer {
    fn drop(&mut self) {
        // SAFETY: the pointer is unique and was returned by the matching constructor.
        unsafe { droidloom_egl_renderer_destroy(self.0.as_ptr()) };
    }
}

fn c_error(buffer: &[c_char]) -> String {
    // SAFETY: all C error writers use snprintf into this initialized fixed-size array.
    unsafe { CStr::from_ptr(buffer.as_ptr()) }
        .to_string_lossy()
        .into_owned()
}

#[derive(Debug, Error)]
enum ProbeError {
    #[error("render node must be /dev/dri/renderD<number>, not {0}")]
    RenderNodePolicy(PathBuf),
    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),
    #[error("Wayland connection failure: {0}")]
    Connect(#[from] wayland_client::ConnectError),
    #[error("Wayland global enumeration failure: {0}")]
    Global(#[from] wayland_client::globals::GlobalError),
    #[error("Wayland toolkit global failure: {0}")]
    ToolkitGlobal(#[from] smithay_client_toolkit::error::GlobalError),
    #[error("Wayland bind failure: {0}")]
    Bind(#[from] BindError),
    #[error("Wayland dispatch failure: {0}")]
    Dispatch(#[from] wayland_client::DispatchError),
    #[error("EGL/GLES failure: {0}")]
    Egl(String),
    #[error("Denial does not advertise linux-dmabuf v4 feedback")]
    DmabufV4Required,
    #[error("Denial feedback lacks linear XRGB8888 import")]
    MissingLinearXrgb,
    #[error("GBM allocated {0} planes; the minimum probe requires exactly one")]
    PlaneCount(u32),
    #[error("configured dimensions exceed signed Wayland/EGL bounds")]
    Dimension,
    #[error("DRM syncobj operation failed: {0}")]
    Syncobj(String),
    #[error("release point was not signalled within five seconds")]
    ReleaseTimeout,
    #[error("transport invariant failed: {0}")]
    Transport(#[from] droidloom_transport::TransportError),
    #[error("probe callback failed: {0}")]
    Callback(String),
}

#[derive(Debug, Serialize)]
struct ProbeEvidence {
    schema_version: u32,
    render_node: PathBuf,
    gbm_backend: String,
    gl_renderer: String,
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
    plane_count: u32,
    stride: u32,
    producer: &'static str,
    acquire_fence: &'static str,
    compositor_sync: &'static str,
    release_observed: bool,
    cpu_readback: bool,
    steady_state_cpu_copies: u32,
    drm_card_opened: bool,
    frame_callbacks: u32,
}

struct App {
    registry_state: RegistryState,
    output_state: OutputState,
    dmabuf_state: DmabufState,
    feedback: Option<DmabufFeedback>,
    window: Option<Window>,
    sync_surface: Option<WpLinuxDrmSyncobjSurfaceV1>,
    timeline: Option<WpLinuxDrmSyncobjTimelineV1>,
    timeline_handle: Option<syncobj::Handle>,
    drm: DrmNode,
    gbm: GbmDevice<File>,
    egl: EglRenderer,
    bo: Option<BufferObject<()>>,
    buffer: Option<wl_buffer::WlBuffer>,
    transport: PresentationState,
    submitted_frame: Option<FrameId>,
    render_node: PathBuf,
    target_callbacks: u32,
    callbacks: u32,
    detached: bool,
    exit: bool,
    fatal: Option<String>,
    evidence: Option<ProbeEvidence>,
}

impl App {
    fn fail(&mut self, error: String) {
        self.fatal = Some(error);
        self.exit = true;
    }

    fn present(&mut self, qh: &QueueHandle<Self>, serial: u32, width: u32, height: u32) {
        if let Err(error) = self.try_present(qh, serial, width, height) {
            self.fail(error.to_string());
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the one-shot protocol transaction is intentionally kept in wire order"
    )]
    fn try_present(
        &mut self,
        qh: &QueueHandle<Self>,
        serial: u32,
        width: u32,
        height: u32,
    ) -> Result<(), ProbeError> {
        if self.buffer.is_some() {
            return Ok(());
        }
        let feedback = self.feedback.as_ref().ok_or(ProbeError::DmabufV4Required)?;
        let fourcc = DrmFourcc::Xrgb8888 as u32;
        let modifier = u64::from(DrmModifier::Linear);
        let advertised = feedback.tranches().iter().any(|tranche| {
            tranche.formats.iter().any(|index| {
                feedback
                    .format_table()
                    .get(*index as usize)
                    .is_some_and(|entry| entry.format == fourcc && entry.modifier == modifier)
            })
        });
        if !advertised {
            return Err(ProbeError::MissingLinearXrgb);
        }

        let bo = self
            .gbm
            .create_buffer_object_with_modifiers::<()>(
                width,
                height,
                DrmFourcc::Xrgb8888,
                [Modifier::Linear].into_iter(),
            )
            .map_err(|error| ProbeError::Callback(format!("GBM allocation: {error}")))?;
        if bo.plane_count() != 1 {
            return Err(ProbeError::PlaneCount(bo.plane_count()));
        }
        let actual_modifier = u64::from(bo.modifier());
        if actual_modifier != modifier {
            return Err(ProbeError::Callback(format!(
                "GBM returned modifier {actual_modifier:#018x}, expected linear"
            )));
        }
        let dma_buf = bo
            .fd_for_plane(0)
            .map_err(|error| ProbeError::Callback(format!("DMA-BUF export: {error}")))?;
        let acquire_sync_file = self.egl.render(EglTarget {
            width,
            height,
            fourcc,
            modifier: actual_modifier,
            dma_buf: &dma_buf,
            offset: bo.offset(0),
            stride: bo.stride_for_plane(0),
        })?;

        let timeline_handle = self
            .timeline_handle
            .ok_or_else(|| ProbeError::Callback("timeline handle is absent".into()))?;
        let imported_acquire = self
            .drm
            .create_syncobj(false)
            .map_err(|error| ProbeError::Syncobj(format!("create acquire syncobj: {error}")))?;
        import_sync_file(&self.drm, imported_acquire, &acquire_sync_file)?;
        self.drm
            .syncobj_timeline_transfer(imported_acquire, timeline_handle, 0, ACQUIRE_POINT)
            .map_err(|error| {
                ProbeError::Syncobj(format!("transfer acquire point into timeline: {error}"))
            })?;
        self.drm
            .destroy_syncobj(imported_acquire)
            .map_err(|error| ProbeError::Syncobj(format!("destroy acquire syncobj: {error}")))?;
        let window = self.window.as_ref().ok_or_else(|| {
            ProbeError::Callback("window disappeared before first present".into())
        })?;
        let sync_manager = self
            .sync_surface
            .as_ref()
            .ok_or_else(|| ProbeError::Callback("sync surface is absent".into()))?;
        let timeline_proxy = self
            .timeline
            .as_ref()
            .ok_or_else(|| ProbeError::Callback("sync timeline proxy is absent".into()))?;

        let params = self
            .dmabuf_state
            .create_params(qh)
            .map_err(|error| ProbeError::Callback(format!("create DMA-BUF params: {error}")))?;
        params.add(
            dma_buf.as_fd(),
            0,
            bo.offset(0),
            bo.stride_for_plane(0),
            actual_modifier,
        );
        let width_i32 = i32::try_from(width).map_err(|_| ProbeError::Dimension)?;
        let height_i32 = i32::try_from(height).map_err(|_| ProbeError::Dimension)?;
        let (buffer, params_proxy) = params.create_immed(
            width_i32,
            height_i32,
            fourcc,
            zwp_linux_buffer_params_v1::Flags::empty(),
            qh,
        );
        params_proxy.destroy();

        self.transport.replace_feedback([FormatModifier {
            fourcc,
            modifier: actual_modifier,
        }]);
        self.transport.configure(Configure {
            serial,
            width,
            height,
            refresh_millihz: 0,
        });
        self.transport.acknowledge(serial)?;
        let metadata = BufferMetadata {
            id: BufferId(1),
            width,
            height,
            format: FormatModifier {
                fourcc,
                modifier: actual_modifier,
            },
            planes: vec![PlaneMetadata {
                index: 0,
                offset: bo.offset(0),
                stride: bo.stride_for_plane(0),
            }],
        };
        let accepted = self.transport.submit(
            metadata,
            vec![Damage {
                x: 0,
                y: 0,
                width,
                height,
            }],
            true,
        )?;

        let (acquire_hi, acquire_lo) = split_point(ACQUIRE_POINT);
        let (release_hi, release_lo) = split_point(RELEASE_POINT);
        sync_manager.set_acquire_point(timeline_proxy, acquire_hi, acquire_lo);
        sync_manager.set_release_point(timeline_proxy, release_hi, release_lo);
        window.wl_surface().attach(Some(&buffer), 0, 0);
        window
            .wl_surface()
            .damage_buffer(0, 0, width_i32, height_i32);
        window.wl_surface().frame(qh, window.wl_surface().clone());
        window.commit();

        self.evidence = Some(ProbeEvidence {
            schema_version: 1,
            render_node: self.render_node.clone(),
            gbm_backend: self.gbm.backend_name().to_owned(),
            gl_renderer: self.egl.renderer_name(),
            width,
            height,
            fourcc,
            modifier: actual_modifier,
            plane_count: bo.plane_count(),
            stride: bo.stride_for_plane(0),
            producer: "OpenGL ES framebuffer via EGL_EXT_image_dma_buf_import",
            acquire_fence: "EGL_ANDROID_native_fence_sync -> DRM syncobj timeline",
            compositor_sync: "wp_linux_drm_syncobj_v1",
            release_observed: false,
            cpu_readback: false,
            steady_state_cpu_copies: 0,
            drm_card_opened: false,
            frame_callbacks: 0,
        });
        self.submitted_frame = Some(accepted.frame_id);
        self.bo = Some(bo);
        self.buffer = Some(buffer);
        Ok(())
    }

    fn finish_release(&mut self) -> Result<(), ProbeError> {
        let handle = self
            .timeline_handle
            .ok_or_else(|| ProbeError::Callback("timeline handle is absent".into()))?;
        let drm = self.drm.try_clone()?;
        let (sender, receiver) = mpsc::sync_channel(1);
        std::thread::spawn(move || {
            let result =
                drm.syncobj_timeline_wait(&[handle], &[RELEASE_POINT], i64::MAX, true, true, false);
            let _ = sender.send(result);
        });
        let result = receiver
            .recv_timeout(Duration::from_secs(5))
            .map_err(|_| ProbeError::ReleaseTimeout)?;
        result.map_err(|error| ProbeError::Syncobj(error.to_string()))?;
        let frame = self
            .submitted_frame
            .take()
            .ok_or_else(|| ProbeError::Callback("submitted frame is absent".into()))?;
        self.transport.release(frame, true)?;
        self.drm
            .destroy_syncobj(handle)
            .map_err(|error| ProbeError::Syncobj(error.to_string()))?;
        self.timeline_handle = None;
        if let Some(evidence) = &mut self.evidence {
            evidence.release_observed = true;
            evidence.frame_callbacks = self.callbacks;
        }
        self.exit = true;
        Ok(())
    }
}

fn split_point(point: u64) -> (u32, u32) {
    let high = u32::try_from(point >> 32).unwrap_or_default();
    let low = u32::try_from(point & u64::from(u32::MAX)).unwrap_or_default();
    (high, low)
}

fn import_sync_file(
    drm: &DrmNode,
    handle: syncobj::Handle,
    sync_file: &OwnedFd,
) -> Result<(), ProbeError> {
    let mut error = [0_i8; 512];
    // SAFETY: both descriptors and the process-local syncobj handle are valid for
    // the ioctl; C only writes inside the provided bounded error array.
    let result = unsafe {
        droidloom_import_sync_file(
            drm.as_fd().as_raw_fd(),
            u32::from(handle),
            sync_file.as_raw_fd(),
            error.as_mut_ptr(),
            error.len(),
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(ProbeError::Syncobj(c_error(&error)))
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
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
        qh: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        self.callbacks = self.callbacks.saturating_add(1);
        if self.callbacks < self.target_callbacks {
            surface.frame(qh, surface.clone());
            surface.commit();
        } else if !self.detached {
            self.detached = true;
            surface.attach(None, 0, 0);
            surface.frame(qh, surface.clone());
            surface.commit();
        } else if let Err(error) = self.finish_release() {
            self.fail(error.to_string());
        }
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

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl WindowHandler for App {
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _window: &Window) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        serial: u32,
    ) {
        let width = configure.new_size.0.map_or(1080, std::num::NonZeroU32::get);
        let height = configure.new_size.1.map_or(1920, std::num::NonZeroU32::get);
        self.present(qh, serial, width, height);
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
        self.fail("Denial rejected the DMA-BUF import".to_owned());
    }

    fn released(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _buffer: &wl_buffer::WlBuffer,
    ) {
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState];
}

smithay_client_toolkit::delegate_compositor!(App);
smithay_client_toolkit::delegate_output!(App);
smithay_client_toolkit::delegate_xdg_shell!(App);
smithay_client_toolkit::delegate_xdg_window!(App);
smithay_client_toolkit::delegate_dmabuf!(App);
smithay_client_toolkit::delegate_registry!(App);

wayland_client::delegate_noop!(App: ignore WpLinuxDrmSyncobjManagerV1);
wayland_client::delegate_noop!(App: ignore WpLinuxDrmSyncobjSurfaceV1);
wayland_client::delegate_noop!(App: ignore WpLinuxDrmSyncobjTimelineV1);

fn main() {
    if let Err(error) = run() {
        eprintln!("droidloom-dmabuf-probe: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), ProbeError> {
    let cli = Cli::parse();
    validate_render_node(&cli.render_node)?;
    let render_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&cli.render_node)?;
    let gbm_file = render_file.try_clone()?;
    let drm = DrmNode(render_file);
    let gbm = GbmDevice::new(gbm_file)?;
    let gbm_pointer = gbm::AsRaw::<gbm_sys::gbm_device>::as_raw(&gbm)
        .cast_mut()
        .cast();
    let egl = EglRenderer::new(gbm_pointer)?;

    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)?;
    let xdg_shell = XdgShell::bind(&globals, &qh)?;
    let dmabuf_state = DmabufState::new(&globals, &qh);
    if !matches!(dmabuf_state.version(), Some(4..)) {
        return Err(ProbeError::DmabufV4Required);
    }
    let sync_manager: WpLinuxDrmSyncobjManagerV1 = globals.bind(&qh, 1..=1, ())?;

    let mut app = App {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
        dmabuf_state,
        feedback: None,
        window: None,
        sync_surface: None,
        timeline: None,
        timeline_handle: None,
        drm,
        gbm,
        egl,
        bo: None,
        buffer: None,
        transport: PresentationState::default(),
        submitted_frame: None,
        render_node: cli.render_node,
        target_callbacks: cli.frame_callbacks.max(1),
        callbacks: 0,
        detached: false,
        exit: false,
        fatal: None,
        evidence: None,
    };

    app.dmabuf_state.get_default_feedback(&qh)?;
    while app.feedback.is_none() {
        event_queue.blocking_dispatch(&mut app)?;
    }

    let surface = compositor.create_surface(&qh);
    let window = xdg_shell.create_window(surface, WindowDecorations::None, &qh);
    window.set_title("Droidloom GLES DMA-BUF probe");
    window.set_app_id("org.droidloom.DmabufProbe");
    window.set_fullscreen(None);
    let sync_surface = sync_manager.get_surface(window.wl_surface(), &qh, ());
    let timeline_handle = app
        .drm
        .create_syncobj(false)
        .map_err(|error| ProbeError::Syncobj(error.to_string()))?;
    let timeline_fd = app
        .drm
        .syncobj_to_fd(timeline_handle, false)
        .map_err(|error| ProbeError::Syncobj(error.to_string()))?;
    let timeline = sync_manager.import_timeline(timeline_fd.as_fd(), &qh, ());
    window.commit();
    app.window = Some(window);
    app.sync_surface = Some(sync_surface);
    app.timeline = Some(timeline);
    app.timeline_handle = Some(timeline_handle);

    while !app.exit {
        event_queue.blocking_dispatch(&mut app)?;
    }
    if let Some(error) = app.fatal {
        return Err(ProbeError::Callback(error));
    }
    let evidence = app
        .evidence
        .ok_or_else(|| ProbeError::Callback("probe exited without evidence".into()))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&evidence)
            .map_err(|error| ProbeError::Callback(error.to_string()))?
    );
    Ok(())
}

fn validate_render_node(path: &std::path::Path) -> Result<(), ProbeError> {
    let valid_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("renderD"))
        .is_some_and(|digits| {
            !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
        });
    if path.parent() == Some(std::path::Path::new("/dev/dri")) && valid_name {
        Ok(())
    } else {
        Err(ProbeError::RenderNodePolicy(path.to_path_buf()))
    }
}
