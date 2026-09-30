//! Client navigation with one explicit Android/body geometry and no root reparenting.

use super::*;

pub(super) const HEIGHT: u32 = 48;
pub(super) const BUTTON_SIZE: u32 = 48;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Layout {
    pub content: LogicalSize,
    pub body: LogicalSize,
    pub scale_120: u32,
    pub fullscreen: bool,
}

impl Layout {
    pub fn new(content: LogicalSize, scale_120: u32, fullscreen: bool) -> Result<Self, PresenterError> {
        if content.width < BUTTON_SIZE * 2 {
            return Err(PresenterError::Configuration("window is too narrow for navigation"));
        }
        let height = content.height.checked_add(HEIGHT)
            .filter(|height| *height <= MAX_BUFFER_DIMENSION)
            .ok_or(PresenterError::Configuration("navigation geometry exceeds protocol limit"))?;
        scale_dimension(content.width, scale_120)?;
        scale_dimension(content.height, scale_120)?;
        Ok(Self {
            content,
            body: LogicalSize { width: content.width, height },
            scale_120,
            fullscreen,
        })
    }
}

pub(super) fn content_height(height: Option<NonZeroU32>, reserve: u32) -> Result<Option<NonZeroU32>, PresenterError> {
    height.map(|height| height.get().checked_sub(reserve).and_then(NonZeroU32::new)
        .ok_or(PresenterError::Configuration("window has no space for navigation and content")))
        .transpose()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Action { Back, Fullscreen }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum VisualState { Normal, Hover, Pressed }

struct Buffer(wl_buffer::WlBuffer);
impl Drop for Buffer {
    fn drop(&mut self) { self.0.destroy(); }
}

fn buffer(globals: &layers::Globals, qh: &QueueHandle<App>, width: u32, height: u32, pixels: &[u32]) -> Result<Buffer, PresenterError> {
    let mut file = tempfile::tempfile()?;
    let bytes = usize::try_from(u64::from(width) * u64::from(height) * 4)
        .map_err(|_| PresenterError::Configuration("chrome buffer size overflow"))?;
    if pixels.len() != bytes / 4 || bytes > i32::MAX as usize {
        return Err(PresenterError::Configuration("invalid chrome buffer"));
    }
    let mut encoded = Vec::with_capacity(bytes);
    for pixel in pixels { encoded.extend_from_slice(&pixel.to_ne_bytes()); }
    file.write_all(&encoded)?;
    let pool = globals.shm.create_pool(file.as_fd(), bytes as i32, qh, ());
    let image = pool.create_buffer(0, width as i32, height as i32, (width * 4) as i32,
        wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    Ok(Buffer(image))
}

pub(super) struct Rail {
    pub surface: wl_surface::WlSurface,
    role: wl_subsurface::WlSubsurface,
    viewport: WpViewport,
    _buffer: Buffer,
    pub layout: Layout,
}

impl Rail {
    pub fn desynchronize(&self) { self.role.set_desync(); }
    pub fn new(globals: &layers::Globals, compositor: &CompositorState, viewporter: &WpViewporter,
        parent: &wl_surface::WlSurface, qh: &QueueHandle<App>, layout: Layout) -> Result<Self, PresenterError> {
        let theme = sctk_adwaita::theme::ColorTheme::auto();
        let dark = theme.active.button_icon.red() > 0.5;
        let image = buffer(globals, qh, 1, 1, &[if dark { 0xff24262b } else { 0xffeeeeef }])?;
        let surface = compositor.create_surface(qh);
        let role = globals.subcompositor.get_subsurface(&surface, parent, qh, ());
        let viewport = viewporter.get_viewport(&surface, qh, ());
        viewport.set_destination(layout.content.width as i32, HEIGHT as i32);
        role.set_position(0, layout.content.height as i32);
        // Geometry stays synchronized with the parent's configure transaction.
        surface.attach(Some(&image.0), 0, 0);
        surface.damage_buffer(0, 0, 1, 1);
        surface.commit();
        Ok(Self { surface, role, viewport, _buffer: image, layout })
    }
}

impl Drop for Rail {
    fn drop(&mut self) {
        self.viewport.destroy();
        self.role.destroy();
        self.surface.destroy();
    }
}

pub(super) struct Button {
    pub kind: Action,
    pub surface: wl_surface::WlSurface,
    role: wl_subsurface::WlSubsurface,
    viewport: WpViewport,
    pub visible: bool,
    state: VisualState,
    pixels: u32,
    images: [Buffer; 3],
}

impl Button {
    pub fn new(kind: Action, globals: &layers::Globals, compositor: &CompositorState,
        viewporter: &WpViewporter, rail: &Rail, qh: &QueueHandle<App>) -> Result<Self, PresenterError> {
        let size = scale_dimension(BUTTON_SIZE, rail.layout.scale_120)?;
        let theme = sctk_adwaita::theme::ColorTheme::auto();
        let dark = theme.active.button_icon.red() > 0.5;
        let images = [
            render_button(globals, qh, kind, VisualState::Normal, rail.layout.fullscreen, size, dark)?,
            render_button(globals, qh, kind, VisualState::Hover, rail.layout.fullscreen, size, dark)?,
            render_button(globals, qh, kind, VisualState::Pressed, rail.layout.fullscreen, size, dark)?,
        ];
        let surface = compositor.create_surface(qh);
        let role = globals.subcompositor.get_subsurface(&surface, &rail.surface, qh, ());
        role.set_position(match kind { Action::Back => 0, Action::Fullscreen => rail.layout.content.width as i32 - BUTTON_SIZE as i32 }, 0);
        role.set_desync();
        let viewport = viewporter.get_viewport(&surface, qh, ());
        viewport.set_destination(BUTTON_SIZE as i32, BUTTON_SIZE as i32);
        let mut button = Self { kind, surface, role, viewport, visible: false, state: VisualState::Normal, pixels: size, images };
        button.set_visible(true);
        Ok(button)
    }

    pub fn set_state(&mut self, state: VisualState) {
        if self.state == state { return; }
        self.state = state;
        if self.visible { self.attach(); }
    }

    pub fn set_visible(&mut self, visible: bool) {
        if self.visible == visible { return; }
        self.visible = visible;
        if visible { self.attach(); } else { self.surface.attach(None, 0, 0); self.surface.commit(); }
    }

    fn attach(&self) {
        let index = match self.state { VisualState::Normal => 0, VisualState::Hover => 1, VisualState::Pressed => 2 };
        self.surface.attach(Some(&self.images[index].0), 0, 0);
        self.surface.damage_buffer(0, 0, self.pixels as i32, self.pixels as i32);
        self.surface.commit();
    }
}

impl Drop for Button {
    fn drop(&mut self) {
        self.viewport.destroy();
        self.role.destroy();
        self.surface.destroy();
    }
}

fn render_button(globals: &layers::Globals, qh: &QueueHandle<App>, kind: Action,
    state: VisualState, fullscreen: bool, size: u32, dark: bool) -> Result<Buffer, PresenterError> {
    let mut pixels = vec![0u32; (size * size) as usize];
    let background = if dark { 0x24262b } else { 0xeeeeef };
    let foreground = if dark { 0xf4f4f4 } else { 0x24262b };
    let highlight = match state { VisualState::Normal => background, VisualState::Hover => if dark { 0x41444a } else { 0xd9dadd }, VisualState::Pressed => if dark { 0x5a5d63 } else { 0xc0c2c6 } };
    for y in 0..size {
        for x in 0..size {
            let px = (x as f32 + 0.5) * BUTTON_SIZE as f32 / size as f32;
            let py = (y as f32 + 0.5) * BUTTON_SIZE as f32 / size as f32;
            let circle = ((px - 24.0).powi(2) + (py - 24.0).powi(2)).sqrt() <= 20.0;
            let segments: &[((f32, f32), (f32, f32))] = match kind {
                Action::Back => &[((27.0, 15.0), (18.0, 24.0)), ((18.0, 24.0), (27.0, 33.0))],
                Action::Fullscreen if fullscreen => &[((16.0, 22.0), (22.0, 22.0)), ((22.0, 22.0), (22.0, 16.0)), ((26.0, 32.0), (26.0, 26.0)), ((26.0, 26.0), (32.0, 26.0))],
                Action::Fullscreen => &[((16.0, 22.0), (16.0, 16.0)), ((16.0, 16.0), (22.0, 16.0)), ((26.0, 32.0), (32.0, 32.0)), ((32.0, 32.0), (32.0, 26.0))],
            };
            let distance = segments.iter().map(|&((ax, ay), (bx, by))| {
                let dx = bx - ax; let dy = by - ay;
                let t = (((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)).clamp(0.0, 1.0);
                ((px - ax - t * dx).powi(2) + (py - ay - t * dy).powi(2)).sqrt()
            }).fold(f32::INFINITY, f32::min);
            let color = if distance <= 1.5 { foreground } else if circle { highlight } else { background };
            pixels[(y * size + x) as usize] = 0xff000000 | color;
        }
    }
    buffer(globals, qh, size, size, &pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rail_extends_body_without_changing_android_origin() {
        let layout = Layout::new(LogicalSize { width: 800, height: 517 }, 240, false).unwrap();
        assert_eq!(layout.body, LogicalSize { width: 800, height: 565 });
        assert_eq!(content_height(NonZeroU32::new(565), HEIGHT).unwrap().unwrap().get(), 517);
    }
    #[test]
    fn missing_height_is_not_subtracted_and_tiny_geometry_is_rejected() {
        assert_eq!(content_height(None, HEIGHT).unwrap(), None);
        assert!(content_height(NonZeroU32::new(48), HEIGHT).is_err());
        assert!(Layout::new(LogicalSize { width: 40, height: 200 }, 120, false).is_err());
    }
    #[test]
    fn fractional_chrome_uses_the_same_rounding_as_content() {
        assert_eq!(scale_dimension(BUTTON_SIZE, 180).unwrap(), 72);
        assert_eq!(scale_dimension(BUTTON_SIZE, 240).unwrap(), 96);
    }
}
