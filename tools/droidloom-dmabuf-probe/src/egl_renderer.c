#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <libdrm/drm.h>

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>

struct droidloom_egl_renderer {
    EGLDisplay display;
    EGLContext context;
    EGLSurface surface;
    PFNEGLCREATEIMAGEKHRPROC create_image;
    PFNEGLDESTROYIMAGEKHRPROC destroy_image;
    PFNGLEGLIMAGETARGETTEXTURE2DOESPROC image_target_texture;
    PFNEGLCREATESYNCKHRPROC create_sync;
    PFNEGLDESTROYSYNCKHRPROC destroy_sync;
    PFNEGLDUPNATIVEFENCEFDANDROIDPROC duplicate_fence_fd;
    int has_modifier_import;
    char renderer[256];
};

static int fail(char *error, size_t error_size, const char *message) {
    if (error != NULL && error_size > 0) {
        (void)snprintf(error, error_size, "%s (egl=0x%04x gl=0x%04x)",
                       message, (unsigned int)eglGetError(), (unsigned int)glGetError());
    }
    return -1;
}

static int has_extension(const char *extensions, const char *needle) {
    if (extensions == NULL || needle == NULL || strchr(needle, ' ') != NULL) {
        return 0;
    }
    const size_t length = strlen(needle);
    const char *cursor = extensions;
    while ((cursor = strstr(cursor, needle)) != NULL) {
        const char before = cursor == extensions ? ' ' : cursor[-1];
        const char after = cursor[length];
        if ((before == ' ' || before == '\0') && (after == ' ' || after == '\0')) {
            return 1;
        }
        cursor += length;
    }
    return 0;
}

