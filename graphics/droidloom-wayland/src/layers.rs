//! Direct Android layers on synchronized Wayland subsurfaces. No pixel copies
//! or rendering occur here. The SHM buffers represent constant colors.
use super::*;
use droidloom_denial_ipc::SeqPacket;
use droidloom_surface_bridge::layers::{self as wire, Buffer, Layer, Request};
use std::collections::BTreeSet;
use std::io::Write;
use wayland_client::protocol::{wl_shm, wl_shm_pool, wl_subcompositor, wl_subsurface};
use wayland_protocols::wp::alpha_modifier::v1::client::{
    wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1, wp_alpha_modifier_v1::WpAlphaModifierV1,
};

pub(super) struct Globals {
    pub subcompositor: wl_subcompositor::WlSubcompositor,
    pub shm: wl_shm::WlShm,
    pub alpha: Option<WpAlphaModifierV1>,
}
struct Source {
    metadata: Buffer,
    buffer: Option<wl_buffer::WlBuffer>,
    sync: BufferSync,
    ticket: Option<u64>,
    availability_wakeup: crate::fence_wakeup::FenceWakeup,
}
struct View {
    surface: wl_surface::WlSurface,
    role: wl_subsurface::WlSubsurface,
    viewport: WpViewport,
    alpha: WpAlphaModifierSurfaceV1,
    sync: Option<WpLinuxDrmSyncobjSurfaceV1>,
    source: Option<(u64, u64)>,
    solid: Option<(u32, wl_buffer::WlBuffer)>,
    geometry: Option<Geometry>,
}

/// Persistent Wayland state, independent of the producer's rotating buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Geometry {
    above: Option<u64>,
    destination: [i32; 4],
    source: [u32; 4],
    transform: u32,
    alpha: u32,
    opaque: bool,
}

