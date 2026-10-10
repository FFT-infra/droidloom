//! Direct RPC to the existing Android framework process and FD-only event delegation.
use super::*;
use std::net::Shutdown;
use std::os::fd::OwnedFd;

pub(super) fn connect(init: u32, endpoint: &str) -> Result<UnixStream, ControlError> {
    let path = format!("/proc/{init}/root/dev/socket/droidloom-framework/{endpoint}");
    let metadata =
        fs::symlink_metadata(&path).map_err(|e| io_error("inspect framework endpoint", e))?;
    if !metadata.file_type().is_socket() || metadata.uid() != 1000 || metadata.mode() & 0o077 != 0 {
        return Err(ControlError::Invalid(
            "framework endpoint has unsafe ownership or mode".into(),
        ));
    }
    let stream =
        UnixStream::connect(&path).map_err(|e| io_error("connect framework endpoint", e))?;
    if peer_uid(&stream)? != 1000 {
        return Err(ControlError::Invalid(
            "framework peer is not system UID".into(),
        ));
    }
    Ok(stream)
}
pub(super) fn framework_request(
    init: u32,
    op: u16,
    request: &serde_json::Value,
) -> Result<String, ControlError> {
    let mut stream = connect(init, "control")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(120)))
        .map_err(|e| io_error("set framework deadline", e))?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| io_error("set framework write deadline", e))?;
    framework_wire::write(
        &mut stream,
        op,
        &serde_json::to_vec(request)?,
        framework_wire::MAX_REQUEST,
    )
    .map_err(|e| io_error("send framework request", e))?;
    let (reply, bytes) = framework_wire::read(&mut stream, framework_wire::MAX_RESPONSE)
        .map_err(|e| io_error("read framework reply", e))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| ControlError::Invalid("framework reply is not UTF-8".into()))?;
    if reply == (op | framework_wire::ERROR) {
        return Err(ControlError::Invalid(format!(
            "Android framework rejected request: {text}"
        )));
    }
    if reply != op {
        return Err(ControlError::Invalid(
            "framework reply opcode mismatch".into(),
        ));
    }
    Ok(text)
}
pub(super) fn launch_in_cell(
    init: u32,
    package: &str,
    component: Option<&str>,
    resolution: Option<&str>,
    user: u32,
) -> Result<String, ControlError> {
    let mut request = serde_json::json!({"user":user,"package":package});
    if let Some(component) = component {
        request["component"] = component.into();
    }
    if let Some(resolution) = resolution {
        let dimensions = parse_launch_resolution(resolution)?;
        let (width, height) = dimensions.split_once('x').unwrap();
        request["width"] = width.parse::<u32>().unwrap().into();
        request["height"] = height.parse::<u32>().unwrap().into();
    }
    let output = framework_request(init, framework_wire::LAUNCH, &request)?;
    if !valid_launch_binding(&output) {
        return Err(ControlError::Invalid(
            "invalid framework launch binding".into(),
        ));
    }
    Ok(output)
}
fn valid_launch_binding(output: &str) -> bool {
    let mut fields = output.split(' ');
    for (name, low, high) in [
        ("object=", 1, i64::MAX as u64),
        ("composer_display=", 1, i64::MAX as u64),
        ("android_display=", 0, 0),
        ("task=", 1, i32::MAX as u64),
    ] {
        let Some(value) = fields.next().and_then(|field| field.strip_prefix(name)) else {
            return false;
        };
        if value.is_empty()
            || !value.bytes().all(|byte| byte.is_ascii_digit())
            || !value
                .parse::<u64>()
                .is_ok_and(|value| (low..=high).contains(&value))
        {
            return false;
        }
    }
    fields.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn launch_binding_requires_exact_attached_task_identity() {
        assert!(valid_launch_binding(
            "object=7 composer_display=1 android_display=0 task=42"
        ));
        for binding in [
            "object=0 composer_display=1 android_display=0 task=42",
            "object=7 composer_display=0 android_display=0 task=42",
            "object=7 composer_display=1 android_display=1 task=42",
            "object=7 composer_display=1 android_display=0 task=0",
            "object=7 composer_display=1 android_display=0 task=2147483648",
            "object=7 composer_display=1 android_display=0 task=42 extra=1",
            "object=7 composer_display=1 android_display=0 task=+42",
            "object=7 composer_display=1 task=42",
        ] {
            assert!(!valid_launch_binding(binding), "{binding}");
        }
    }
}
pub(super) fn set_dpi_in_cell(init: u32, dpi: u32) -> Result<(), ControlError> {
    let response = framework_request(
        init,
        framework_wire::DENSITY,
        &serde_json::json!({"user":0,"dpi":dpi}),
    )?;
    if response != format!("dpi={dpi}") {
        return Err(ControlError::Invalid(
            "density acknowledgement mismatch".into(),
        ));
    }
    Ok(())
}

/// Receive a framework catalog subscription descriptor from the authorized lifecycle daemon.
/// The stream carries big-endian u64 generations and closes on framework/cell loss.
/// # Errors
/// Returns authentication, cell readiness, descriptor or transport failures.
pub fn subscribe_applications(socket: &Path) -> Result<UnixStream, ControlError> {
    let mut control =
        UnixStream::connect(socket).map_err(|e| io_error("connect catalog subscription", e))?;
    if peer_uid(&control)? != 0 {
        return Err(ControlError::Invalid(
            "catalog delegation requires root daemon".into(),
        ));
    }
    control
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| io_error("set delegation deadline", e))?;
    control
        .write_all(&serde_json::to_vec(&ControlRequest::WatchApplications)?)
        .map_err(|e| io_error("send catalog subscription", e))?;
    control
        .shutdown(Shutdown::Write)
        .map_err(|e| io_error("finish catalog subscription", e))?;
    let (first, descriptor) = apk::receive_file(&control)?;
    if first != 1 || descriptor.is_none() {
        let mut message = vec![first];
        control
            .take(MAX_RESPONSE_BYTES)
            .read_to_end(&mut message)
            .map_err(|e| io_error("read delegation error", e))?;
        let response: ControlResponse = serde_json::from_slice(&message)?;
        return Err(ControlError::Invalid(response.message));
    }
    let stream = UnixStream::from(OwnedFd::from(descriptor.unwrap()));
    if peer_uid(&stream)? != 1000 {
        return Err(ControlError::Invalid(
            "catalog descriptor peer is not framework".into(),
        ));
    }
    Ok(stream)
}
