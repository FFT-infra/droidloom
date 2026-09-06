//! Android vendor Composer V5 service entry point for Droidloom.

#![forbid(unsafe_code)]

mod control;
mod input;
mod surface_bridge;

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use binder::ProcessState;
use droidloom_composer::denial::AppliedDenialEvent;
use droidloom_composer::hwc3::Session;
use droidloom_composer_aidl::denial::DenialPresentationSink;
use droidloom_composer_aidl::minigbm::MinigbmBufferAdapter;
use droidloom_composer_aidl::service::{ComposerLifecycle, ComposerService, SharedSession};
use droidloom_denial_ipc::{ProtocolSocket, SeqPacket, SeqPacketListener};
use droidloom_denial_protocol::{
    capability, AndroidMessage, DenialMessage, InputEvent, TaskObjectId, PROTOCOL_MAJOR,
};
use droidloom_syncobj::SyncobjDevice;

use crate::control::TaskCoordinator;
use crate::input::InputBridge;
use crate::surface_bridge::SurfaceBridge;

const DEFAULT_SOCKET: &str = "/dev/socket/droidloom/denial";
const DEFAULT_RENDER_NODE: &str = "/dev/dri/renderD128";
const CONTROL_SOCKET_NAME: &str = "droidloom-task-control";
const SURFACE_SOCKET_NAME: &str = "droidloom-surfaceflinger";
const INPUT_SOCKET: &str = "/dev/socket/droidloom_input";
const SERVICE_NAME: &str = "android.hardware.graphics.composer3.IComposer/default";
const BOOTSTRAP_PACKAGE: &str = "android.droidloom.bootstrap";

