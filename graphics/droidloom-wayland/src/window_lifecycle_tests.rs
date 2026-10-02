//! Exercise task window ownership against a protocol-checking Wayland server.

use super::*;
use std::os::unix::net::UnixStream;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};
use wayland_protocols::xdg::{
    decoration::zv1::server::{zxdg_decoration_manager_v1, zxdg_toplevel_decoration_v1},
    shell::server::{xdg_surface, xdg_toplevel, xdg_wm_base},
};
use wayland_server::{
    Client, DataInit, Dispatch as ServerDispatch, Display, DisplayHandle, GlobalDispatch, New,
    Resource,
    protocol::{wl_compositor, wl_region, wl_surface as server_surface},
};

#[derive(Default)]
struct Server {
    events: Arc<Mutex<Vec<&'static str>>>,
    scales: Arc<Mutex<Vec<(u32, i32)>>>,
    decoration: Option<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1>,
    toplevel_alive: bool,
    xdg_surface_alive: bool,
    detached: bool,
}

impl Server {
    fn record(&self, event: &'static str) {
        self.events.lock().unwrap().push(event);
    }
}

#[derive(Debug)]
struct Peer;
impl wayland_server::backend::ClientData for Peer {}

macro_rules! global {
    ($ty:ty) => {
        impl GlobalDispatch<$ty, ()> for Server {
            fn bind(
                _: &mut Self,
                _: &DisplayHandle,
                _: &Client,
                id: New<$ty>,
                _: &(),
                init: &mut DataInit<'_, Self>,
            ) {
                init.init(id, ());
            }
        }
    };
}
global!(wl_compositor::WlCompositor);
global!(xdg_wm_base::XdgWmBase);
global!(zxdg_decoration_manager_v1::ZxdgDecorationManagerV1);

impl ServerDispatch<wl_compositor::WlCompositor, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                init.init(id, ());
            }
            wl_compositor::Request::CreateRegion { id } => {
                init.init(id, ());
            }
            _ => unreachable!(),
        }
    }
}
impl ServerDispatch<wl_region::WlRegion, ()> for Server {
    fn request(
        _: &mut Self,
        _: &Client,
        _: &wl_region::WlRegion,
        _: wl_region::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
    }
}
impl ServerDispatch<server_surface::WlSurface, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        surface: &server_surface::WlSurface,
        request: server_surface::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        match request {
            server_surface::Request::Attach { buffer: None, .. } => state.detached = true,
            server_surface::Request::Commit if state.detached => {
                state.record("unmap");
                state.detached = false;
            }
            server_surface::Request::Destroy => {
                assert!(
                    !state.xdg_surface_alive,
                    "wl_surface destroyed before its xdg_surface"
                );
                state.record("wl_surface");
            }
            server_surface::Request::Attach {
                buffer: Some(_), ..
            } => surface.post_error(0_u32, "fixture expects a null-buffer unmap"),
            server_surface::Request::SetBufferScale { scale } => {
                state
                    .scales
                    .lock()
                    .unwrap()
                    .push((surface.id().protocol_id(), scale));
            }
            _ => {}
        }
    }
}
impl ServerDispatch<xdg_wm_base::XdgWmBase, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &xdg_wm_base::XdgWmBase,
        request: xdg_wm_base::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let xdg_wm_base::Request::GetXdgSurface { id, .. } = request {
            init.init(id, ());
            state.xdg_surface_alive = true;
        }
    }
}
impl ServerDispatch<xdg_surface::XdgSurface, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        surface: &xdg_surface::XdgSurface,
        request: xdg_surface::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_surface::Request::GetToplevel { id } => {
                init.init(id, ());
                state.toplevel_alive = true;
            }
            xdg_surface::Request::Destroy => {
                if state.toplevel_alive {
                    surface.post_error(
                        xdg_surface::Error::DefunctRoleObject,
                        "toplevel still alive",
                    );
                }
                state.xdg_surface_alive = false;
                state.record("xdg_surface");
            }
            _ => {}
        }
    }
}
impl ServerDispatch<xdg_toplevel::XdgToplevel, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &xdg_toplevel::XdgToplevel,
        request: xdg_toplevel::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let xdg_toplevel::Request::Destroy = request {
            if let Some(decoration) = &state.decoration {
                decoration.post_error(
                    zxdg_toplevel_decoration_v1::Error::Orphaned,
                    "The xdg_toplevel_decoration object must be destroyed before its xdg_toplevel.",
                );
            }
            state.toplevel_alive = false;
            state.record("toplevel");
        }
    }
}
impl ServerDispatch<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
        request: zxdg_decoration_manager_v1::Request,
        _: &(),
        _: &DisplayHandle,
        init: &mut DataInit<'_, Self>,
    ) {
        if let zxdg_decoration_manager_v1::Request::GetToplevelDecoration { id, .. } = request {
            state.decoration = Some(init.init(id, ()));
        }
    }
}
impl ServerDispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, ()> for Server {
    fn request(
        state: &mut Self,
        _: &Client,
        _: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        request: zxdg_toplevel_decoration_v1::Request,
        _: &(),
        _: &DisplayHandle,
        _: &mut DataInit<'_, Self>,
    ) {
        if let zxdg_toplevel_decoration_v1::Request::Destroy = request {
            state.decoration = None;
            state.record("decoration");
        }
    }
}

