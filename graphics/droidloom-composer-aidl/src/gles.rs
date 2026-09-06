//! Narrow EGL/GLES compositor for Android layers and Denial-owned targets.
//!
//! All unsafe code in the Composer AIDL crate is confined here because EGL,
//! GLES and their extension entry points are C ABIs. The public surface
//! owns or borrows typed Rust descriptors and never exposes raw graphics
//! handles.

#![allow(unsafe_code)]

use std::ffi::{c_char, c_void};
use std::fmt;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd};
use std::ptr;

use droidloom_transport::BufferMetadata;

use crate::minigbm::{ImportedRenderTarget, PreparedLayer};

type EglDisplay = *mut c_void;
type EglContext = *mut c_void;
type EglConfig = *mut c_void;
type EglImage = *mut c_void;
type EglSync = *mut c_void;
type EglBoolean = u32;
type EglInt = i32;
type GlEnum = u32;
type GlInt = i32;
type GlUint = u32;
type GlSize = i32;

const EGL_FALSE: EglBoolean = 0;
const EGL_NONE: EglInt = 0x3038;
const EGL_OPENGL_ES_API: EglInt = 0x30a0;
const EGL_RENDERABLE_TYPE: EglInt = 0x3040;
const EGL_OPENGL_ES2_BIT: EglInt = 0x0004;
const EGL_RED_SIZE: EglInt = 0x3024;
const EGL_GREEN_SIZE: EglInt = 0x3023;
const EGL_BLUE_SIZE: EglInt = 0x3022;
const EGL_ALPHA_SIZE: EglInt = 0x3021;
const EGL_CONTEXT_CLIENT_VERSION: EglInt = 0x3098;
const EGL_LINUX_DMA_BUF_EXT: GlEnum = 0x3270;
const EGL_WIDTH: EglInt = 0x3057;
const EGL_HEIGHT: EglInt = 0x3056;
const EGL_LINUX_DRM_FOURCC_EXT: EglInt = 0x3271;
const EGL_SYNC_NATIVE_FENCE_ANDROID: GlEnum = 0x3144;
const EGL_SYNC_NATIVE_FENCE_FD_ANDROID: EglInt = 0x3145;
const EGL_NO_NATIVE_FENCE_FD_ANDROID: EglInt = -1;

const GL_VERTEX_SHADER: GlEnum = 0x8b31;
const GL_FRAGMENT_SHADER: GlEnum = 0x8b30;
const GL_COMPILE_STATUS: GlEnum = 0x8b81;
const GL_LINK_STATUS: GlEnum = 0x8b82;
const GL_ARRAY_BUFFER: GlEnum = 0x8892;
const GL_STREAM_DRAW: GlEnum = 0x88e0;
const GL_FLOAT: GlEnum = 0x1406;
const GL_TEXTURE_2D: GlEnum = 0x0de1;
const GL_TEXTURE0: GlEnum = 0x84c0;
const GL_TEXTURE_MIN_FILTER: GlEnum = 0x2801;
const GL_TEXTURE_MAG_FILTER: GlEnum = 0x2800;
const GL_TEXTURE_WRAP_S: GlEnum = 0x2802;
const GL_TEXTURE_WRAP_T: GlEnum = 0x2803;
const GL_LINEAR: GlInt = 0x2601;
const GL_CLAMP_TO_EDGE: GlInt = 0x812f;
const GL_FRAMEBUFFER: GlEnum = 0x8d40;
const GL_COLOR_ATTACHMENT0: GlEnum = 0x8ce0;
const GL_FRAMEBUFFER_COMPLETE: GlEnum = 0x8cd5;
const GL_COLOR_BUFFER_BIT: GlEnum = 0x0000_4000;
const GL_TRIANGLE_STRIP: GlEnum = 0x0005;
const GL_BLEND: GlEnum = 0x0be2;
const GL_ONE: GlEnum = 1;
const GL_SRC_ALPHA: GlEnum = 0x0302;
const GL_ONE_MINUS_SRC_ALPHA: GlEnum = 0x0303;