fn main() {
    // Keep metadata in the executable itself, tied to the linked wire encoder.
    let compatibility = std::str::from_utf8(&droidloom_input_protocol::BUILD_COMPATIBILITY)
        .expect("input compatibility marker is ASCII");
    if std::env::args().any(|arg| arg == "--compatibility") {
        println!("{compatibility}");
        return;
    }
    eprintln!("Droidloom Composer {compatibility}");
    if let Err(error) = run() {
        eprintln!("Droidloom Composer fatal: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let (socket_path, render_node) = arguments()?;
    let control_listener = SeqPacketListener::from_android_init_socket(CONTROL_SOCKET_NAME)
        .map_err(|error| format!("adopt Android init task-control socket: {error}"))?;
    let surface_listener = SeqPacketListener::from_android_init_socket(SURFACE_SOCKET_NAME)
        .map_err(|error| format!("adopt Android init SurfaceFlinger socket: {error}"))?;
    let render_file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&render_node)
        .map_err(|error| format!("open render node {}: {error}", render_node.display()))?;
    let socket = ProtocolSocket::new(
        SeqPacket::connect(&socket_path)
            .map_err(|error| format!("connect {}: {error}", socket_path.display()))?,
    );
    handshake(&socket)?;

    let sink = DenialPresentationSink::new(socket, SyncobjDevice::from_file(render_file));
    let session: SharedSession = Arc::new(Mutex::new(Session::default()));
    let factory_sink = sink.clone();
    let service = ComposerService::new(Arc::clone(&session), move || {
        MinigbmBufferAdapter::new(factory_sink.clone())
    });
    let lifecycle = service.lifecycle();
    let coordinator = TaskCoordinator::new(sink.clone(), lifecycle.clone());
    let input = InputBridge::new(INPUT_SOCKET);

    ProcessState::set_thread_pool_max_thread_count(4);
    ProcessState::start_thread_pool();
    let service = service.into_binder();
    binder::add_service(SERVICE_NAME, service.as_binder())
        .map_err(|error| format!("register {SERVICE_NAME}: {error:?}"))?;

    let event_coordinator = coordinator.clone();
    let event_session = Arc::clone(&session);
    let surface_sink = sink.clone();
    let _event_thread = std::thread::Builder::new()
        .name("droidloom-denial-events".to_owned())
        .spawn(move || {
            if let Err(error) = event_loop(
                &sink,
                &event_session,
                &lifecycle,
                &event_coordinator,
                &input,
            ) {
                eprintln!("Droidloom Denial event loop fatal: {error}");
                std::process::exit(1);
            }
        })
        .map_err(|error| format!("start Denial event loop: {error}"))?;

    let (bootstrap_object, bootstrap_display) = coordinator
        .stage_bootstrap_display(BOOTSTRAP_PACKAGE.to_owned())
        .map_err(|error| format!("stage SurfaceFlinger bootstrap display: {error}"))?;
    eprintln!(
        "Droidloom bootstrap display configured: object={} display={}",
        bootstrap_object.0, bootstrap_display.0
    );

    let (bootstrap_width, bootstrap_height) = {
        let session = session
            .lock()
            .map_err(|_| "Composer session lock is poisoned".to_owned())?;
        let configure = session
            .windows()
            .composer_for_display(bootstrap_display)
            .map_err(|error| error.to_string())?
            .latest_configure()
            .ok_or_else(|| "bootstrap display has no Denial configure".to_owned())?;
        (configure.width, configure.height)
    };
    let surface_bridge = SurfaceBridge::new(
        surface_sink,
        Arc::clone(&session),
        coordinator.clone(),
        bootstrap_display,
        bootstrap_width,
        bootstrap_height,
    )?;
    let _surface_thread = std::thread::Builder::new()
        .name("droidloom-surfaceflinger".to_owned())
        .spawn(move || {
            if let Err(error) = surface_bridge.serve(surface_listener) {
                eprintln!("Droidloom SurfaceFlinger listener fatal: {error}");
                std::process::exit(1);
            }
        })
        .map_err(|error| format!("start SurfaceFlinger bridge thread: {error}"))?;

    let _control_thread = std::thread::Builder::new()
        .name("droidloom-task-control".to_owned())
        .spawn(move || {
            if let Err(error) = coordinator.serve(control_listener) {
                eprintln!("Droidloom task-control listener fatal: {error}");
                std::process::exit(1);
            }
        })
        .map_err(|error| format!("start task-control listener: {error}"))?;

    ProcessState::join_thread_pool();
    Err("Binder thread pool exited unexpectedly".to_owned())
}

fn arguments() -> Result<(PathBuf, PathBuf), String> {
    let mut arguments = std::env::args_os().skip(1);
    let socket = arguments
        .next()
        .map_or_else(|| PathBuf::from(DEFAULT_SOCKET), PathBuf::from);
    let render_node = arguments
        .next()
        .map_or_else(|| PathBuf::from(DEFAULT_RENDER_NODE), PathBuf::from);
    if arguments.next().is_some() {
        return Err("usage: composer-service [DENIAL_SOCKET [DRM_RENDER_NODE]]".to_owned());
    }
    Ok((socket, render_node))
}

fn handshake(socket: &ProtocolSocket) -> Result<(), String> {
    socket
        .send_android(
            &AndroidMessage::ClientHello {
                min_major: PROTOCOL_MAJOR,
                max_major: PROTOCOL_MAJOR,
                capabilities: capability::REQUIRED_V1,
            },
            &[],
        )
        .map_err(|error| format!("send ClientHello: {error}"))?;
    match socket
        .receive_denial()
        .map_err(|error| format!("receive ServerHello: {error}"))?
        .message
    {
        DenialMessage::ServerHello {
            major,
            capabilities,
            ..
        } if major == PROTOCOL_MAJOR
            && capabilities & capability::REQUIRED_V1 == capability::REQUIRED_V1 =>
        {
            Ok(())
        }
        DenialMessage::Error { message, .. } => {
            Err(format!("Denial rejected handshake: {message}"))
        }
        _ => Err("Denial returned an incompatible handshake response".to_owned()),
    }
}

fn event_loop(
    sink: &DenialPresentationSink,
    session: &SharedSession,
    lifecycle: &ComposerLifecycle,
    coordinator: &TaskCoordinator,
    input: &InputBridge,
) -> Result<(), String> {
    loop {
        let received = sink
            .socket()
            .receive_denial()
            .map_err(|error| error.to_string())?;
        let message = received.message;
        match message {
            DenialMessage::Ping { cookie } => sink
                .socket()
                .send_android(&AndroidMessage::Pong { cookie }, &[])
                .map_err(|error| error.to_string())?,
            DenialMessage::Error { message, .. } => {
                return Err(format!("Denial protocol error: {message}"));
            }
            DenialMessage::ServerHello { .. } => {
                return Err("repeated Denial ServerHello".to_owned());
            }
            DenialMessage::RegisterRenderTarget {
                object,
                configure_serial,
                buffer,
            } => sink
                .register_render_target(object, configure_serial, buffer, received.descriptors)
                .map_err(|error| error.to_string())?,
            DenialMessage::UnregisterRenderTarget { object, buffer } => sink
                .unregister_render_target(object, buffer)
                .map_err(|error| error.to_string())?,
            task_message => {
                if let DenialMessage::Presented {
                    timestamp_nanos,
                    refresh_period_nanos,
                    ..
                } = &task_message
                {
                    lifecycle.observe_presentation(*timestamp_nanos, *refresh_period_nanos);
                }
                let object = task_object(&task_message)
                    .ok_or_else(|| "task event omitted object identity".to_owned())?;
                let display = match sink.display_for_object(object) {
                    Ok(display) => display,
                    Err(_) if coordinator.is_retired(object) => continue,
                    Err(error) => return Err(error.to_string()),
                };
                let event = {
                    let mut session = session
                        .lock()
                        .map_err(|_| "Composer session lock is poisoned".to_owned())?;
                    let event = sink
                        .apply_task_event(&mut session, &task_message)
                        .map_err(|error| error.to_string())?;
                    if let AppliedDenialEvent::Configure { serial, .. } = event {
                        sink.acknowledge_configure(&mut session, display, serial)
                            .map_err(|error| error.to_string())?;
                    }
                    event
                };
                // A configure has completed once its state has been applied and
                // acknowledged to Denial. Wake the reservation before invoking
                // Android's hotplug callback: during SurfaceFlinger bootstrap
                // that callback may wait for the reservation owner to proceed.
                if let AppliedDenialEvent::Configure {
                    scale_numerator,
                    scale_denominator,
                    ..
                } = &event
                {
                    coordinator.set_input_scale(object, *scale_numerator, *scale_denominator)?;
                    coordinator.display_connected(object)?;
                }
                dispatch_event(lifecycle, coordinator, input, object, display, event)?;
            }
        }
    }
}

fn dispatch_event(
    lifecycle: &ComposerLifecycle,
    coordinator: &TaskCoordinator,
    input: &InputBridge,
    object: TaskObjectId,
    display: droidloom_composer::DisplayId,
    event: AppliedDenialEvent,
) -> Result<(), String> {
    match event {
        AppliedDenialEvent::Configure {
            width,
            height,
            scale_numerator,
            scale_denominator,
            ..
        } => {
            if coordinator.is_direct_task(object) {
                let (task, android_display) = coordinator.direct_task_route_for_object(object)?;
                match input.send_task_bounds(
                    task.0,
                    android_display,
                    width,
                    height,
                    scale_numerator,
                    scale_denominator,
                ) {
                    Ok(()) => eprintln!(
                        "Droidloom requested Android task {} bounds {width}x{height} at scale \
                         {scale_numerator}/{scale_denominator}",
                        task.0,
                    ),
                    Err(error) => eprintln!(
                        "Droidloom deferred Android task {} bounds {width}x{height}: {error}",
                        task.0
                    ),
                }
                return Ok(());
            }
            if lifecycle
                .is_connected(display)
                .map_err(|error| format!("query hotplug state: {error:?}"))?
            {
                lifecycle
                    .refresh(display)
                    .map_err(|error| format!("refresh task display: {error:?}"))?;
            } else {
                lifecycle
                    .connect_display(display)
                    .map_err(|error| format!("connect task display: {error:?}"))?;
            }
        }
        AppliedDenialEvent::Presented(_) => {}
        AppliedDenialEvent::Visibility { focused, .. } => {
            if focused && coordinator.is_direct_task(object) {
                let (task, _) = coordinator.direct_task_route_for_object(object)?;
                if let Err(error) = input.send_task_focus(task.0, true) {
                    eprintln!(
                        "Droidloom deferred Android task {} keyboard focus: {error}",
                        task.0
                    );
                }
            }
        }
        AppliedDenialEvent::FormatFeedback { .. }
        | AppliedDenialEvent::Insets { .. }
        | AppliedDenialEvent::BufferReleased(_) => {}
        AppliedDenialEvent::Close => {
            if !coordinator.is_direct_task(object) {
                eprintln!(
                    "Droidloom ignored close for non-task display {display:?} object={}",
                    object.0
                );
                return Ok(());
            }
            let (task, _) = coordinator.direct_task_route_for_object(object)?;
            match input.send_task_close(task.0) {
                Ok(()) => eprintln!(
                    "Droidloom requested Android task {} removal for host close object={}",
                    task.0, object.0
                ),
                Err(error) => eprintln!(
                    "Droidloom could not request Android task {} removal for host close: {error}",
                    task.0
                ),
            }
        }
        AppliedDenialEvent::Input {
            serial,
            timestamp_nanos,
            event,
        } => match coordinator.input_route_for_object(object) {
            Ok((task, android_display, scale_numerator, scale_denominator)) => {
                if let InputEvent::Key {
                    action,
                    keycode,
                    repeat,
                } = &event
                {
                    eprintln!(
                        "Droidloom key trace: stage=composer serial={serial} object={} task={} display={} action={action:?} scan_code={keycode} repeat={repeat}",
                        object.0, task.0, android_display
                    );
                }
                if let Err(error) = input.send(
                    serial,
                    android_display,
                    task.0,
                    scale_numerator,
                    scale_denominator,
                    timestamp_nanos,
                    event,
                ) {
                    eprintln!(
                        "Droidloom dropped input serial {serial} for display {display:?}: {error}"
                    );
                }
            }
            Err(error) => eprintln!(
                "Droidloom dropped unbound input serial {serial} for display {display:?}: {error}"
            ),
        },
    }
    Ok(())
}

fn task_object(message: &DenialMessage) -> Option<TaskObjectId> {
    match message {
        DenialMessage::FormatFeedback { object, .. }
        | DenialMessage::RegisterRenderTarget { object, .. }
        | DenialMessage::UnregisterRenderTarget { object, .. }
        | DenialMessage::Configure { object, .. }
        | DenialMessage::Insets { object, .. }
        | DenialMessage::Visibility { object, .. }
        | DenialMessage::Close { object }
        | DenialMessage::BufferReleased { object, .. }
        | DenialMessage::Presented { object, .. }
        | DenialMessage::Input { object, .. } => Some(*object),
        DenialMessage::Error { object, .. } => *object,
        DenialMessage::ServerHello { .. } | DenialMessage::Ping { .. } => None,
    }
}
