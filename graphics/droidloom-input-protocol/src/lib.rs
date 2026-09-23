//! Fixed-size, descriptor-free records for the Android framework bridge.
//!
//! The Wayland compositor remains the physical-input authority. This protocol
//! carries touch and keyboard events already routed to one authenticated
//! Android task surface.

#![no_std]
#![forbid(unsafe_code)]

use core::fmt;

/// Marker at the start of every input record.
pub const MAGIC: [u8; 4] = *b"DLIN";
/// Wire protocol major version.
pub const PROTOCOL_MAJOR: u16 = 6;

/// Build metadata retained by the native sender for offline package verification.
pub const BUILD_COMPATIBILITY: [u8; 26] = compatibility_marker(PROTOCOL_MAJOR);

const fn compatibility_marker(mut version: u16) -> [u8; 26] {
    let mut marker = *b"DROIDLOOM_INPUT_ABI=00000;";
    let mut index = 24;
    loop {
        marker[index] = b'0' + (version % 10) as u8;
        version /= 10;
        if index == 20 {
            break;
        }
        index -= 1;
    }
    marker
}

/// Exact size of one sequenced-packet record.
pub const RECORD_BYTES: usize = 64;
/// Record kind for a touchscreen contact update.
pub const KIND_TOUCH: u8 = 1;
/// Record kind for one Linux input key transition.
pub const KIND_KEY: u8 = 2;
/// Record kind for one Android task-bounds update.
pub const KIND_TASK_BOUNDS: u8 = 3;
/// Record kind for one Android task focus update.
pub const KIND_TASK_FOCUS: u8 = 4;
/// Record kind for a request to remove one Android task.
pub const KIND_TASK_CLOSE: u8 = 5;
/// Record kind for one graphics-tablet tool update.
pub const KIND_TABLET: u8 = 6;
/// Tablet action: tool entered proximity of a task surface.
pub const TABLET_ACTION_PROXIMITY_IN: u8 = 0;
/// Tablet action: hover or in-contact motion update.
pub const TABLET_ACTION_MOTION: u8 = 1;
/// Tablet action: stylus tip touched the surface.
pub const TABLET_ACTION_DOWN: u8 = 2;
/// Tablet action: stylus tip left the surface.
pub const TABLET_ACTION_UP: u8 = 3;
/// Tablet action: tool left proximity of a task surface.
pub const TABLET_ACTION_PROXIMITY_OUT: u8 = 4;
/// Tablet action: one tool button was pressed.
pub const TABLET_ACTION_BUTTON_PRESS: u8 = 5;
/// Tablet action: one tool button was released.
pub const TABLET_ACTION_BUTTON_RELEASE: u8 = 6;
/// Tablet action: cancel the current tool state.
pub const TABLET_ACTION_CANCEL: u8 = 7;
/// Tablet action: wheel movement was reported.
pub const TABLET_ACTION_WHEEL: u8 = 8;
/// Tablet tool type: regular pen-like tool.
pub const TABLET_TOOL_PEN: u8 = 0;
/// Tablet tool type: inverted/eraser tool.
pub const TABLET_TOOL_ERASER: u8 = 1;
/// Tablet tool type: brush.
pub const TABLET_TOOL_BRUSH: u8 = 2;
/// Tablet tool type: pencil.
pub const TABLET_TOOL_PENCIL: u8 = 3;
/// Tablet tool type: airbrush.
pub const TABLET_TOOL_AIRBRUSH: u8 = 4;
/// Tablet axis-validity bit for pressure.
pub const TABLET_AXIS_PRESSURE: u8 = 1 << 0;
/// Tablet axis-validity bit for distance.
pub const TABLET_AXIS_DISTANCE: u8 = 1 << 1;
/// Tablet axis-validity bit for tilt.
pub const TABLET_AXIS_TILT: u8 = 1 << 2;
/// Tablet axis-validity bit for rotation.
pub const TABLET_AXIS_ROTATION: u8 = 1 << 3;
/// Tablet axis-validity bit for slider.
pub const TABLET_AXIS_SLIDER: u8 = 1 << 4;
/// Tablet axis-validity bit for wheel.
pub const TABLET_AXIS_WHEEL: u8 = 1 << 5;
/// Highest pointer identity accepted by Android `MotionEvent`.
pub const MAX_POINTER_ID: u32 = 31;
/// Highest key identity defined by Linux's evdev input ABI.
pub const MAX_EVDEV_KEYCODE: u32 = 0x2ff;

