/*
 * Development-only compatibility interposer for a shared Linux kernel on
 * which no Android SELinux policy is loaded. The real libselinux remains
 * linked and supplies label-file parsing; only kernel-policy operations are
 * made permissive. Production confinement belongs to the host sandbox.
 */

#include <selinux/android.h>
#include <selinux/selinux.h>

#include <fcntl.h>
#include <stdbool.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

static const char kProcessContext[] = "u:r:droidloom_unconfined:s0";
static const char kServiceContext[] = "u:r:droidloom_service:s0";
static const char kFileContext[] = "u:object_r:system_file:s0";
static const char kCellMarker[] = "DROIDLOOM_CELL";
static const char kProcSelfFdPrefix[] = "/proc/self/fd/";
static const char kSyntheticRootPrefix[] = "/newroot";

/*
 * Aston's shared-kernel procfs exposes files opened below the cell root with
 * the synthetic /newroot prefix. Android's zygote deliberately validates and
 * later reopens inherited files by their canonical Android paths, so preserve
 * that security check by translating only this known namespace alias.
 *
 * Call readlinkat through the kernel rather than resolving the next readlink
 * symbol: this interposer is loaded before libc and must not recurse.
 */
ssize_t readlink(const char *path, char *buffer, size_t buffer_size) {
  ssize_t result = syscall(__NR_readlinkat, AT_FDCWD, path, buffer, buffer_size);
  if (result <= 0 || getenv(kCellMarker) == NULL ||
      strncmp(path, kProcSelfFdPrefix, sizeof(kProcSelfFdPrefix) - 1) != 0 ||
      (size_t)result <= sizeof(kSyntheticRootPrefix) - 1 ||
      memcmp(buffer, "/newroot/", sizeof(kSyntheticRootPrefix)) != 0) {
    return result;
  }

  const size_t prefix_length = sizeof(kSyntheticRootPrefix) - 1;
  memmove(buffer, buffer + prefix_length, (size_t)result - prefix_length);
  return result - (ssize_t)prefix_length;
}

static int copy_context(char **destination, const char *context,
                        bool return_length) {
  if (destination == NULL) {
    return -1;
  }
  *destination = strdup(context);
  if (*destination == NULL) {
    return -1;
  }
  return return_length ? (int)strlen(context) : 0;
}

int is_selinux_enabled(void) { return 0; }

int security_getenforce(void) { return 0; }

int security_setenforce(int value __attribute__((unused))) { return 0; }

int getcon(char **context) {
  return copy_context(context, kProcessContext, false);
}

int getprevcon(char **context) {
  return copy_context(context, kProcessContext, false);
}

int getpidcon(pid_t pid __attribute__((unused)), char **context) {
  return copy_context(context, kProcessContext, false);
}

int getpeercon(int fd __attribute__((unused)), char **context) {
  return copy_context(context, kProcessContext, false);
}

int getfilecon(const char *path __attribute__((unused)), char **context) {
  return copy_context(context, kFileContext, true);
}

int lgetfilecon(const char *path __attribute__((unused)), char **context) {
  return copy_context(context, kFileContext, true);
}

int fgetfilecon(int fd __attribute__((unused)), char **context) {
  return copy_context(context, kFileContext, true);
}

int security_compute_create(const char *source __attribute__((unused)),
                            const char *target __attribute__((unused)),
                            security_class_t object_class
                            __attribute__((unused)),
                            char **context) {
  // Android init rejects services when the computed context is identical to
  // its own even in permissive mode. The shared host kernel has no Android
  // policy to compute a transition, so return a distinct development-only
  // context while setexeccon remains a no-op. Host confinement is unchanged.
  return copy_context(context, kServiceContext, false);
}

int selinux_check_access(const char *source __attribute__((unused)),
                         const char *target __attribute__((unused)),
                         const char *object_class __attribute__((unused)),
                         const char *permission __attribute__((unused)),
                         void *audit_data __attribute__((unused))) {
  return 0;
}

#define DROIDLOOM_NOOP_CONTEXT(function_name)                                  \
  int function_name(const char *context __attribute__((unused))) { return 0; }

DROIDLOOM_NOOP_CONTEXT(setcon)
DROIDLOOM_NOOP_CONTEXT(setexeccon)
DROIDLOOM_NOOP_CONTEXT(setfscreatecon)
DROIDLOOM_NOOP_CONTEXT(setsockcreatecon)
DROIDLOOM_NOOP_CONTEXT(selinux_android_setcon)

#define DROIDLOOM_NOOP_PATH_CONTEXT(function_name)                             \
  int function_name(const char *path __attribute__((unused)),                  \
                    const char *context __attribute__((unused))) {             \
    return 0;                                                                  \
  }

DROIDLOOM_NOOP_PATH_CONTEXT(setfilecon)
DROIDLOOM_NOOP_PATH_CONTEXT(lsetfilecon)

int fsetfilecon(int fd __attribute__((unused)),
                const char *context __attribute__((unused))) {
  return 0;
}

int selinux_android_restorecon(const char *path __attribute__((unused)),
                               unsigned int flags __attribute__((unused))) {
  return 0;
}

int selinux_android_restorecon_pkgdir(const char *path __attribute__((unused)),
                                      const char *seinfo
                                      __attribute__((unused)),
                                      uid_t uid __attribute__((unused)),
                                      unsigned int flags
                                      __attribute__((unused))) {
  return 0;
}

void selinux_android_seapp_context_init(void) {}

int selinux_android_setcontext(uid_t uid __attribute__((unused)),
                               bool system_server __attribute__((unused)),
                               const char *seinfo __attribute__((unused)),
                               const char *name __attribute__((unused))) {
  return 0;
}

int selinux_status_open(int fallback __attribute__((unused))) { return 0; }

int selinux_status_updated(void) { return 0; }