const PLANE_FD: [EglInt; 4] = [0x3272, 0x3275, 0x3278, 0x3440];
const PLANE_OFFSET: [EglInt; 4] = [0x3273, 0x3276, 0x3279, 0x3441];
const PLANE_PITCH: [EglInt; 4] = [0x3274, 0x3277, 0x327a, 0x3442];
const PLANE_MODIFIER_LO: [EglInt; 4] = [0x3443, 0x3445, 0x3447, 0x3449];
const PLANE_MODIFIER_HI: [EglInt; 4] = [0x3444, 0x3446, 0x3448, 0x344a];

const VERTEX_SHADER: &[u8] = b"attribute vec2 a_position;
attribute vec2 a_texcoord;
varying vec2 v_texcoord;
void main() {
  gl_Position = vec4(a_position, 0.0, 1.0);
  v_texcoord = a_texcoord;
}
\0";

const FRAGMENT_SHADER: &[u8] = b"precision mediump float;
varying vec2 v_texcoord;
uniform sampler2D u_texture;
uniform vec4 u_color;
uniform float u_alpha;
uniform int u_textured;
void main() {
  vec4 source = u_textured != 0 ? texture2D(u_texture, v_texcoord) : u_color;
  gl_FragColor = source * u_alpha;
}
\0";

