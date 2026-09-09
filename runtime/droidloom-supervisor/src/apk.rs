//! APK descriptor transport and bounded package-manager invocation.

use std::fs::File;
use std::io::{self, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};

use super::{ControlError, diagnostics, io_error};

const MAX_APK_BYTES: u64 = 2 * 1024 * 1024 * 1024;

pub(super) fn validate(file: &File) -> Result<u64, ControlError> {
    let metadata = file.metadata().map_err(|e| io_error("inspect APK", e))?;
    if !metadata.is_file() || !(4..=MAX_APK_BYTES).contains(&metadata.len()) {
        return Err(ControlError::Invalid(
            "APK must be a nonempty regular file no larger than 2 GiB".into(),
        ));
    }
    let mut magic = [0; 4];
    file.read_exact_at(&mut magic, 0)
        .map_err(|e| io_error("read APK header", e))?;
    if magic != *b"PK\x03\x04" {
        return Err(ControlError::Invalid(
            "file is not an APK (expected a ZIP archive); supply a standalone .apk".into(),
        ));
    }
    Ok(metadata.len())
}

pub(super) fn install(pid: u32, user: u32, mut file: File) -> Result<(), ControlError> {
    let bytes = validate(&file)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|e| io_error("rewind APK", e))?;
    let mut command = install_command(pid, user, bytes);
    let output = diagnostics::capture_with_stdin(&mut command, Stdio::from(file))
        .map_err(|error| ControlError::Invalid(format!("APK installation failed: {error}")))?;
    check_result(&output)
}

fn install_command(pid: u32, user: u32, bytes: u64) -> Command {
    let mut command = droidloom_cpu_placement::command("/usr/bin/timeout");
    command
        .args(["--kill-after=5s", "110s", "/usr/bin/nsenter", "--target"])
        .arg(pid.to_string())
        .args([
            "--mount",
            "--uts",
            "--ipc",
            "--net",
            "--pid",
            "--cgroup",
            "--root",
            "--wd",
            "--env",
            "--",
            "/system/bin/cmd",
            "package",
            "install",
            "-r",
            "--user",
        ])
        .arg(user.to_string())
        .arg("-S")
        .arg(bytes.to_string());
    // With -S and no path, PackageManagerShellCommand reads stdin. A bare
    // trailing '-' is consumed by its option parser and rejected before install.
    command
}

fn check_result(output: &str) -> Result<(), ControlError> {
    if output.lines().any(|line| line.trim() == "Success") && !output.contains("Failure [") {
        Ok(())
    } else {
        Err(ControlError::Invalid(format!(
            "Android did not confirm APK installation: {}",
            output.trim()
        )))
    }
}

// A cmsghdr-aligned buffer large enough to receive several descriptors, so
// malformed clients' extra descriptors can be owned and closed on rejection.
#[repr(C)]
struct Ancillary {
    header: libc::cmsghdr,
    extra: [usize; 32],
}

pub(super) fn send_file(stream: &UnixStream, first: u8, file: &File) -> Result<(), ControlError> {
    // SAFETY: zero is a valid initial representation for these C structs.
    let mut control: Ancillary = unsafe { std::mem::zeroed() };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    let mut byte = first;
    let mut vector = libc::iovec {
        iov_base: std::ptr::from_mut(&mut byte).cast(),
        iov_len: 1,
    };
    message.msg_iov = &raw mut vector;
    message.msg_iovlen = 1;
    message.msg_control = std::ptr::from_mut(&mut control).cast();
    // SAFETY: the buffer is aligned and larger than CMSG_SPACE for one fd;
    // all referenced objects remain alive for the synchronous sendmsg call.
    let result = unsafe {
        message.msg_controllen = libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) as usize;
        let header = libc::CMSG_FIRSTHDR(&message);
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as usize;
        libc::CMSG_DATA(header)
            .cast::<libc::c_int>()
            .write_unaligned(file.as_raw_fd());
        libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL)
    };
    if result != 1 {
        return Err(io_error("send APK descriptor", io::Error::last_os_error()));
    }
    Ok(())
}

