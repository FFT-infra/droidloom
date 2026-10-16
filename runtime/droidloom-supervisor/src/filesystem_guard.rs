//! Inherited protection for filesystems shared with the host.
//!
//! This is a narrow filesystem policy, not a complete Android sandbox. Install
//! it while the namespace leader still has CAP_SYS_ADMIN; do not set NNP on
//! Android, whose services use executable capabilities. Children cannot remove
//! this filter. Both native and compat syscall ABIs are covered.

use std::io;

const LD: u16 = 0x20;
const JEQ: u16 = 0x15;
const JSET: u16 = 0x45;
const AND: u16 = 0x54;
const RET: u16 = 0x06;
const ALLOW: u32 = 0x7fff_0000;
const DENY: u32 = 0x0005_0000 | libc::EPERM as u32;
const ENOSYS: u32 = 0x0005_0000 | libc::ENOSYS as u32;

fn ins(code: u16, jt: u8, jf: u8, k: u32) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
}

fn abi_policy(ioctl: u32, mount: u32, quota: u32) -> Vec<libc::sock_filter> {
    let mut code = vec![ins(LD, 0, 0, 0), ins(AND, 0, 0, 0x3fff_ffff)];
    for nr in [quota, 443] {
        // quotactl_fd uses the common new syscall numbering.
        code.extend([ins(JEQ, 0, 1, nr), ins(RET, 0, 0, DENY)]);
    }
    // fsconfig(FSCONFIG_CMD_RECONFIGURE) can change a shared superblock.
    code.extend([
        ins(JEQ, 0, 4, 431),
        ins(LD, 0, 0, 24),
        ins(JEQ, 0, 1, 7),
        ins(RET, 0, 0, DENY),
        ins(RET, 0, 0, ALLOW),
    ]);
    // A bind remount changes mount flags only; ordinary remount changes the
    // superblock. Android may still mount its private tmpfs/FUSE/APEX trees.
    code.extend([
        ins(JEQ, 0, 5, mount),
        ins(LD, 0, 0, 40),
        ins(JSET, 0, 2, libc::MS_REMOUNT as u32),
        ins(JSET, 1, 0, libc::MS_BIND as u32),
        ins(RET, 0, 0, DENY),
        ins(RET, 0, 0, ALLOW),
        ins(JEQ, 1, 0, ioctl),
        ins(RET, 0, 0, ALLOW),
        ins(LD, 0, 0, 24),
        ins(AND, 0, 0, 0xffff),
    ]);
    // Match type/number independently of argument size (native/compat) and
    // direction. Keep per-file fscrypt, flags, FIEMAP, FICLONE and DRM intact.
    for request in [
        0x5877, 0x5878, 0x5879, 0x587d, // freeze, thaw, trim, shutdown
        0x6607, 0x6608, 0x6610, 0x6611, // ext4 resize/group/boot inode swap
        0x662b, 0x662c, 0x662e, // checkpoint, UUID, superblock tuning
        0x9432, // filesystem label
        0xf506, 0xf50a, 0xf50b, 0xf510, // f2fs GC/flush/resize
    ] {
        code.extend([ins(JEQ, 0, 1, request), ins(RET, 0, 0, DENY)]);
    }
    code.push(ins(RET, 0, 0, ALLOW));
    code
}

fn program() -> Vec<libc::sock_filter> {
    #[cfg(target_arch = "x86_64")]
    let (native_arch, compat_arch, native) = (0xc000_003e, 0x4000_0003, abi_policy(16, 165, 179));
    #[cfg(target_arch = "aarch64")]
    let (native_arch, compat_arch, native) = (0xc000_00b7, 0x4000_0028, abi_policy(29, 40, 60));
    let compat = abi_policy(54, 21, 131);
    let mut code = vec![
        ins(LD, 0, 0, 4),
        ins(JEQ, 0, native.len() as u8, native_arch),
    ];
    code.extend(native);
    code.push(ins(JEQ, 0, compat.len() as u8, compat_arch));
    code.extend(compat);
    code.push(ins(RET, 0, 0, ENOSYS));
    code
}