/// Invalid task/display/input data at the framework bridge boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputProtocolError {
    /// Android display IDs are signed nonnegative 32-bit integers.
    AndroidDisplay,
    /// Android accepts pointer identities from zero through 31.
    PointerId,
    /// Linux evdev key identities are bounded by `KEY_MAX`.
    KeyCode,
    /// Touch actions are down, motion, up, or cancel.
    Action,
    /// Logical-to-buffer scale components must both be non-zero.
    Scale,
    /// A scaled signed 16.16 coordinate cannot fit on the wire.
    Coordinate,
    /// A tablet tool type is outside the supported pen/eraser range.
    ToolType,
    /// Android task identities are positive signed 32-bit integers.
    TaskId,
    /// Task bounds must be non-zero signed 32-bit dimensions.
    TaskExtent,
}

impl fmt::Display for InputProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::AndroidDisplay => "Android display ID is outside the signed framework range",
            Self::PointerId => "touch pointer identity exceeds Android's limit",
            Self::KeyCode => "key identity exceeds Linux evdev's limit",
            Self::Action => "input action is outside the protocol range",
            Self::Scale => "logical-to-buffer scale components must be non-zero",
            Self::Coordinate => "scaled touch coordinate exceeds the signed 16.16 wire range",
            Self::ToolType => "tablet tool type is outside the protocol range",
            Self::TaskId => "Android task ID is outside the positive framework range",
            Self::TaskExtent => "Android task bounds are outside the framework range",
        })
    }
}

/// Convert one signed 16.16 logical coordinate to buffer pixels.
///
/// # Errors
///
/// Rejects zero scale components and results outside the signed wire range.
pub fn scale_fixed(
    value: i32,
    numerator: u32,
    denominator: u32,
) -> Result<i32, InputProtocolError> {
    if numerator == 0 || denominator == 0 {
        return Err(InputProtocolError::Scale);
    }
    let scaled = i64::from(value)
        .checked_mul(i64::from(numerator))
        .ok_or(InputProtocolError::Coordinate)?
        / i64::from(denominator);
    i32::try_from(scaled).map_err(|_| InputProtocolError::Coordinate)
}