struct Harness {
    conn: Connection,
    _queue: EventQueue<App>,
    compositor: CompositorState,
    qh: QueueHandle<App>,
    events: Arc<Mutex<Vec<&'static str>>>,
    scales: Arc<Mutex<Vec<(u32, i32)>>>,
    stop: Arc<AtomicBool>,
    server: Option<std::thread::JoinHandle<()>>,
}
impl Harness {
    fn new(decorated: bool) -> (Self, Window) {
        let (client, server) = UnixStream::pair().unwrap();
        let mut display = Display::<Server>::new().unwrap();
        let mut handle = display.handle();
        handle.insert_client(server, Arc::new(Peer)).unwrap();
        handle.create_global::<Server, wl_compositor::WlCompositor, _>(6, ());
        handle.create_global::<Server, xdg_wm_base::XdgWmBase, _>(6, ());
        if decorated {
            handle.create_global::<Server, zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, _>(
                1,
                (),
            );
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let scales = Arc::new(Mutex::new(Vec::new()));
        let mut state = Server {
            events: events.clone(),
            scales: scales.clone(),
            ..Server::default()
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let server = std::thread::spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                display.dispatch_clients(&mut state).unwrap();
                display.flush_clients().unwrap();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let conn = Connection::from_socket(client).unwrap();
        let (globals, queue) = registry_queue_init::<App>(&conn).unwrap();
        let qh = queue.handle();
        let compositor = CompositorState::bind(&globals, &qh).unwrap();
        let shell = XdgShell::bind(&globals, &qh).unwrap();
        let window = shell.create_window(
            compositor.create_surface(&qh),
            WindowDecorations::RequestServer,
            &qh,
        );
        conn.roundtrip().unwrap();
        (
            Self {
                conn,
                _queue: queue,
                compositor,
                qh,
                events,
                scales,
                stop,
                server: Some(server),
            },
            window,
        )
    }
    fn events(&self) -> Vec<&'static str> {
        self.events.lock().unwrap().clone()
    }
    fn scales(&self) -> Vec<(u32, i32)> {
        self.scales.lock().unwrap().clone()
    }
    fn extra_surface(&self) -> wl_surface::WlSurface {
        self.compositor.create_surface(&self.qh)
    }
}
impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.server.take().unwrap().join().unwrap();
    }
}