pub(crate) fn install() -> io::Result<()> {
    let code = program();
    let filter = libc::sock_fprog {
        len: code.len() as u16,
        filter: code.as_ptr().cast_mut(),
    };
    // SAFETY: the kernel copies the bounded filter synchronously. This runs in
    // the single-threaded namespace child, before exec of Android init.
    if unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &filter) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    fn evaluate(code: &[libc::sock_filter], nr: u32, args: [u64; 6]) -> u32 {
        let mut words = vec![nr, 0, 0, 0];
        for value in args {
            words.extend([value as u32, (value >> 32) as u32]);
        }
        let (mut pc, mut a) = (0, 0);
        loop {
            let op = code[pc];
            pc += 1;
            match op.code {
                LD => a = words[op.k as usize / 4],
                AND => a &= op.k,
                JEQ => pc += if a == op.k { op.jt } else { op.jf } as usize,
                JSET => pc += if a & op.k != 0 { op.jt } else { op.jf } as usize,
                RET => return op.k,
                _ => panic!("unexpected BPF instruction"),
            }
        }
    }

    #[test]
    fn native_and_compat_filesystem_operations() {
        for (ioctl, mount, quota) in [(16, 165, 179), (29, 40, 60), (54, 21, 131)] {
            let code = abi_policy(ioctl, mount, quota);
            for request in [0x8004587du64, 0xc0045877, 0xc0185879, 0x40086607] {
                assert_eq!(evaluate(&code, ioctl, [0, request, 0, 0, 0, 0]), DENY);
            }
            for request in [0x541b, 0xc0106440, 0xc020660b, 0x400c6613] {
                assert_eq!(evaluate(&code, ioctl, [0, request, 0, 0, 0, 0]), ALLOW);
            }
            assert_eq!(evaluate(&code, quota, [0; 6]), DENY);
            assert_eq!(evaluate(&code, 443, [0; 6]), DENY);
            assert_eq!(evaluate(&code, 431, [0, 7, 0, 0, 0, 0]), DENY);
            assert_eq!(evaluate(&code, 431, [0, 6, 0, 0, 0, 0]), ALLOW);
            assert_eq!(evaluate(&code, mount, [0, 0, 0, 32, 0, 0]), DENY);
            assert_eq!(evaluate(&code, mount, [0, 0, 0, 32 | 4096, 0, 0]), ALLOW);
            assert_eq!(evaluate(&code, mount, [0; 6]), ALLOW);
        }
    }

    #[test]
    fn kernel_filter_is_inherited_and_keeps_file_io() {
        const CHILD: &str = "DROIDLOOM_TEST_FILESYSTEM_FILTER";
        if std::env::var_os(CHILD).is_some() {
            // Only the test child uses NNP; production installs while privileged.
            assert_eq!(
                unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
                0
            );
            install().unwrap();
            let file = tempfile::tempfile().unwrap();
            // Use an invalid FD: even a broken filter cannot touch a filesystem.
            for request in [0x8004587du64, 0xc0045877, 0xc0185879] {
                assert_eq!(unsafe { libc::ioctl(-1, request as libc::c_ulong, 0) }, -1);
                assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
            }
            assert_eq!(unsafe { libc::ioctl(-1, 0x541b, 0) }, -1);
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
            assert_eq!(unsafe { libc::ftruncate(file.as_raw_fd(), 123) }, 0);
            assert!(
                std::process::Command::new("true")
                    .status()
                    .unwrap()
                    .success()
            );
            return;
        }
        assert!(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "filesystem_guard::tests::kernel_filter_is_inherited_and_keeps_file_io"
                ])
                .env(CHILD, "1")
                .status()
                .unwrap()
                .success()
        );
    }
}