/// Encode one routed touchscreen sample.
///
/// # Errors
///
/// Rejects display and pointer identities Android cannot represent.
#[allow(clippy::too_many_arguments)]
pub fn encode_touch(
    android_display: u32,
    task_id: u64,
    timestamp_nanos: u64,
    action: u8,
    pointer_id: u32,
    x_fixed: i32,
    y_fixed: i32,
    pressure: u16,
) -> Result<[u8; RECORD_BYTES], InputProtocolError> {
    if android_display > i32::MAX as u32 {
        return Err(InputProtocolError::AndroidDisplay);
    }
    let task_id = u32::try_from(task_id).map_err(|_| InputProtocolError::TaskId)?;
    if task_id == 0 || task_id > i32::MAX as u32 {
        return Err(InputProtocolError::TaskId);
    }
    if pointer_id > MAX_POINTER_ID {
        return Err(InputProtocolError::PointerId);
    }
    if action > 3 {
        return Err(InputProtocolError::Action);
    }

    let mut record = [0_u8; RECORD_BYTES];
    record[0..4].copy_from_slice(&MAGIC);
    record[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record[6] = KIND_TOUCH;
    record[7] = action;
    record[8..12].copy_from_slice(&android_display.to_le_bytes());
    record[12..16].copy_from_slice(&pointer_id.to_le_bytes());
    record[16..24].copy_from_slice(&timestamp_nanos.to_le_bytes());
    record[24..28].copy_from_slice(&x_fixed.to_le_bytes());
    record[28..32].copy_from_slice(&y_fixed.to_le_bytes());
    record[32..34].copy_from_slice(&pressure.to_le_bytes());
    // Bytes 34..36 are reserved and remain zero.
    record[36..40].copy_from_slice(&task_id.to_le_bytes());
    Ok(record)
}

/// Encode one routed Linux input key transition.
///
/// The key code remains in evdev space on the wire. The Android framework
/// bridge converts it to a `KeyEvent` code while retaining the original scan
/// code for diagnostics and policy.
///
/// # Errors
///
/// Rejects display IDs, key identities, and actions outside their platform
/// ranges.
pub fn encode_key(
    android_display: u32,
    task_id: u64,
    timestamp_nanos: u64,
    action: u8,
    keycode: u32,
    repeat: u16,
    route_serial: u64,
) -> Result<[u8; RECORD_BYTES], InputProtocolError> {
    if android_display > i32::MAX as u32 {
        return Err(InputProtocolError::AndroidDisplay);
    }
    let task_id = u32::try_from(task_id).map_err(|_| InputProtocolError::TaskId)?;
    if task_id == 0 || task_id > i32::MAX as u32 {
        return Err(InputProtocolError::TaskId);
    }
    if keycode > MAX_EVDEV_KEYCODE {
        return Err(InputProtocolError::KeyCode);
    }
    if action > 1 {
        return Err(InputProtocolError::Action);
    }

    let mut record = [0_u8; RECORD_BYTES];
    record[0..4].copy_from_slice(&MAGIC);
    record[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record[6] = KIND_KEY;
    record[7] = action;
    record[8..12].copy_from_slice(&android_display.to_le_bytes());
    record[12..16].copy_from_slice(&keycode.to_le_bytes());
    record[16..24].copy_from_slice(&timestamp_nanos.to_le_bytes());
    record[24..26].copy_from_slice(&repeat.to_le_bytes());
    // Bytes 26..28 are reserved and remain zero.
    record[28..36].copy_from_slice(&route_serial.to_le_bytes());
    record[36..40].copy_from_slice(&task_id.to_le_bytes());
    Ok(record)
}

/// Encode a requested content size for one Android task.
///
/// The Android task stays at a private origin because the Wayland compositor
/// owns its real desktop position. Its pixel bounds track the toplevel's
/// physical Wayland buffer size. The scale remains explicit for coordinate
/// conversion only; Android density is a separate persistent user setting and
/// must not be overridden per task.
///
/// # Errors
///
/// Rejects task identities and dimensions Android cannot represent.
pub fn encode_task_bounds(
    task_id: u64,
    android_display: u32,
    width: u32,
    height: u32,
    scale_numerator: u32,
    scale_denominator: u32,
) -> Result<[u8; RECORD_BYTES], InputProtocolError> {
    let task_id = u32::try_from(task_id).map_err(|_| InputProtocolError::TaskId)?;
    if task_id == 0 || task_id > i32::MAX as u32 {
        return Err(InputProtocolError::TaskId);
    }
    if width == 0 || width > i32::MAX as u32 || height == 0 || height > i32::MAX as u32 {
        return Err(InputProtocolError::TaskExtent);
    }
    if android_display > i32::MAX as u32 {
        return Err(InputProtocolError::AndroidDisplay);
    }
    if scale_numerator == 0 || scale_denominator == 0 {
        return Err(InputProtocolError::Scale);
    }

    let mut record = [0_u8; RECORD_BYTES];
    record[0..4].copy_from_slice(&MAGIC);
    record[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record[6] = KIND_TASK_BOUNDS;
    record[8..12].copy_from_slice(&task_id.to_le_bytes());
    record[12..16].copy_from_slice(&width.to_le_bytes());
    record[16..20].copy_from_slice(&height.to_le_bytes());
    record[20..24].copy_from_slice(&android_display.to_le_bytes());
    record[24..28].copy_from_slice(&scale_numerator.to_le_bytes());
    record[28..32].copy_from_slice(&scale_denominator.to_le_bytes());
    Ok(record)
}

/// Encode the host keyboard-focus state for one Android task.
///
/// # Errors
///
/// Rejects task identities Android cannot represent.
pub fn encode_task_focus(
    task_id: u64,
    focused: bool,
) -> Result<[u8; RECORD_BYTES], InputProtocolError> {
    let task_id = u32::try_from(task_id).map_err(|_| InputProtocolError::TaskId)?;
    if task_id == 0 || task_id > i32::MAX as u32 {
        return Err(InputProtocolError::TaskId);
    }

    let mut record = [0_u8; RECORD_BYTES];
    record[0..4].copy_from_slice(&MAGIC);
    record[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record[6] = KIND_TASK_FOCUS;
    record[7] = u8::from(focused);
    record[8..12].copy_from_slice(&task_id.to_le_bytes());
    Ok(record)
}

/// Encode a request to finish and remove one Android task.
///
/// # Errors
///
/// Rejects task identities Android cannot represent.
pub fn encode_task_close(task_id: u64) -> Result<[u8; RECORD_BYTES], InputProtocolError> {
    let task_id = u32::try_from(task_id).map_err(|_| InputProtocolError::TaskId)?;
    if task_id == 0 || task_id > i32::MAX as u32 {
        return Err(InputProtocolError::TaskId);
    }

    let mut record = [0_u8; RECORD_BYTES];
    record[0..4].copy_from_slice(&MAGIC);
    record[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record[6] = KIND_TASK_CLOSE;
    record[8..12].copy_from_slice(&task_id.to_le_bytes());
    Ok(record)
}

/// Encode one routed graphics-tablet update.
///
/// The angular values use tenths of a degree. wheel_degrees_fixed and the
/// coordinates use signed 16.16 values. The receiver may ignore axes it does
/// not understand, but all values remain available for a future Android axis
/// mapping without changing the record ABI again.
#[allow(clippy::too_many_arguments)]
pub fn encode_tablet(
    android_display: u32,
    task_id: u64,
    timestamp_nanos: u64,
    action: u8,
    pointer_id: u32,
    x_fixed: i32,
    y_fixed: i32,
    pressure: u16,
    distance: u16,
    tilt_x_tenths: i16,
    tilt_y_tenths: i16,
    rotation_tenths: i16,
    wheel_clicks: i16,
    slider: i32,
    wheel_degrees_fixed: i32,
    button: u32,
    tool_type: u8,
    axis_flags: u8,
) -> Result<[u8; RECORD_BYTES], InputProtocolError> {
    if android_display > i32::MAX as u32 {
        return Err(InputProtocolError::AndroidDisplay);
    }
    let task_id = u32::try_from(task_id).map_err(|_| InputProtocolError::TaskId)?;
    if task_id == 0 || task_id > i32::MAX as u32 {
        return Err(InputProtocolError::TaskId);
    }
    if pointer_id > MAX_POINTER_ID {
        return Err(InputProtocolError::PointerId);
    }
    if action > TABLET_ACTION_WHEEL {
        return Err(InputProtocolError::Action);
    }
    if tool_type > TABLET_TOOL_AIRBRUSH {
        return Err(InputProtocolError::ToolType);
    }

    let mut record = [0_u8; RECORD_BYTES];
    record[0..4].copy_from_slice(&MAGIC);
    record[4..6].copy_from_slice(&PROTOCOL_MAJOR.to_le_bytes());
    record[6] = KIND_TABLET;
    record[7] = action;
    record[8..12].copy_from_slice(&android_display.to_le_bytes());
    record[12..16].copy_from_slice(&pointer_id.to_le_bytes());
    record[16..24].copy_from_slice(&timestamp_nanos.to_le_bytes());
    record[24..28].copy_from_slice(&x_fixed.to_le_bytes());
    record[28..32].copy_from_slice(&y_fixed.to_le_bytes());
    record[32..34].copy_from_slice(&pressure.to_le_bytes());
    record[34..36].copy_from_slice(&distance.to_le_bytes());
    record[36..38].copy_from_slice(&tilt_x_tenths.to_le_bytes());
    record[38..40].copy_from_slice(&tilt_y_tenths.to_le_bytes());
    record[40..42].copy_from_slice(&rotation_tenths.to_le_bytes());
    record[42..44].copy_from_slice(&wheel_clicks.to_le_bytes());
    record[44..48].copy_from_slice(&slider.to_le_bytes());
    record[48..52].copy_from_slice(&wheel_degrees_fixed.to_le_bytes());
    record[52..56].copy_from_slice(&button.to_le_bytes());
    record[56..60].copy_from_slice(&task_id.to_le_bytes());
    record[60] = tool_type;
    record[61] = axis_flags;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_compatibility_tracks_the_encoder_version() {
        for version in [0, 4, 5, 99, 10000, u16::MAX] {
            let marker = compatibility_marker(version);
            let decoded = core::str::from_utf8(&marker)
                .unwrap()
                .strip_prefix("DROIDLOOM_INPUT_ABI=")
                .unwrap()
                .strip_suffix(';')
                .unwrap()
                .parse::<u16>()
                .unwrap();
            assert_eq!(decoded, version);
        }
        assert_eq!(BUILD_COMPATIBILITY, compatibility_marker(PROTOCOL_MAJOR));
    }

    #[test]
    fn touch_record_has_stable_little_endian_layout() {
        let record = encode_touch(
            3,
            29,
            0x0102_0304_0506_0708,
            1,
            7,
            0x0012_8000,
            0x0034_4000,
            32_768,
        )
        .unwrap();

        assert_eq!(&record[0..4], b"DLIN");
        assert_eq!(&record[4..6], &PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(record[6], KIND_TOUCH);
        assert_eq!(record[7], 1);
        assert_eq!(&record[8..12], &3_u32.to_le_bytes());
        assert_eq!(&record[12..16], &7_u32.to_le_bytes());
        assert_eq!(&record[16..24], &0x0102_0304_0506_0708_u64.to_le_bytes());
        assert_eq!(&record[24..28], &0x0012_8000_i32.to_le_bytes());
        assert_eq!(&record[28..32], &0x0034_4000_i32.to_le_bytes());
        assert_eq!(&record[32..34], &32_768_u16.to_le_bytes());
        assert_eq!(&record[34..36], &[0, 0]);
        assert_eq!(&record[36..40], &29_u32.to_le_bytes());
    }

    #[test]
    fn tablet_record_has_stable_little_endian_layout() {
        let record = encode_tablet(
            3,
            29,
            0x0102_0304_0506_0708,
            TABLET_ACTION_MOTION,
            7,
            0x0012_8000,
            0x0034_4000,
            32_768,
            1_024,
            -120,
            340,
            900,
            -2,
            -12_345,
            0x0002_0000,
            0x14a,
            TABLET_TOOL_ERASER,
            TABLET_AXIS_PRESSURE | TABLET_AXIS_TILT,
        )
        .unwrap();

        assert_eq!(record.len(), RECORD_BYTES);
        assert_eq!(&record[0..4], b"DLIN");
        assert_eq!(&record[4..6], &PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(record[6], KIND_TABLET);
        assert_eq!(record[7], TABLET_ACTION_MOTION);
        assert_eq!(&record[8..12], &3_u32.to_le_bytes());
        assert_eq!(&record[12..16], &7_u32.to_le_bytes());
        assert_eq!(&record[16..24], &0x0102_0304_0506_0708_u64.to_le_bytes());
        assert_eq!(&record[36..38], &(-120_i16).to_le_bytes());
        assert_eq!(&record[42..44], &(-2_i16).to_le_bytes());
        assert_eq!(&record[52..56], &0x14a_u32.to_le_bytes());
        assert_eq!(&record[56..60], &29_u32.to_le_bytes());
        assert_eq!(record[60], TABLET_TOOL_ERASER);
        assert_eq!(record[61], TABLET_AXIS_PRESSURE | TABLET_AXIS_TILT);
        assert!(record[62..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn invalid_tablet_identity_and_tool_fail_closed() {
        assert_eq!(
            encode_tablet(0, 29, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0),
            Err(InputProtocolError::ToolType)
        );
        assert_eq!(
            encode_tablet(0, 29, 1, 0, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0),
            Err(InputProtocolError::PointerId)
        );
        assert_eq!(
            encode_tablet(0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0),
            Err(InputProtocolError::TaskId)
        );
    }

    #[test]
    fn unrepresentable_android_ids_fail_closed() {
        assert_eq!(
            encode_touch(i32::MAX as u32 + 1, 29, 1, 0, 0, 0, 0, u16::MAX,),
            Err(InputProtocolError::AndroidDisplay)
        );
        assert_eq!(
            encode_touch(0, 29, 1, 0, 32, 0, 0, u16::MAX),
            Err(InputProtocolError::PointerId)
        );
        assert_eq!(
            encode_touch(0, 29, 1, 4, 0, 0, 0, u16::MAX),
            Err(InputProtocolError::Action)
        );
        assert_eq!(
            encode_touch(0, 0, 1, 0, 0, 0, 0, u16::MAX),
            Err(InputProtocolError::TaskId)
        );
    }

    #[test]
    fn key_record_has_stable_little_endian_layout() {
        let record = encode_key(
            3,
            29,
            0x0102_0304_0506_0708,
            1,
            30,
            7,
            0x1112_1314_1516_1718,
        )
        .unwrap();

        assert_eq!(&record[0..4], b"DLIN");
        assert_eq!(&record[4..6], &PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(record[6], KIND_KEY);
        assert_eq!(record[7], 1);
        assert_eq!(&record[8..12], &3_u32.to_le_bytes());
        assert_eq!(&record[12..16], &30_u32.to_le_bytes());
        assert_eq!(&record[16..24], &0x0102_0304_0506_0708_u64.to_le_bytes());
        assert_eq!(&record[24..26], &7_u16.to_le_bytes());
        assert_eq!(&record[26..28], &[0; 2]);
        assert_eq!(&record[28..36], &0x1112_1314_1516_1718_u64.to_le_bytes());
        assert_eq!(&record[36..40], &29_u32.to_le_bytes());
    }

    #[test]
    fn invalid_key_records_fail_closed() {
        assert_eq!(
            encode_key(i32::MAX as u32 + 1, 29, 1, 0, 30, 0, 1),
            Err(InputProtocolError::AndroidDisplay)
        );
        assert_eq!(
            encode_key(0, 29, 1, 0, MAX_EVDEV_KEYCODE + 1, 0, 1),
            Err(InputProtocolError::KeyCode)
        );
        assert_eq!(
            encode_key(0, 29, 1, 2, 30, 0, 1),
            Err(InputProtocolError::Action)
        );
        assert_eq!(
            encode_key(0, 0, 1, 0, 30, 0, 1),
            Err(InputProtocolError::TaskId)
        );
    }

    #[test]
    fn scales_logical_coordinates_to_two_x_buffer_pixels() {
        assert_eq!(scale_fixed(320 << 16, 2, 1), Ok(640 << 16));
    }

    #[test]
    fn supports_fractional_and_negative_scales() {
        assert_eq!(scale_fixed(200 << 16, 3, 2), Ok(300 << 16));
        assert_eq!(scale_fixed(-(20 << 16), 3, 2), Ok(-(30 << 16)));
    }

    #[test]
    fn invalid_scales_and_wire_overflow_fail_closed() {
        assert_eq!(scale_fixed(1, 0, 1), Err(InputProtocolError::Scale));
        assert_eq!(scale_fixed(1, 1, 0), Err(InputProtocolError::Scale));
        assert_eq!(
            scale_fixed(i32::MAX, 2, 1),
            Err(InputProtocolError::Coordinate)
        );
    }

    #[test]
    fn task_bounds_record_has_stable_little_endian_layout() {
        let record = encode_task_bounds(29, 0, 800, 600, 132, 120).unwrap();

        assert_eq!(&record[0..4], b"DLIN");
        assert_eq!(&record[4..6], &PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(record[6], KIND_TASK_BOUNDS);
        assert_eq!(record[7], 0);
        assert_eq!(&record[8..12], &29_u32.to_le_bytes());
        assert_eq!(&record[12..16], &800_u32.to_le_bytes());
        assert_eq!(&record[16..20], &600_u32.to_le_bytes());
        assert_eq!(&record[20..24], &0_u32.to_le_bytes());
        assert_eq!(&record[24..28], &132_u32.to_le_bytes());
        assert_eq!(&record[28..32], &120_u32.to_le_bytes());
        assert!(record[32..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn invalid_task_bounds_fail_closed() {
        assert_eq!(
            encode_task_bounds(0, 0, 800, 600, 1, 1),
            Err(InputProtocolError::TaskId)
        );
        assert_eq!(
            encode_task_bounds(29, 0, 0, 600, 1, 1),
            Err(InputProtocolError::TaskExtent)
        );
        assert_eq!(
            encode_task_bounds(29, 0, 800, 600, 0, 1),
            Err(InputProtocolError::Scale)
        );
    }

    #[test]
    fn task_focus_record_has_stable_little_endian_layout() {
        let record = encode_task_focus(29, true).unwrap();

        assert_eq!(&record[0..4], b"DLIN");
        assert_eq!(&record[4..6], &PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(record[6], KIND_TASK_FOCUS);
        assert_eq!(record[7], 1);
        assert_eq!(&record[8..12], &29_u32.to_le_bytes());
        assert!(record[12..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn invalid_task_focus_fails_closed() {
        assert_eq!(encode_task_focus(0, true), Err(InputProtocolError::TaskId));
    }

    #[test]
    fn task_close_record_has_stable_little_endian_layout() {
        let record = encode_task_close(29).unwrap();

        assert_eq!(&record[0..4], b"DLIN");
        assert_eq!(&record[4..6], &PROTOCOL_MAJOR.to_le_bytes());
        assert_eq!(record[6], KIND_TASK_CLOSE);
        assert_eq!(record[7], 0);
        assert_eq!(&record[8..12], &29_u32.to_le_bytes());
        assert!(record[12..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn invalid_task_close_fails_closed() {
        assert_eq!(encode_task_close(0), Err(InputProtocolError::TaskId));
    }
}