fn task(window: Window) -> TaskWindow {
    TaskWindow {
        layers: None,
        package: "test.camera".into(),
        android_task: None,
        window: Some(window),
        window_frame: None,
        chrome: None,
        chrome_capabilities: WindowManagerCapabilities::empty(),
        chrome_active: false,
        frame_shown: false,
        gesture_feedback: None,
        fullscreen_controls_revealed_until: None,
        decorations_hidden: false,
        viewport: None,
        fractional_scale: None,
        sync_surface: None,
        logical_size: None,
        buffer_size: (0, 0),
        preferred_scale_120: FRACTIONAL_SCALE_DENOMINATOR,
        configure_serial: 0,
        refresh_millihz: 60_000,
        entered_outputs: Vec::new(),
        presentation_output: None,
        targets: BTreeMap::new(),
        pending: VecDeque::from([PendingFrame {
            frame: FrameId(3),
            buffer: BufferId(4),
            release_point: 5,
            submitted_nanos: 6,
            trace: None,
        }]),
        presentation_feedback: Vec::new(),
        presentation_audit: None,
        content_opaque: false,
        applied_opaque: None,
        focused: false,
        fullscreen: false,
        maximized: false,
        floating_size: None,
        unmap_requested: true,
        closing: true,
    }
}

#[test]
fn decorated_task_destroys_decoration_before_its_role_once() {
    let (server, window) = Harness::new(true);
    let mut task = task(window);
    let surface = task.surface().unwrap().clone();
    // The production unmap detaches first, independently of retained buffer fences.
    surface.attach(None, 0, 0);
    surface.commit();
    task.drop_native_window();
    task.drop_native_window();
    server
        .conn
        .roundtrip()
        .expect("task teardown must not disconnect the Wayland client");
    assert_eq!(
        server.events(),
        [
            "unmap",
            "decoration",
            "toplevel",
            "xdg_surface",
            "wl_surface"
        ]
    );
    assert!(task.window.is_none());
    assert_eq!(
        task.pending.len(),
        1,
        "closing the shell must not release an in-flight buffer"
    );
}

#[test]
fn retained_callback_clone_does_not_force_an_early_role_destroy() {
    let (server, window) = Harness::new(true);
    let callback_window = window.clone();
    let mut task = task(window);
    let surface = task.surface().unwrap().clone();
    surface.attach(None, 0, 0);
    surface.commit();
    task.drop_native_window();
    server
        .conn
        .roundtrip()
        .expect("a retained Window clone must keep the complete role tree alive");
    assert_eq!(server.events(), ["unmap"]);
    assert!(!task.accepts_present());
    drop(callback_window);
    server.conn.roundtrip().unwrap();
    assert_eq!(
        server.events(),
        [
            "unmap",
            "decoration",
            "toplevel",
            "xdg_surface",
            "wl_surface"
        ]
    );
}

#[test]
fn task_without_decoration_manager_destroys_its_role_once() {
    let (server, window) = Harness::new(false);
    let mut task = task(window);
    task.drop_native_window();
    task.drop_native_window();
    server.conn.roundtrip().unwrap();
    assert_eq!(server.events(), ["toplevel", "xdg_surface", "wl_surface"]);
}

#[test]
fn preferred_scale_change_pins_only_the_window_surface() {
    let (server, window) = Harness::new(true);
    // Neither a chrome button nor a gesture-feedback slab is the task window
    // itself: both pre-render their buffers at an integer scale they apply
    // when they build them, so the preferred-scale callback must leave their
    // own buffer scale alone.
    let chrome = server.extra_surface();
    let feedback = server.extra_surface();
    let task = task(window);
    let window_surface = task.surface().unwrap().clone();
    let tasks = BTreeMap::from([(TaskObjectId(1), task)]);
    for surface in [&chrome, &feedback] {
        surface.set_buffer_scale(2);
        apply_preferred_buffer_scale(&tasks, surface);
    }
    apply_preferred_buffer_scale(&tasks, &window_surface);
    server.conn.roundtrip().unwrap();
    assert_eq!(
        server.scales(),
        [
            (chrome.id().protocol_id(), 2),
            (feedback.id().protocol_id(), 2),
            (window_surface.id().protocol_id(), 1),
        ]
    );
}