impl Geometry {
    fn new(l: &Layer, above: Option<u64>, logical: (u32, u32), physical: (u32, u32)) -> Self {
        let [x, y, w, h] = l.destination;
        let edge = |v: i32, logical: u32, physical: u32| -> i32 {
            ((i64::from(v) * i64::from(logical) + i64::from(physical) / 2) / i64::from(physical))
                as i32
        };
        let dx = edge(x, logical.0, physical.0);
        let dy = edge(y, logical.1, physical.1);
        Self {
            above,
            destination: [
                dx,
                dy,
                (edge(x + w, logical.0, physical.0) - dx).max(1),
                (edge(y + h, logical.1, physical.1) - dy).max(1),
            ],
            source: if l.buffer == 0 {
                [0, 0, 256, 256]
            } else {
                l.source
            },
            transform: l.transform,
            alpha: l.alpha,
            opaque: l.opaque && l.alpha == u32::MAX,
        }
    }
}
impl Drop for View {
    fn drop(&mut self) {
        if let Some(sync) = self.sync.take() {
            sync.destroy();
        }
        self.alpha.destroy();
        self.viewport.destroy();
        self.role.destroy();
        self.surface.destroy();
        if let Some((_, buffer)) = self.solid.take() {
            buffer.destroy();
        }
    }
}
struct Read {
    buffer: u64,
    ticket: u64,
    point: u64,
    fence_sent: bool,
}
pub(super) struct Stream {
    requests: SeqPacket,
    events: SeqPacket,
    buffers: BTreeMap<u64, Source>,
    staged: wire::Staging<OwnedFd>,
    views: BTreeMap<u64, View>,
    reads: Vec<Read>,
    importing: Option<(u64, zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1)>,
    root: Option<((u32, u32), wl_buffer::WlBuffer)>,
    root_logical_size: Option<(u32, u32)>,
    active: bool,
    connected: bool,
    last_ticket: u64,
    configuration: wire::ConfigPublisher,
}
impl Stream {
    pub fn busy(&self) -> bool {
        !self.reads.is_empty()
    }
    pub fn fds(&self) -> impl Iterator<Item = (i32, i16)> + '_ {
        self.connected
            .then_some((self.requests.as_fd().as_raw_fd(), libc::POLLIN))
            .into_iter()
            .chain(self.reads.iter().flat_map(|read| {
                let source = self.buffers.get(&read.buffer);
                let available = source
                    .and_then(|source| source.availability_wakeup.fd())
                    .filter(|_| self.connected && !read.fence_sent);
                let released = source
                    .and_then(|source| source.sync.release_wakeup.as_ref())
                    .map(AsFd::as_fd);
                [available, released]
                    .into_iter()
                    .flatten()
                    .map(|event| (event.as_raw_fd(), libc::POLLIN))
            }))
    }
    pub fn poll_fallback(&self) -> bool {
        self.reads.iter().any(|r| {
            (self.connected
                && !r.fence_sent
                && self
                    .buffers
                    .get(&r.buffer)
                    .is_some_and(|b| b.availability_wakeup.fd().is_none()))
                || self
                    .buffers
                    .get(&r.buffer)
                    .is_some_and(|b| b.sync.release_wakeup.is_none())
        })
    }
    fn ack(&self, code: i32) -> Result<(), PresenterError> {
        self.requests
            .send_record(&wire::response(0x8001, &[code as u32]), &[])?;
        Ok(())
    }
    /// Detach children in the parent's pending transaction. Their original
    /// read records survive until the compositor releases each buffer.
    pub fn hide(&mut self) -> bool {
        if !self.active {
            return false;
        }
        for view in self.views.values_mut() {
            view.surface.attach(None, 0, 0);
            view.surface.commit();
            view.source = None;
        }
        self.active = false;
        true
    }
    fn release(&mut self) -> Result<(), PresenterError> {
        // Idle cached imports cannot complete a read. Avoid a read(2) for every
        // cached allocation on every presenter wakeup.
        for source in self
            .buffers
            .values()
            .filter(|source| source.ticket.is_some())
        {
            if let Some(event) = source.sync.release_wakeup.as_ref() {
                let mut count = 0_u64;
                // SAFETY: event is a live nonblocking eventfd; count is eight writable bytes.
                let n = unsafe {
                    libc::read(event.as_raw_fd(), std::ptr::from_mut(&mut count).cast(), 8)
                };
                if n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock {
                    return Err(io::Error::last_os_error().into());
                }
            }
        }
        let mut index = 0;
        while index < self.reads.len() {
            let read = &mut self.reads[index];
            let source = self
                .buffers
                .get_mut(&read.buffer)
                .expect("read owns source");
            if self.connected && !read.fence_sent {
                if let Some(fence) = source.sync.timeline.try_export_release_fence(read.point)? {
                    self.events
                        .send_record(&wire::release_fence(read.ticket), &[fence.as_fd()])?;
                    read.fence_sent = true;
                }
            }
            if source.sync.timeline.signalled_point()? < read.point {
                index += 1;
                continue;
            }
            // Completion comes from the actual Wayland buffer read. Neither
            // presentation timing nor socket acknowledgements release sources.
            let packet = wire::response(0x8003, &[read.ticket as u32, (read.ticket >> 32) as u32]);
            if self.connected {
                self.events.send_record(&packet, &[])?;
            }
            source.ticket = None;
            self.reads.swap_remove(index);
        }
        Ok(())
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        self.views.clear();
        for source in self.buffers.values_mut() {
            if let Some(b) = source.buffer.take() {
                b.destroy();
            }
        }
        if let Some((_, b)) = self.root.take() {
            b.destroy();
        }
        if let Some((_, p)) = self.importing.take() {
            p.destroy();
        }
    }
}