pub(super) fn receive_file(stream: &UnixStream) -> Result<(u8, Option<File>), ControlError> {
    // SAFETY: zero is a valid initial representation for these C structs.
    let mut control: Ancillary = unsafe { std::mem::zeroed() };
    let mut message: libc::msghdr = unsafe { std::mem::zeroed() };
    let mut byte = 0_u8;
    let mut vector = libc::iovec {
        iov_base: std::ptr::from_mut(&mut byte).cast(),
        iov_len: 1,
    };
    message.msg_iov = &raw mut vector;
    message.msg_iovlen = 1;
    message.msg_control = std::ptr::from_mut(&mut control).cast();
    message.msg_controllen = size_of::<Ancillary>();
    // SAFETY: the iovec and aligned control storage are writable and remain
    // alive throughout the call. CLOEXEC prevents leaking received fds to children.
    let result =
        unsafe { libc::recvmsg(stream.as_raw_fd(), &raw mut message, libc::MSG_CMSG_CLOEXEC) };
    if result < 0 {
        return Err(io_error(
            "receive lifecycle descriptor",
            io::Error::last_os_error(),
        ));
    }
    let mut files = Vec::new();
    // SAFETY: recvmsg supplies valid kernel-checked cmsghdr records within our
    // aligned buffer. Each SCM_RIGHTS fd is newly owned and wrapped exactly once,
    // including extras that must be closed if the request is rejected.
    unsafe {
        let mut header = libc::CMSG_FIRSTHDR(&message);
        while !header.is_null() {
            if (*header).cmsg_level == libc::SOL_SOCKET && (*header).cmsg_type == libc::SCM_RIGHTS {
                let count =
                    ((*header).cmsg_len - libc::CMSG_LEN(0) as usize) / size_of::<libc::c_int>();
                let data = libc::CMSG_DATA(header).cast::<libc::c_int>();
                for index in 0..count {
                    files.push(File::from_raw_fd(data.add(index).read_unaligned()));
                }
            }
            header = libc::CMSG_NXTHDR(&message, header);
        }
    }
    if result != 1 || message.msg_flags & libc::MSG_CTRUNC != 0 || files.len() > 1 {
        return Err(ControlError::Invalid(
            "expected a lifecycle message with at most one APK descriptor".into(),
        ));
    }
    Ok((byte, files.pop()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn stdin_install_ends_with_size_not_a_bare_dash_option() {
        let command = install_command(1234, 10, 4567);
        let args: Vec<_> = command.get_args().map(|s| s.to_str().unwrap()).collect();
        let android = args
            .iter()
            .position(|arg| *arg == "/system/bin/cmd")
            .unwrap();
        assert_eq!(
            &args[android..],
            &[
                "/system/bin/cmd",
                "package",
                "install",
                "-r",
                "--user",
                "10",
                "-S",
                "4567"
            ]
        );
    }

    #[test]
    fn descriptor_survives_unlink_and_is_close_on_exec() {
        let mut original = tempfile::NamedTempFile::new().unwrap();
        original.write_all(b"PK\x03\x04test payload").unwrap();
        let (sender, receiver) = UnixStream::pair().unwrap();
        send_file(&sender, b'{', original.as_file()).unwrap();
        drop(original);
        let (byte, file) = receive_file(&receiver).unwrap();
        assert_eq!(byte, b'{');
        let mut file = file.unwrap();
        assert_eq!(validate(&file).unwrap(), 16);
        // SAFETY: fcntl reads flags on the live descriptor and takes no pointer.
        assert_ne!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        file.seek(SeekFrom::Start(0)).unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"PK\x03\x04test payload");
    }

    #[test]
    fn legacy_json_needs_no_descriptor() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        sender.write_all(b"{").unwrap();
        let (byte, file) = receive_file(&receiver).unwrap();
        assert_eq!(byte, b'{');
        assert!(file.is_none());
    }

    #[test]
    fn rejects_multiple_and_truncated_descriptors() {
        for count in [2_usize, 100] {
            let file = tempfile::tempfile().unwrap();
            let (sender, receiver) = UnixStream::pair().unwrap();
            let mut byte = b'{';
            let mut vector = libc::iovec {
                iov_base: std::ptr::from_mut(&mut byte).cast(),
                iov_len: 1,
            };
            // usize storage provides cmsghdr alignment and enough room for
            // 100 descriptors, deliberately exceeding the receiver's buffer.
            let mut storage = [0_usize; 128];
            // SAFETY: the initialized C structures reference live, aligned
            // buffers; every outgoing fd is borrowed from the same live file.
            unsafe {
                let mut message: libc::msghdr = std::mem::zeroed();
                message.msg_iov = &raw mut vector;
                message.msg_iovlen = 1;
                message.msg_control = storage.as_mut_ptr().cast();
                let bytes = (count * size_of::<libc::c_int>()) as u32;
                message.msg_controllen = libc::CMSG_SPACE(bytes) as usize;
                let header = libc::CMSG_FIRSTHDR(&message);
                (*header).cmsg_level = libc::SOL_SOCKET;
                (*header).cmsg_type = libc::SCM_RIGHTS;
                (*header).cmsg_len = libc::CMSG_LEN(bytes) as usize;
                let data = libc::CMSG_DATA(header).cast::<libc::c_int>();
                for index in 0..count {
                    data.add(index).write_unaligned(file.as_raw_fd());
                }
                assert_eq!(
                    libc::sendmsg(sender.as_raw_fd(), &message, libc::MSG_NOSIGNAL),
                    1
                );
            }
            assert!(receive_file(&receiver).is_err());
        }
    }

    #[test]
    fn rejects_non_files_empty_oversized_and_non_zip_inputs() {
        assert!(validate(&File::open("/dev/null").unwrap()).is_err());
        let mut file = tempfile::tempfile().unwrap();
        assert!(validate(&file).is_err());
        file.write_all(b"not an apk").unwrap();
        assert!(validate(&file).is_err());
        file.set_len(MAX_APK_BYTES + 1).unwrap();
        assert!(validate(&file).is_err());
    }

    #[test]
    fn requires_explicit_package_manager_success() {
        assert!(check_result("Success\n").is_ok());
        for output in [
            "",
            "Failure [INSTALL_FAILED_NO_MATCHING_ABIS]",
            "device is still booting",
            "Success\nFailure [INSTALL_FAILED_INVALID_APK]",
        ] {
            assert!(check_result(output).is_err());
        }
    }

    #[test]
    fn package_command_receives_apk_on_standard_input() {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(b"PK\x03\x04test payload").unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let output =
            diagnostics::capture_with_stdin(&mut droidloom_cpu_placement::command("/usr/bin/cat"), Stdio::from(file))
                .unwrap();
        assert_eq!(output.as_bytes(), b"PK\x03\x04test payload");
    }
}