/// EGL/GLES setup, import, shader, or native-fence failure.
#[derive(Debug)]
pub struct GlesError(&'static str);

impl fmt::Display for GlesError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for GlesError {}

type CreateImage =
    unsafe extern "C" fn(EglDisplay, EglContext, GlEnum, *mut c_void, *const EglInt) -> EglImage;
type DestroyImage = unsafe extern "C" fn(EglDisplay, EglImage) -> EglBoolean;
type ImageTargetTexture = unsafe extern "C" fn(GlEnum, EglImage);
type CreateSync = unsafe extern "C" fn(EglDisplay, GlEnum, *const EglInt) -> EglSync;
type DestroySync = unsafe extern "C" fn(EglDisplay, EglSync) -> EglBoolean;
type WaitSync = unsafe extern "C" fn(EglDisplay, EglSync, EglInt) -> EglBoolean;
type DupNativeFence = unsafe extern "C" fn(EglDisplay, EglSync) -> EglInt;

#[derive(Debug)]
struct Extensions {
    create_image: CreateImage,
    destroy_image: DestroyImage,
    image_target_texture: ImageTargetTexture,
    create_sync: CreateSync,
    destroy_sync: DestroySync,
    wait_sync: WaitSync,
    dup_native_fence: DupNativeFence,
}

/// Stateful renderer shared by the Composer service's presentation sink.
#[derive(Debug)]
pub struct LayerCompositor {
    _render_fd: OwnedFd,
    display: EglDisplay,
    context: EglContext,
    extensions: Extensions,
    program: GlUint,
    vertex_buffer: GlUint,
    framebuffer: GlUint,
    position: GlInt,
    texcoord: GlInt,
    texture_uniform: GlInt,
    color_uniform: GlInt,
    alpha_uniform: GlInt,
    textured_uniform: GlInt,
}

// The context is made current only while guarded by the sink's mutex. EGL and
// Mesa permit moving an unbound context between threads, but not concurrent use.
unsafe impl Send for LayerCompositor {}

impl LayerCompositor {
    /// Create one surfaceless GLES2 context on the selected render node.
    pub fn new(render_fd: BorrowedFd<'_>) -> Result<Self, GlesError> {
        let render_fd = render_fd
            .try_clone_to_owned()
            .map_err(|_| GlesError("duplicate render-node descriptor"))?;
        // Android's EGL loader owns display/device selection. It rejects the
        // desktop EGL_PLATFORM_GBM_KHR entry point even when Mesa is backing
        // the implementation, while EGL_DEFAULT_DISPLAY supports the same
        // dma-buf image import extensions used below.
        let display = unsafe { eglGetDisplay(ptr::null_mut()) };
        if display.is_null() {
            return Err(GlesError("eglGetDisplay failed"));
        }
        let mut major = 0;
        let mut minor = 0;
        if unsafe { eglInitialize(display, &mut major, &mut minor) } == EGL_FALSE
            || unsafe { eglBindAPI(EGL_OPENGL_ES_API) } == EGL_FALSE
        {
            return Err(GlesError("initialize EGL GLES display"));
        }
        let config_attributes = [
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES2_BIT,
            EGL_RED_SIZE,
            8,
            EGL_GREEN_SIZE,
            8,
            EGL_BLUE_SIZE,
            8,
            EGL_ALPHA_SIZE,
            8,
            EGL_NONE,
        ];
        let mut config: EglConfig = ptr::null_mut();
        let mut count = 0;
        if unsafe {
            eglChooseConfig(
                display,
                config_attributes.as_ptr(),
                &mut config,
                1,
                &mut count,
            )
        } == EGL_FALSE
            || count != 1
        {
            unsafe { eglTerminate(display) };
            return Err(GlesError("choose EGL config"));
        }
        let context_attributes = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
        let context = unsafe {
            eglCreateContext(
                display,
                config,
                ptr::null_mut(),
                context_attributes.as_ptr(),
            )
        };
        if context.is_null()
            || unsafe { eglMakeCurrent(display, ptr::null_mut(), ptr::null_mut(), context) }
                == EGL_FALSE
        {
            unsafe { eglTerminate(display) };
            return Err(GlesError("create surfaceless GLES context"));
        }
        let extensions = unsafe { Extensions::load()? };
        let program = unsafe { create_program()? };
        let mut vertex_buffer = 0;
        let mut framebuffer = 0;
        unsafe {
            glGenBuffers(1, &mut vertex_buffer);
            glGenFramebuffers(1, &mut framebuffer);
        }
        let position = uniform_or_attribute(program, b"a_position\0", true)?;
        let texcoord = uniform_or_attribute(program, b"a_texcoord\0", true)?;
        let texture_uniform = uniform_or_attribute(program, b"u_texture\0", false)?;
        let color_uniform = uniform_or_attribute(program, b"u_color\0", false)?;
        let alpha_uniform = uniform_or_attribute(program, b"u_alpha\0", false)?;
        let textured_uniform = uniform_or_attribute(program, b"u_textured\0", false)?;
        unsafe { eglMakeCurrent(display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut()) };
        Ok(Self {
            _render_fd: render_fd,
            display,
            context,
            extensions,
            program,
            vertex_buffer,
            framebuffer,
            position,
            texcoord,
            texture_uniform,
            color_uniform,
            alpha_uniform,
            textured_uniform,
        })
    }

    /// Compose one complete task frame and return its native completion fence.
    pub fn compose(
        &mut self,
        target: &ImportedRenderTarget,
        layers: &mut [PreparedLayer],
        display_extent: (u32, u32),
    ) -> Result<OwnedFd, GlesError> {
        if unsafe { eglMakeCurrent(self.display, ptr::null_mut(), ptr::null_mut(), self.context) }
            == EGL_FALSE
        {
            return Err(GlesError("make GLES context current"));
        }
        let result = self.compose_current(target, layers, display_extent);
        unsafe {
            eglMakeCurrent(
                self.display,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
        }
        result
    }

    fn compose_current(
        &mut self,
        target: &ImportedRenderTarget,
        layers: &mut [PreparedLayer],
        display_extent: (u32, u32),
    ) -> Result<OwnedFd, GlesError> {
        let target_fds = target.plane_fds.iter().map(AsFd::as_fd).collect::<Vec<_>>();
        let target_image = self.import_image(&target.metadata, &target_fds)?;
        let mut target_texture = 0;
        unsafe {
            glGenTextures(1, &mut target_texture);
            bind_image_texture(&self.extensions, target_texture, target_image);
            glBindFramebuffer(GL_FRAMEBUFFER, self.framebuffer);
            glFramebufferTexture2D(
                GL_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                target_texture,
                0,
            );
        }
        if unsafe { glCheckFramebufferStatus(GL_FRAMEBUFFER) } != GL_FRAMEBUFFER_COMPLETE {
            self.destroy_texture_image(target_texture, target_image);
            return Err(GlesError(
                "Denial render target is not framebuffer complete",
            ));
        }
        let width = i32::try_from(target.metadata.width).map_err(|_| GlesError("target width"))?;
        let height =
            i32::try_from(target.metadata.height).map_err(|_| GlesError("target height"))?;
        unsafe {
            glViewport(0, 0, width, height);
            glClearColor(0.0, 0.0, 0.0, 0.0);
            glClear(GL_COLOR_BUFFER_BIT);
            glUseProgram(self.program);
            glBindBuffer(GL_ARRAY_BUFFER, self.vertex_buffer);
            glEnableVertexAttribArray(self.position as GlUint);
            glEnableVertexAttribArray(self.texcoord as GlUint);
            glUniform1i(self.texture_uniform, 0);
        }

        // SurfaceFlinger continues to describe layer geometry in the display
        // space announced at hotplug even after Denial resizes the containing
        // native window and supplies a differently sized render target. Map
        // that stable Android space over the complete current target instead
        // of interpreting Android coordinates as target pixels.
        let (display_width, display_height) = display_extent;
        if display_width == 0 || display_height == 0 {
            return Err(GlesError("invalid Android display extent"));
        }

        for layer in layers {
            if let Some(fence) = layer.acquire_fence.take() {
                self.wait_fence(fence.as_fd())?;
            }
            self.draw_layer(layer, display_width, display_height)?;
        }

        let fence = self.export_fence()?;
        unsafe {
            glDisableVertexAttribArray(self.position as GlUint);
            glDisableVertexAttribArray(self.texcoord as GlUint);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
        }
        self.destroy_texture_image(target_texture, target_image);
        Ok(fence)
    }

    fn draw_layer(
        &self,
        layer: &PreparedLayer,
        target_width: u32,
        target_height: u32,
    ) -> Result<(), GlesError> {
        let mut texture_image = None;
        let (source_width, source_height) = if let Some(buffer) = &layer.buffer {
            let fds = buffer
                .plane_fds
                .iter()
                .map(|fd| fd.as_fd())
                .collect::<Vec<_>>();
            let image = self.import_image(&buffer.metadata.buffer, &fds)?;
            let mut texture = 0;
            unsafe {
                glGenTextures(1, &mut texture);
                bind_image_texture(&self.extensions, texture, image);
            }
            texture_image = Some((texture, image));
            (
                buffer.metadata.buffer.width as f32,
                buffer.metadata.buffer.height as f32,
            )
        } else {
            (1.0, 1.0)
        };
        let vertices = vertices(
            layer,
            target_width,
            target_height,
            source_width,
            source_height,
        )?;
        unsafe {
            glBufferData(
                GL_ARRAY_BUFFER,
                isize::try_from(std::mem::size_of_val(&vertices))
                    .map_err(|_| GlesError("vertex buffer size"))?,
                vertices.as_ptr().cast(),
                GL_STREAM_DRAW,
            );
            glVertexAttribPointer(self.position as GlUint, 2, GL_FLOAT, 0, 16, ptr::null());
            glVertexAttribPointer(
                self.texcoord as GlUint,
                2,
                GL_FLOAT,
                0,
                16,
                8_usize as *const c_void,
            );
            glUniform1f(self.alpha_uniform, layer.plane_alpha);
            if let Some(color) = layer.solid_color {
                glUniform1i(self.textured_uniform, 0);
                glUniform4f(self.color_uniform, color[0], color[1], color[2], color[3]);
            } else {
                glUniform1i(self.textured_uniform, 1);
                glActiveTexture(GL_TEXTURE0);
            }
            match layer.blend_mode {
                1 => glDisable(GL_BLEND),
                2 => {
                    glEnable(GL_BLEND);
                    glBlendFunc(GL_ONE, GL_ONE_MINUS_SRC_ALPHA);
                }
                3 => {
                    glEnable(GL_BLEND);
                    glBlendFunc(GL_SRC_ALPHA, GL_ONE_MINUS_SRC_ALPHA);
                }
                _ => return Err(GlesError("unsupported Android blend mode")),
            }
            glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);
        }
        if let Some((texture, image)) = texture_image {
            self.destroy_texture_image(texture, image);
        }
        Ok(())
    }

    fn import_image(
        &self,
        metadata: &BufferMetadata,
        fds: &[BorrowedFd<'_>],
    ) -> Result<EglImage, GlesError> {
        if fds.len() != metadata.planes.len() || fds.is_empty() || fds.len() > 4 {
            return Err(GlesError("DMA-BUF plane table mismatch"));
        }
        let mut attributes = Vec::with_capacity(8 + fds.len() * 12);
        attributes.extend_from_slice(&[
            EGL_WIDTH,
            i32::try_from(metadata.width).map_err(|_| GlesError("image width"))?,
            EGL_HEIGHT,
            i32::try_from(metadata.height).map_err(|_| GlesError("image height"))?,
            EGL_LINUX_DRM_FOURCC_EXT,
            metadata.format.fourcc as i32,
        ]);
        for (index, (fd, plane)) in fds.iter().zip(&metadata.planes).enumerate() {
            let modifier = metadata.format.modifier;
            attributes.extend_from_slice(&[
                PLANE_FD[index],
                fd.as_raw_fd(),
                PLANE_OFFSET[index],
                i32::try_from(plane.offset).map_err(|_| GlesError("plane offset"))?,
                PLANE_PITCH[index],
                i32::try_from(plane.stride).map_err(|_| GlesError("plane stride"))?,
                PLANE_MODIFIER_LO[index],
                modifier as u32 as i32,
                PLANE_MODIFIER_HI[index],
                (modifier >> 32) as u32 as i32,
            ]);
        }
        attributes.push(EGL_NONE);
        let image = unsafe {
            (self.extensions.create_image)(
                self.display,
                ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                ptr::null_mut(),
                attributes.as_ptr(),
            )
        };
        if image.is_null() {
            Err(GlesError("import DMA-BUF EGLImage"))
        } else {
            Ok(image)
        }
    }

    fn wait_fence(&self, fence: BorrowedFd<'_>) -> Result<(), GlesError> {
        let owned = fence
            .try_clone_to_owned()
            .map_err(|_| GlesError("duplicate layer acquire fence"))?;
        let raw = owned.into_raw_fd();
        let attributes = [EGL_SYNC_NATIVE_FENCE_FD_ANDROID, raw, EGL_NONE];
        let sync = unsafe {
            (self.extensions.create_sync)(
                self.display,
                EGL_SYNC_NATIVE_FENCE_ANDROID,
                attributes.as_ptr(),
            )
        };
        if sync.is_null() {
            unsafe { drop(OwnedFd::from_raw_fd(raw)) };
            return Err(GlesError("import layer acquire fence"));
        }
        let waited = unsafe { (self.extensions.wait_sync)(self.display, sync, 0) };
        unsafe { (self.extensions.destroy_sync)(self.display, sync) };
        if waited == EGL_FALSE {
            Err(GlesError("wait layer acquire fence"))
        } else {
            Ok(())
        }
    }

    fn export_fence(&self) -> Result<OwnedFd, GlesError> {
        let attributes = [
            EGL_SYNC_NATIVE_FENCE_FD_ANDROID,
            EGL_NO_NATIVE_FENCE_FD_ANDROID,
            EGL_NONE,
        ];
        let sync = unsafe {
            (self.extensions.create_sync)(
                self.display,
                EGL_SYNC_NATIVE_FENCE_ANDROID,
                attributes.as_ptr(),
            )
        };
        if sync.is_null() {
            return Err(GlesError("create composition native fence"));
        }
        unsafe { glFlush() };
        let raw = unsafe { (self.extensions.dup_native_fence)(self.display, sync) };
        unsafe { (self.extensions.destroy_sync)(self.display, sync) };
        if raw < 0 {
            Err(GlesError("export composition native fence"))
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(raw) })
        }
    }

    fn destroy_texture_image(&self, texture: GlUint, image: EglImage) {
        unsafe {
            glDeleteTextures(1, &texture);
            (self.extensions.destroy_image)(self.display, image);
        }
    }
}