fn pixel(
    globals: &Globals,
    qh: &QueueHandle<App>,
    color: u32,
) -> Result<wl_buffer::WlBuffer, PresenterError> {
    let mut file = tempfile::tempfile()?;
    file.write_all(&color.to_ne_bytes())?;
    let pool = globals.shm.create_pool(file.as_fd(), 4, qh, ());
    let buffer = pool.create_buffer(0, 1, 1, 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    Ok(buffer)
}

fn carrier_size(width: u32, height: u32) -> Result<(u32, u32), PresenterError> {
    if width == 0 || height == 0 {
        return Err(PresenterError::Configuration("empty layer task size"));
    }
    // Mobile shells can use the root buffer's aspect when fitting the whole
    // surface tree. Preserve that aspect for the constant black backing.
    // Reduce the dimensions to avoid allocating a full task-sized blank image.
    let (mut a, mut b) = (width, height);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    Ok((width / a, height / a))
}

fn carrier(
    globals: &Globals,
    qh: &QueueHandle<App>,
    (width, height): (u32, u32),
) -> Result<wl_buffer::WlBuffer, PresenterError> {
    let stride = width.checked_mul(4);
    let bytes = stride
        .and_then(|stride| stride.checked_mul(height))
        .and_then(|bytes| i32::try_from(bytes).ok())
        .ok_or(PresenterError::Configuration("layer root buffer too large"))?;
    let file = tempfile::tempfile()?;
    file.set_len(bytes as u64)?;
    let pool = globals.shm.create_pool(file.as_fd(), bytes, qh, ());
    let buffer = pool.create_buffer(
        0,
        width as i32,
        height as i32,
        stride.unwrap() as i32,
        wl_shm::Format::Xrgb8888,
        qh,
        (),
    );
    pool.destroy();
    Ok(buffer)
}

impl App {
    fn layer_config(&self, object: TaskObjectId) -> Option<[u32; 4]> {
        let task = self.tasks.get(&object)?;
        let enabled = self.layer_globals.alpha.is_some() && task.accepts_present();
        Some([
            if enabled { wire::SUPPORTED } else { 0 },
            task.configure_serial,
            task.buffer_size.0,
            task.buffer_size.1,
        ])
    }
    pub(super) fn bind_layers(
        &mut self,
        object: TaskObjectId,
        sockets: Vec<OwnedFd>,
    ) -> Result<(), PresenterError> {
        let task = self
            .tasks
            .get_mut(&object)
            .ok_or(PresenterError::UnknownTask(object))?;
        if task.layers.is_some() || sockets.len() != 2 || task.headless() {
            return Err(PresenterError::Configuration(
                "invalid layer stream binding",
            ));
        }
        let mut sockets = sockets.into_iter();
        let requests = SeqPacket::from_owned_fd(sockets.next().unwrap())?;
        let events = SeqPacket::from_owned_fd(sockets.next().unwrap())?;
        requests.set_nonblocking(true)?;
        // Releases are bounded by MAX_BUFFERS, far below the socket send queue.
        events.set_nonblocking(true)?;
        task.layers = Some(Stream {
            requests,
            events,
            buffers: BTreeMap::new(),
            staged: wire::Staging::default(),
            views: BTreeMap::new(),
            reads: Vec::new(),
            importing: None,
            root: None,
            root_logical_size: None,
            active: false,
            connected: true,
            last_ticket: 0,
            configuration: wire::ConfigPublisher::default(),
        });
        Ok(())
    }
    pub(super) fn drain_layers(
        &mut self,
        qh: &QueueHandle<Self>,
        objects: &mut Vec<TaskObjectId>,
    ) -> Result<(), PresenterError> {
        let mut receive_buffer = [0; 128];
        objects.clear();
        objects.extend(self.tasks.keys().copied());
        for object in objects.iter().copied() {
            if let Some(stream) = self.tasks.get_mut(&object).and_then(|t| t.layers.as_mut()) {
                stream.release()?;
            }
            for _ in 0..128 {
                let Some(stream) = self.tasks.get_mut(&object).and_then(|t| t.layers.as_mut())
                else {
                    break;
                };
                if !stream.connected || stream.importing.is_some() {
                    break;
                }
                let record = match stream.requests.receive_record_into(&mut receive_buffer) {
                    Ok(r) => r,
                    Err(IpcError::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => break,
                    Err(IpcError::PeerClosed) => {
                        stream.connected = false;
                        stream.staged.clear();
                        stream.hide();
                        if let Some(surface) = self.tasks[&object].surface() {
                            surface.commit();
                        }
                        break;
                    }
                    Err(e) => return Err(e.into()),
                };
                let request = wire::decode(record.bytes, record.descriptors.len())
                    .map_err(|_| PresenterError::Configuration("invalid layer stream record"))?;
                if matches!(
                    request,
                    Request::Commit { .. } | Request::Config | Request::SubscribeConfig
                ) {
                    // A legacy target is sent on Composer's socket before SF
                    // receives its ack and can send this request. Drain that
                    // socket AFTER receiving the barrier, while task.layers
                    // is still attached, so fallback/direct mode cannot reorder
                    // across the two file descriptors.
                    self.drain_endpoint(qh)?;
                }
                let Some(mut stream) = self.tasks.get_mut(&object).and_then(|t| t.layers.take())
                else {
                    break;
                };
                let result =
                    self.layer_request(qh, object, &mut stream, request, record.descriptors);
                self.tasks.get_mut(&object).unwrap().layers = Some(stream);
                result?;
            }
        }
        // A barrier above may drain endpoint changes for any task. Publish
        // after all barriers so no configuration waits for a future wakeup.
        for object in objects.iter().copied() {
            let Some(config) = self.layer_config(object) else {
                continue;
            };
            if let Some(stream) = self.tasks.get_mut(&object).and_then(|t| t.layers.as_mut())
                && stream.connected
            {
                let update = stream.configuration.update(config).map_err(|_| {
                    PresenterError::Configuration("layer configuration revision exhausted")
                })?;
                if let Some(words) = update {
                    stream
                        .events
                        .send_record(&wire::response(0x8005, &words), &[])?;
                }
            }
        }
        Ok(())
    }
    fn layer_request(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        stream: &mut Stream,
        request: Request,
        fds: Vec<OwnedFd>,
    ) -> Result<(), PresenterError> {
        match request {
            Request::SubscribeConfig => {
                let words = stream
                    .configuration
                    .subscribe(
                        self.layer_config(object)
                            .ok_or(PresenterError::UnknownTask(object))?,
                    )
                    .map_err(|_| {
                        PresenterError::Configuration("layer configuration revision exhausted")
                    })?;
                stream
                    .requests
                    .send_record(&wire::response(0x8005, &words), &[])?;
            }
            Request::Config => {
                stream.requests.send_record(
                    &wire::response(
                        0x8002,
                        &self
                            .layer_config(object)
                            .ok_or(PresenterError::UnknownTask(object))?,
                    ),
                    &[],
                )?;
            }
            Request::Register(metadata) => {
                let supported = self.feedback.as_ref().is_some_and(|f| {
                    f.tranches()
                        .iter()
                        .flat_map(|t| t.formats.iter())
                        .filter_map(|i| f.format_table().get(*i as usize))
                        .any(|f| f.format == metadata.fourcc && f.modifier == metadata.modifier)
                });
                if !supported
                    || stream.buffers.len() >= wire::MAX_BUFFERS
                    || stream.buffers.contains_key(&metadata.id)
                {
                    return stream.ack(-95);
                }
                let params = self.dmabuf_state.create_params(qh)?;
                for (i, (p, fd)) in metadata.planes.iter().zip(&fds).enumerate() {
                    params.add(fd.as_fd(), i as u32, p.offset, p.stride, metadata.modifier);
                }
                let timeline = WaylandTimeline::create(&self.syncobj)?;
                let fd = timeline.export_descriptor()?;
                let proxy = self.sync_manager.import_timeline(fd.as_fd(), qh, ());
                // SAFETY: eventfd returns a new owned descriptor without pointers.
                let event = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
                if event < 0 {
                    return Err(io::Error::last_os_error().into());
                }
                let sync = BufferSync {
                    timeline,
                    proxy,
                    next_point: 0,
                    release_wakeup: Some(unsafe { OwnedFd::from_raw_fd(event) }),
                };
                let id = metadata.id;
                let params = params.create(
                    metadata.width as i32,
                    metadata.height as i32,
                    metadata.fourcc,
                    zwp_linux_buffer_params_v1::Flags::empty(),
                );
                stream.buffers.insert(
                    id,
                    Source {
                        metadata,
                        buffer: None,
                        sync,
                        ticket: None,
                        availability_wakeup: crate::fence_wakeup::FenceWakeup::default(),
                    },
                );
                stream.importing = Some((id, params));
                // Ack only after the compositor confirms import. Failure is a
                // normal capability fallback, never a fatal Wayland error.
            }
            Request::Unregister(id) => {
                if stream.buffers.get(&id).is_none_or(|b| b.ticket.is_some()) {
                    return stream.ack(-16);
                }
                if let Some(mut source) = stream.buffers.remove(&id) {
                    if let Some(b) = source.buffer.take() {
                        b.destroy();
                    }
                }
                stream.ack(0)?;
            }
            Request::Stage(layer) => {
                let accepted = stream.staged.push(layer, fds.into_iter().next());
                stream.ack(if accepted { 0 } else { -7 })?;
            }
            Request::StageAsync(layer) => {
                // No per-layer reply. The failure remains sticky until the
                // Commit/Abort barrier; an overflowing prefix cannot be shown.
                stream.staged.push(layer, fds.into_iter().next());
            }
            Request::Abort => {
                stream.staged.clear();
                stream.ack(0)?;
            }
            Request::Commit {
                serial,
                width,
                height,
                count,
            } => {
                let task = &self.tasks[&object];
                let exact_geometry = stream.staged.layers().iter().all(|(l, _)| {
                    let Some((lw, lh)) = task.logical_size else {
                        return false;
                    };
                    let [x, y, w, h] = l.destination;
                    [
                        (i64::from(x), lw, width),
                        (i64::from(y), lh, height),
                        (i64::from(x) + i64::from(w), lw, width),
                        (i64::from(y) + i64::from(h), lh, height),
                    ]
                    .iter()
                    .all(|&(edge, logical, physical)| {
                        physical > 0 && edge * i64::from(logical) % i64::from(physical) == 0
                    })
                });
                let valid = !stream.staged.failed()
                    && self.layer_globals.alpha.is_some()
                    && task.accepts_present()
                    && task.configure_serial == serial
                    && task.buffer_size == (width, height)
                    && exact_geometry
                    && wire::validate_scene(
                        stream
                            .staged
                            .layers()
                            .iter()
                            .map(|(l, fd)| (l, fd.is_some())),
                        count,
                        width,
                        height,
                        stream.last_ticket,
                        |id| {
                            stream.buffers.get(&id).map(|b| wire::SourceState {
                                buffer: &b.metadata,
                                imported: b.buffer.is_some(),
                                ticket: b.ticket,
                            })
                        },
                        |id| stream.views.get(&id).and_then(|v| v.source),
                    );
                if !valid {
                    stream.staged.clear();
                    return stream.ack(-95);
                }
                self.commit_layers(qh, object, stream, width, height)?;
                stream.ack(0)?;
            }
        }
        Ok(())
    }
    fn commit_layers(
        &mut self,
        qh: &QueueHandle<Self>,
        object: TaskObjectId,
        stream: &mut Stream,
        width: u32,
        height: u32,
    ) -> Result<(), PresenterError> {
        let task = self.tasks.get_mut(&object).unwrap();
        let root = task.surface().unwrap().clone();
        let (lw, lh) = task.logical_size.ok_or(PresenterError::Configuration(
            "layer task has no logical size",
        ))?;
        // The task root owns input and the XDG role. Its zero-filled XRGB buffer
        // supplies the same opaque black backing as SF's composed task target.
        // Gaps and transparent app pixels blend over black in either mode.
        let root_size = carrier_size(lw, lh)?;
        let root_changed = stream
            .root
            .as_ref()
            .is_none_or(|(size, _)| *size != root_size);
        if root_changed {
            let buffer = carrier(&self.layer_globals, qh, root_size)?;
            if let Some((_, old)) = stream.root.replace((root_size, buffer)) {
                old.destroy();
            }
        }
        let mut staged = stream.staged.take();
        let ids: BTreeSet<_> = staged.iter().map(|(l, _)| l.id).collect();
        for (id, v) in &mut stream.views {
            if !ids.contains(id) {
                v.surface.attach(None, 0, 0);
                v.surface.commit();
                v.source = None;
            }
        }
        let mut previous = root.clone();
        let mut previous_id = None;
        for (l, acquire) in staged.drain(..) {
            if !stream.views.contains_key(&l.id) {
                let surface = self.compositor.create_surface(qh);
                let role = self
                    .layer_globals
                    .subcompositor
                    .get_subsurface(&surface, &root, qh, ());
                role.set_sync();
                let viewport = self.viewporter.get_viewport(&surface, qh, ());
                let alpha =
                    self.layer_globals
                        .alpha
                        .as_ref()
                        .unwrap()
                        .get_surface(&surface, qh, ());
                let sync = None;
                // Route input to the root, retaining task-relative Android coordinates.
                let empty = Region::new(self.compositor.as_ref())?;
                surface.set_input_region(Some(empty.wl_region()));
                stream.views.insert(
                    l.id,
                    View {
                        surface,
                        role,
                        viewport,
                        alpha,
                        sync,
                        source: None,
                        solid: None,
                        geometry: None,
                    },
                );
            }
            let v = stream.views.get_mut(&l.id).unwrap();
            let geometry = Geometry::new(&l, previous_id, (lw, lh), (width, height));
            if v.geometry != Some(geometry) {
                v.role.place_above(&previous);
                let [dx, dy, dw, dh] = geometry.destination;
                v.role.set_position(dx, dy);
                v.viewport.set_destination(dw, dh);
                v.alpha.set_multiplier(geometry.alpha);
                if geometry.opaque {
                    let r = Region::new(self.compositor.as_ref())?;
                    r.add(0, 0, dw, dh);
                    v.surface.set_opaque_region(Some(r.wl_region()));
                } else {
                    v.surface.set_opaque_region(None);
                }
                v.surface.set_buffer_transform(
                    wl_output::Transform::try_from(geometry.transform)
                        .map_err(|_| PresenterError::Configuration("layer transform"))?,
                );
                v.viewport.set_source(
                    f64::from(geometry.source[0]) / 256.0,
                    f64::from(geometry.source[1]) / 256.0,
                    f64::from(geometry.source[2]) / 256.0,
                    f64::from(geometry.source[3]) / 256.0,
                );
                v.geometry = Some(geometry);
            }
            previous = v.surface.clone();
            previous_id = Some(l.id);
            if l.buffer == 0 {
                if let Some(sync) = v.sync.take() {
                    sync.destroy();
                }
                if v.solid.as_ref().is_none_or(|(color, _)| *color != l.color) {
                    let b = pixel(&self.layer_globals, qh, l.color)?;
                    if let Some((_, old)) = v.solid.replace((l.color, b)) {
                        old.destroy();
                    }
                }
                v.surface.attach(v.solid.as_ref().map(|(_, b)| b), 0, 0);
                v.source = None;
            } else {
                let b = stream.buffers.get_mut(&l.buffer).unwrap();
                if b.ticket.is_none() {
                    let acquire_point = b
                        .sync
                        .next_point
                        .checked_add(1)
                        .ok_or(PresenterError::Configuration("layer timeline exhausted"))?;
                    let release = acquire_point
                        .checked_add(1)
                        .ok_or(PresenterError::Configuration("layer timeline exhausted"))?;
                    if let Some(fd) = acquire {
                        b.sync
                            .timeline
                            .import_acquire_fence(acquire_point, fd.as_fd())?;
                    } else {
                        b.sync.timeline.signal_acquire(acquire_point)?;
                    }
                    b.sync.next_point = release;
                    b.sync.arm_release_wakeup(release)?;
                    // Fence availability unblocks Android callbacks. Completion
                    // separately permits this allocation/timeline to be reused.
                    b.availability_wakeup.arm(&b.sync.timeline, release)?;
                    let sync = v
                        .sync
                        .get_or_insert_with(|| self.sync_manager.get_surface(&v.surface, qh, ()));
                    let (hi, lo) = split_point(acquire_point);
                    sync.set_acquire_point(&b.sync.proxy, hi, lo);
                    let (hi, lo) = split_point(release);
                    sync.set_release_point(&b.sync.proxy, hi, lo);
                    v.surface.attach(b.buffer.as_ref(), 0, 0);
                    b.ticket = Some(l.ticket);
                    stream.last_ticket = stream.last_ticket.max(l.ticket);
                    stream.reads.push(Read {
                        buffer: l.buffer,
                        ticket: l.ticket,
                        point: release,
                        fence_sent: false,
                    });
                }
                v.source = Some((l.buffer, l.ticket));
            }
            v.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            v.surface.commit();
        }
        // Retain the bounded layer capacity across buffer-only frames.
        stream.staged.recycle(staged);
        // Removed roles can be destroyed after their detach is committed with
        // the root. Keep them through this transaction to avoid partial updates.
        if let Some(sync) = task.sync_surface.take() {
            sync.destroy();
        }
        if !stream.active || root_changed || stream.root_logical_size != Some((lw, lh)) {
            let region = Region::new(self.compositor.as_ref())?;
            region.add(0, 0, lw as i32, lh as i32);
            root.set_opaque_region(Some(region.wl_region()));
            root.set_buffer_transform(wl_output::Transform::Normal);
            task.viewport.as_ref().unwrap().set_source(
                0.0,
                0.0,
                f64::from(root_size.0),
                f64::from(root_size.1),
            );
            task.viewport
                .as_ref()
                .unwrap()
                .set_destination(lw as i32, lh as i32);
            stream.root_logical_size = Some((lw, lh));
        }
        if root_changed || !stream.active {
            root.attach(stream.root.as_ref().map(|(_, buffer)| buffer), 0, 0);
            root.damage_buffer(0, 0, root_size.0 as i32, root_size.1 as i32);
        }
        root.commit();
        self.activation.mapped(object, &root);
        if !stream.active {
            eprintln!(
                "Droidloom task {}: direct Wayland composition, {} layers",
                object.0,
                ids.len()
            );
        }
        task.applied_opaque = Some(true);
        stream.active = true;
        stream.views.retain(|id, _| ids.contains(id));
        Ok(())
    }
    pub(super) fn layer_imported(
        &mut self,
        params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        buffer: Option<wl_buffer::WlBuffer>,
    ) -> bool {
        for task in self.tasks.values_mut() {
            let Some(s) = task.layers.as_mut() else {
                continue;
            };
            if s.importing.as_ref().is_none_or(|(_, p)| p != params) {
                continue;
            }
            let (id, p) = s.importing.take().unwrap();
            p.destroy();
            let code = if let Some(buffer) = buffer {
                s.buffers.get_mut(&id).unwrap().buffer = Some(buffer);
                0
            } else {
                s.buffers.remove(&id);
                -95
            };
            if let Err(e) = s.ack(code) {
                self.fatal = Some(e.to_string());
            }
            return true;
        }
        false
    }
}
wayland_client::delegate_noop!(App: ignore wl_shm::WlShm);
wayland_client::delegate_noop!(App: ignore wl_shm_pool::WlShmPool);
wayland_client::delegate_noop!(App: ignore wl_subcompositor::WlSubcompositor);
wayland_client::delegate_noop!(App: ignore wl_subsurface::WlSubsurface);
wayland_client::delegate_noop!(App: ignore wl_buffer::WlBuffer);
wayland_client::delegate_noop!(App: ignore WpAlphaModifierV1);
wayland_client::delegate_noop!(App: ignore WpAlphaModifierSurfaceV1);

#[cfg(test)]
mod tests {
    use super::{Geometry, Layer, carrier_size};

    fn fixture() -> Layer {
        Layer {
            id: 1,
            buffer: 10,
            ticket: 1,
            destination: [0, 0, 800, 600],
            source: [0, 0, 800 * 256, 600 * 256],
            transform: 0,
            alpha: u32::MAX,
            opaque: true,
            color: 0,
        }
    }

    #[test]
    fn rotating_producer_buffers_preserves_wayland_geometry() {
        let mut layer = fixture();
        let original = Geometry::new(&layer, None, (400, 300), (800, 600));
        assert_eq!(original.destination, [0, 0, 400, 300]);
        for frame in 2..100 {
            layer.buffer = 10 + frame % 3;
            layer.ticket = frame;
            assert_eq!(
                Geometry::new(&layer, None, (400, 300), (800, 600)),
                original
            );
        }
        assert_ne!(
            Geometry::new(&layer, None, (800, 600), (800, 600)),
            original
        );
        assert_ne!(
            Geometry::new(&layer, Some(2), (400, 300), (800, 600)),
            original
        );
        for change in 0..5 {
            let mut changed = layer.clone();
            match change {
                0 => changed.opaque = false,
                1 => changed.alpha /= 2,
                2 => changed.transform = 2,
                3 => changed.source[2] /= 2,
                _ => changed.destination[2] /= 2,
            }
            assert_ne!(
                Geometry::new(&changed, None, (400, 300), (800, 600)),
                original
            );
        }
    }

    #[test]
    fn solid_buffer_transitions_restore_the_correct_viewport_source() {
        let mut layer = fixture();
        let buffered = Geometry::new(&layer, None, (400, 300), (800, 600));
        layer.buffer = 0;
        let solid = Geometry::new(&layer, None, (400, 300), (800, 600));
        assert_eq!(solid.source, [0, 0, 256, 256]);
        assert_ne!(solid, buffered);
        layer.color = 0xffaabbcc;
        assert_eq!(Geometry::new(&layer, None, (400, 300), (800, 600)), solid);
        layer.buffer = 11;
        assert_eq!(
            Geometry::new(&layer, None, (400, 300), (800, 600)),
            buffered
        );
    }

    #[test]
    fn root_backing_preserves_task_aspect_through_resize_and_rotation() {
        // The Moto task is 610 x 1356 logical pixels. A 1 x 1 carrier made
        // the mobile shell fit its portrait surface tree into a square first.
        for (width, height) in [(610, 1356), (1356, 610), (611, 1357), (800, 600)] {
            let (root_width, root_height) = carrier_size(width, height).unwrap();
            assert!(root_width > 0 && root_height > 0);
            assert_eq!(
                u64::from(root_width) * u64::from(height),
                u64::from(root_height) * u64::from(width),
                "root aspect changed for {width} x {height}",
            );
            assert!(root_width <= width && root_height <= height);
        }
        assert!(carrier_size(0, 1356).is_err());
        assert!(carrier_size(610, 0).is_err());
    }
}
