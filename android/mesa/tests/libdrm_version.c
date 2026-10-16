/* Exercise the actual libdrm API against kernel version replies with empty fields. */
#include <assert.h>
#include <stdarg.h>
#include <stddef.h>
#include <string.h>
#include <xf86drm.h>

static const char *driver_name;
static const char *driver_date;
static const char *driver_description;
static unsigned int queries;

static void version_field(char *destination, size_t *length, const char *source)
{
    size_t size = strlen(source);
    if (destination != NULL) {
        size_t available = *length < size ? *length : size;
        memcpy(destination, source, available);
    }
    *length = size;
}

int __wrap_ioctl(int fd, unsigned long request, ...)
{
    assert(fd == 123);
    assert(request == DRM_IOCTL_VERSION);
    va_list arguments;
    va_start(arguments, request);
    drm_version_t *version = va_arg(arguments, drm_version_t *);
    va_end(arguments);
    version->version_major = 1;
    version->version_minor = 9;
    version->version_patchlevel = 0;
    version_field(version->name, &version->name_len, driver_name);
    version_field(version->date, &version->date_len, driver_date);
    version_field(version->desc, &version->desc_len, driver_description);
    ++queries;
    return 0;
}

static void check_version(const char *name, const char *date, const char *description)
{
    driver_name = name;
    driver_date = date;
    driver_description = description;
    queries = 0;
    drmVersionPtr version = drmGetVersion(123);
    assert(version != NULL);
    assert(queries == 2);
    assert(version->version_major == 1);
    assert(version->version_minor == 9);
    assert(version->version_patchlevel == 0);
    assert(version->name != NULL && strcmp(version->name, name) == 0);
    assert(version->date != NULL && strcmp(version->date, date) == 0);
    assert(version->desc != NULL && strcmp(version->desc, description) == 0);
    assert(version->name_len == (int)strlen(name));
    assert(version->date_len == (int)strlen(date));
    assert(version->desc_len == (int)strlen(description));
    drmFreeVersion(version);
}

int main(void)
{
    check_version("panthor", "", "Panthor DRM driver");
    check_version("test", "20261011", "driver description");
    check_version("test", "20261011", "");
    check_version("", "", "");
    return 0;
}