impl Drop for LayerCompositor {
    fn drop(&mut self) {
        unsafe {
            eglMakeCurrent(self.display, ptr::null_mut(), ptr::null_mut(), self.context);
            glDeleteFramebuffers(1, &self.framebuffer);
            glDeleteBuffers(1, &self.vertex_buffer);
            glDeleteProgram(self.program);
            eglMakeCurrent(
                self.display,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
            eglDestroyContext(self.display, self.context);
            eglTerminate(self.display);
        }
    }
}

impl Extensions {
    unsafe fn load() -> Result<Self, GlesError> {
        Ok(Self {
            create_image: unsafe { load_create_image(b"eglCreateImageKHR\0")? },
            destroy_image: unsafe { load_destroy_image(b"eglDestroyImageKHR\0")? },
            image_target_texture: unsafe {
                load_image_target_texture(b"glEGLImageTargetTexture2DOES\0")?
            },
            create_sync: unsafe { load_create_sync(b"eglCreateSyncKHR\0")? },
            destroy_sync: unsafe { load_destroy_sync(b"eglDestroySyncKHR\0")? },
            wait_sync: unsafe { load_wait_sync(b"eglWaitSyncKHR\0")? },
            dup_native_fence: unsafe { load_dup_native_fence(b"eglDupNativeFenceFDANDROID\0")? },
        })
    }
}

macro_rules! extension_loader {
    ($name:ident, $type:ty) => {
        unsafe fn $name(name: &'static [u8]) -> Result<$type, GlesError> {
            let address = unsafe { eglGetProcAddress(name.as_ptr().cast()) };
            if address.is_null() {
                Err(GlesError("required EGL/GLES extension is unavailable"))
            } else {
                Ok(unsafe { std::mem::transmute::<*const c_void, $type>(address) })
            }
        }
    };
}

extension_loader!(load_create_image, CreateImage);
extension_loader!(load_destroy_image, DestroyImage);
extension_loader!(load_image_target_texture, ImageTargetTexture);
extension_loader!(load_create_sync, CreateSync);
extension_loader!(load_destroy_sync, DestroySync);
extension_loader!(load_wait_sync, WaitSync);
extension_loader!(load_dup_native_fence, DupNativeFence);

fn vertices(
    layer: &PreparedLayer,
    target_width: u32,
    target_height: u32,
    source_width: f32,
    source_height: f32,
) -> Result<[f32; 16], GlesError> {
    if target_width == 0 || target_height == 0 || source_width <= 0.0 || source_height <= 0.0 {
        return Err(GlesError("invalid composition dimensions"));
    }
    let frame = layer.display_frame;
    let left = 2.0 * frame.left as f32 / target_width as f32 - 1.0;
    let right = 2.0 * frame.right as f32 / target_width as f32 - 1.0;
    // DMA-BUF rows are consumed by Denial from their top-left origin, while
    // GLES framebuffer storage has a bottom-left origin. Invert clip-space Y
    // while drawing so the exported target is upright when Denial imports it.
    let top = 2.0 * frame.top as f32 / target_height as f32 - 1.0;
    let bottom = 2.0 * frame.bottom as f32 / target_height as f32 - 1.0;
    let crop = layer.source_crop;
    let mut uv = [
        [crop.left / source_width, crop.top / source_height],
        [crop.right / source_width, crop.top / source_height],
        [crop.left / source_width, crop.bottom / source_height],
        [crop.right / source_width, crop.bottom / source_height],
    ];
    if layer.transform & 1 != 0 {
        for coordinate in &mut uv {
            coordinate[0] = 1.0 - coordinate[0];
        }
    }
    if layer.transform & 2 != 0 {
        for coordinate in &mut uv {
            coordinate[1] = 1.0 - coordinate[1];
        }
    }
    if layer.transform & 4 != 0 {
        uv = [uv[2], uv[0], uv[3], uv[1]];
    }
    Ok([
        left, top, uv[0][0], uv[0][1], right, top, uv[1][0], uv[1][1], left, bottom, uv[2][0],
        uv[2][1], right, bottom, uv[3][0], uv[3][1],
    ])
}

unsafe fn bind_image_texture(extensions: &Extensions, texture: GlUint, image: EglImage) {
    unsafe {
        glBindTexture(GL_TEXTURE_2D, texture);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
        (extensions.image_target_texture)(GL_TEXTURE_2D, image);
    }
}

unsafe fn create_program() -> Result<GlUint, GlesError> {
    let vertex = unsafe { compile_shader(GL_VERTEX_SHADER, VERTEX_SHADER)? };
    let fragment = unsafe { compile_shader(GL_FRAGMENT_SHADER, FRAGMENT_SHADER)? };
    let program = unsafe { glCreateProgram() };
    unsafe {
        glAttachShader(program, vertex);
        glAttachShader(program, fragment);
        glLinkProgram(program);
        glDeleteShader(vertex);
        glDeleteShader(fragment);
    }
    let mut status = 0;
    unsafe { glGetProgramiv(program, GL_LINK_STATUS, &mut status) };
    if status == 0 {
        unsafe { glDeleteProgram(program) };
        Err(GlesError("link GLES layer program"))
    } else {
        Ok(program)
    }
}

unsafe fn compile_shader(kind: GlEnum, source: &'static [u8]) -> Result<GlUint, GlesError> {
    let shader = unsafe { glCreateShader(kind) };
    let source = source.as_ptr().cast::<c_char>();
    unsafe {
        glShaderSource(shader, 1, &source, ptr::null());
        glCompileShader(shader);
    }
    let mut status = 0;
    unsafe { glGetShaderiv(shader, GL_COMPILE_STATUS, &mut status) };
    if status == 0 {
        unsafe { glDeleteShader(shader) };
        Err(GlesError("compile GLES layer shader"))
    } else {
        Ok(shader)
    }
}

fn uniform_or_attribute(
    program: GlUint,
    name: &'static [u8],
    attribute: bool,
) -> Result<GlInt, GlesError> {
    let location = unsafe {
        if attribute {
            glGetAttribLocation(program, name.as_ptr().cast())
        } else {
            glGetUniformLocation(program, name.as_ptr().cast())
        }
    };
    if location < 0 {
        Err(GlesError("resolve GLES shader location"))
    } else {
        Ok(location)
    }
}

unsafe extern "C" {
    fn eglGetDisplay(native_display: *mut c_void) -> EglDisplay;
    fn eglInitialize(display: EglDisplay, major: *mut EglInt, minor: *mut EglInt) -> EglBoolean;
    fn eglTerminate(display: EglDisplay) -> EglBoolean;
    fn eglBindAPI(api: EglInt) -> EglBoolean;
    fn eglChooseConfig(
        display: EglDisplay,
        attributes: *const EglInt,
        configs: *mut EglConfig,
        config_size: EglInt,
        count: *mut EglInt,
    ) -> EglBoolean;
    fn eglCreateContext(
        display: EglDisplay,
        config: EglConfig,
        share: EglContext,
        attributes: *const EglInt,
    ) -> EglContext;
    fn eglDestroyContext(display: EglDisplay, context: EglContext) -> EglBoolean;
    fn eglMakeCurrent(
        display: EglDisplay,
        draw: *mut c_void,
        read: *mut c_void,
        context: EglContext,
    ) -> EglBoolean;
    fn eglGetProcAddress(name: *const c_char) -> *const c_void;

    fn glCreateShader(kind: GlEnum) -> GlUint;
    fn glShaderSource(
        shader: GlUint,
        count: GlSize,
        strings: *const *const c_char,
        lengths: *const GlInt,
    );
    fn glCompileShader(shader: GlUint);
    fn glGetShaderiv(shader: GlUint, parameter: GlEnum, value: *mut GlInt);
    fn glDeleteShader(shader: GlUint);
    fn glCreateProgram() -> GlUint;
    fn glAttachShader(program: GlUint, shader: GlUint);
    fn glLinkProgram(program: GlUint);
    fn glGetProgramiv(program: GlUint, parameter: GlEnum, value: *mut GlInt);
    fn glDeleteProgram(program: GlUint);
    fn glUseProgram(program: GlUint);
    fn glGetAttribLocation(program: GlUint, name: *const c_char) -> GlInt;
    fn glGetUniformLocation(program: GlUint, name: *const c_char) -> GlInt;
    fn glGenBuffers(count: GlSize, buffers: *mut GlUint);
    fn glDeleteBuffers(count: GlSize, buffers: *const GlUint);
    fn glBindBuffer(target: GlEnum, buffer: GlUint);
    fn glBufferData(target: GlEnum, size: isize, data: *const c_void, usage: GlEnum);
    fn glEnableVertexAttribArray(index: GlUint);
    fn glDisableVertexAttribArray(index: GlUint);
    fn glVertexAttribPointer(
        index: GlUint,
        size: GlInt,
        kind: GlEnum,
        normalized: u8,
        stride: GlSize,
        pointer: *const c_void,
    );
    fn glUniform1i(location: GlInt, value: GlInt);
    fn glUniform1f(location: GlInt, value: f32);
    fn glUniform4f(location: GlInt, red: f32, green: f32, blue: f32, alpha: f32);
    fn glGenTextures(count: GlSize, textures: *mut GlUint);
    fn glDeleteTextures(count: GlSize, textures: *const GlUint);
    fn glBindTexture(target: GlEnum, texture: GlUint);
    fn glTexParameteri(target: GlEnum, parameter: GlEnum, value: GlInt);
    fn glActiveTexture(texture: GlEnum);
    fn glGenFramebuffers(count: GlSize, framebuffers: *mut GlUint);
    fn glDeleteFramebuffers(count: GlSize, framebuffers: *const GlUint);
    fn glBindFramebuffer(target: GlEnum, framebuffer: GlUint);
    fn glFramebufferTexture2D(
        target: GlEnum,
        attachment: GlEnum,
        texture_target: GlEnum,
        texture: GlUint,
        level: GlInt,
    );
    fn glCheckFramebufferStatus(target: GlEnum) -> GlEnum;
    fn glViewport(x: GlInt, y: GlInt, width: GlSize, height: GlSize);
    fn glClearColor(red: f32, green: f32, blue: f32, alpha: f32);
    fn glClear(mask: GlEnum);
    fn glEnable(capability: GlEnum);
    fn glDisable(capability: GlEnum);
    fn glBlendFunc(source: GlEnum, destination: GlEnum);
    fn glDrawArrays(mode: GlEnum, first: GlInt, count: GlSize);
    fn glFlush();
}