struct droidloom_egl_renderer *droidloom_egl_renderer_create(
    void *gbm_device, char *error, size_t error_size) {
    PFNEGLGETPLATFORMDISPLAYEXTPROC get_platform_display =
        (PFNEGLGETPLATFORMDISPLAYEXTPROC)eglGetProcAddress("eglGetPlatformDisplayEXT");
    if (get_platform_display == NULL) {
        fail(error, error_size, "EGL_EXT_platform_base is unavailable");
        return NULL;
    }

    struct droidloom_egl_renderer *renderer = calloc(1, sizeof(*renderer));
    if (renderer == NULL) {
        fail(error, error_size, "cannot allocate EGL renderer state");
        return NULL;
    }
    renderer->display = get_platform_display(EGL_PLATFORM_GBM_KHR, gbm_device, NULL);
    renderer->context = EGL_NO_CONTEXT;
    renderer->surface = EGL_NO_SURFACE;
    if (renderer->display == EGL_NO_DISPLAY || !eglInitialize(renderer->display, NULL, NULL)) {
        fail(error, error_size, "cannot initialize EGL on the render node");
        free(renderer);
        return NULL;
    }
    if (!eglBindAPI(EGL_OPENGL_ES_API)) {
        fail(error, error_size, "cannot bind the OpenGL ES API");
        eglTerminate(renderer->display);
        free(renderer);
        return NULL;
    }

    const EGLint config_attributes[] = {
        EGL_RENDERABLE_TYPE, EGL_OPENGL_ES2_BIT,
        EGL_RED_SIZE, 8,
        EGL_GREEN_SIZE, 8,
        EGL_BLUE_SIZE, 8,
        EGL_ALPHA_SIZE, 0,
        EGL_NONE,
    };
    EGLConfig config = NULL;
    EGLint config_count = 0;
    if (!eglChooseConfig(renderer->display, config_attributes, &config, 1, &config_count) ||
        config_count < 1) {
        fail(error, error_size, "cannot choose an EGL GBM config");
        eglTerminate(renderer->display);
        free(renderer);
        return NULL;
    }
    const EGLint context_attributes[] = {EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE};
    renderer->context = eglCreateContext(renderer->display, config, EGL_NO_CONTEXT,
                                         context_attributes);
    if (renderer->context == EGL_NO_CONTEXT ||
        !eglMakeCurrent(renderer->display, EGL_NO_SURFACE, EGL_NO_SURFACE,
                        renderer->context)) {
        fail(error, error_size, "cannot create the GLES context");
        if (renderer->context != EGL_NO_CONTEXT) {
            eglDestroyContext(renderer->display, renderer->context);
        }
        eglTerminate(renderer->display);
        free(renderer);
        return NULL;
    }

    renderer->create_image =
        (PFNEGLCREATEIMAGEKHRPROC)eglGetProcAddress("eglCreateImageKHR");
    renderer->destroy_image =
        (PFNEGLDESTROYIMAGEKHRPROC)eglGetProcAddress("eglDestroyImageKHR");
    renderer->image_target_texture =
        (PFNGLEGLIMAGETARGETTEXTURE2DOESPROC)eglGetProcAddress(
            "glEGLImageTargetTexture2DOES");
    renderer->create_sync =
        (PFNEGLCREATESYNCKHRPROC)eglGetProcAddress("eglCreateSyncKHR");
    renderer->destroy_sync =
        (PFNEGLDESTROYSYNCKHRPROC)eglGetProcAddress("eglDestroySyncKHR");
    renderer->duplicate_fence_fd =
        (PFNEGLDUPNATIVEFENCEFDANDROIDPROC)eglGetProcAddress(
            "eglDupNativeFenceFDANDROID");
    const char *extensions = eglQueryString(renderer->display, EGL_EXTENSIONS);
    renderer->has_modifier_import =
        has_extension(extensions, "EGL_EXT_image_dma_buf_import_modifiers");
    if (renderer->create_image == NULL || renderer->destroy_image == NULL ||
        renderer->image_target_texture == NULL || renderer->create_sync == NULL ||
        renderer->destroy_sync == NULL || renderer->duplicate_fence_fd == NULL ||
        !has_extension(extensions, "EGL_EXT_image_dma_buf_import") ||
        !has_extension(extensions, "EGL_ANDROID_native_fence_sync")) {
        fail(error, error_size, "required DMA-BUF/native-fence EGL extensions are unavailable");
        eglMakeCurrent(renderer->display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
        eglDestroyContext(renderer->display, renderer->context);
        eglTerminate(renderer->display);
        free(renderer);
        return NULL;
    }

    const GLubyte *name = glGetString(GL_RENDERER);
    (void)snprintf(renderer->renderer, sizeof(renderer->renderer), "%s",
                   name == NULL ? "unknown" : (const char *)name);
    return renderer;
}

const char *droidloom_egl_renderer_name(const struct droidloom_egl_renderer *renderer) {
    return renderer == NULL ? "unknown" : renderer->renderer;
}

int droidloom_egl_render_dmabuf(struct droidloom_egl_renderer *renderer,
                                int width, int height, uint32_t fourcc,
                                uint64_t modifier, int dma_buf_fd,
                                uint32_t offset, uint32_t stride,
                                int *fence_fd, char *error, size_t error_size) {
    if (renderer == NULL || width <= 0 || height <= 0 || dma_buf_fd < 0 ||
        stride == 0 || fence_fd == NULL) {
        return fail(error, error_size, "invalid GLES DMA-BUF render arguments");
    }
    if (!eglMakeCurrent(renderer->display, EGL_NO_SURFACE, EGL_NO_SURFACE,
                        renderer->context)) {
        return fail(error, error_size, "cannot make the GLES context current");
    }

    EGLint attributes[32];
    size_t index = 0;
#define ADD_ATTRIBUTE(name, value) \
    do {                             \
        attributes[index++] = name;  \
        attributes[index++] = value; \
    } while (0)
    ADD_ATTRIBUTE(EGL_WIDTH, width);
    ADD_ATTRIBUTE(EGL_HEIGHT, height);
    ADD_ATTRIBUTE(EGL_LINUX_DRM_FOURCC_EXT, (EGLint)fourcc);
    ADD_ATTRIBUTE(EGL_DMA_BUF_PLANE0_FD_EXT, dma_buf_fd);
    ADD_ATTRIBUTE(EGL_DMA_BUF_PLANE0_OFFSET_EXT, (EGLint)offset);
    ADD_ATTRIBUTE(EGL_DMA_BUF_PLANE0_PITCH_EXT, (EGLint)stride);
    if (renderer->has_modifier_import) {
        ADD_ATTRIBUTE(EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT, (EGLint)(modifier & 0xffffffffu));
        ADD_ATTRIBUTE(EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT, (EGLint)(modifier >> 32));
    } else if (modifier != 0) {
        return fail(error, error_size, "EGL modifier import is unavailable for a non-linear BO");
    }
    attributes[index++] = EGL_NONE;
#undef ADD_ATTRIBUTE

    EGLImageKHR image = renderer->create_image(
        renderer->display, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, NULL, attributes);
    if (image == EGL_NO_IMAGE_KHR) {
        return fail(error, error_size, "cannot import GBM DMA-BUF into EGL");
    }

    GLuint texture = 0;
    GLuint framebuffer = 0;
    glGenTextures(1, &texture);
    glBindTexture(GL_TEXTURE_2D, texture);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_NEAREST);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_NEAREST);
    renderer->image_target_texture(GL_TEXTURE_2D, image);
    glGenFramebuffers(1, &framebuffer);
    glBindFramebuffer(GL_FRAMEBUFFER, framebuffer);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texture, 0);
    if (glCheckFramebufferStatus(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE) {
        glDeleteFramebuffers(1, &framebuffer);
        glDeleteTextures(1, &texture);
        renderer->destroy_image(renderer->display, image);
        return fail(error, error_size, "DMA-BUF GLES framebuffer is incomplete");
    }

    glViewport(0, 0, width, height);
    glDisable(GL_SCISSOR_TEST);
    glClearColor(0.02f, 0.35f, 0.48f, 1.0f);
    glClear(GL_COLOR_BUFFER_BIT);

    EGLSyncKHR sync = renderer->create_sync(renderer->display,
                                            EGL_SYNC_NATIVE_FENCE_ANDROID, NULL);
    if (sync == EGL_NO_SYNC_KHR) {
        glDeleteFramebuffers(1, &framebuffer);
        glDeleteTextures(1, &texture);
        renderer->destroy_image(renderer->display, image);
        return fail(error, error_size, "cannot create the GLES native fence");
    }
    glFlush();
    *fence_fd = renderer->duplicate_fence_fd(renderer->display, sync);
    renderer->destroy_sync(renderer->display, sync);
    glDeleteFramebuffers(1, &framebuffer);
    glDeleteTextures(1, &texture);
    renderer->destroy_image(renderer->display, image);
    if (*fence_fd < 0) {
        return fail(error, error_size, "cannot export the GLES native fence");
    }
    return 0;
}

void droidloom_egl_renderer_destroy(struct droidloom_egl_renderer *renderer) {
    if (renderer == NULL) {
        return;
    }
    eglMakeCurrent(renderer->display, EGL_NO_SURFACE, EGL_NO_SURFACE, EGL_NO_CONTEXT);
    if (renderer->surface != EGL_NO_SURFACE) {
        eglDestroySurface(renderer->display, renderer->surface);
    }
    eglDestroyContext(renderer->display, renderer->context);
    eglTerminate(renderer->display);
    free(renderer);
}

int droidloom_import_sync_file(int drm_fd, uint32_t syncobj_handle,
                               int sync_file_fd, char *error, size_t error_size) {
    struct drm_syncobj_handle arguments = {
        .handle = syncobj_handle,
        .flags = DRM_SYNCOBJ_FD_TO_HANDLE_FLAGS_IMPORT_SYNC_FILE,
        .fd = sync_file_fd,
    };
    if (ioctl(drm_fd, DRM_IOCTL_SYNCOBJ_FD_TO_HANDLE, &arguments) == 0) {
        return 0;
    }
    if (error != NULL && error_size > 0) {
        (void)snprintf(error, error_size, "DRM sync-file import: %s", strerror(errno));
    }
    return -1;
}
