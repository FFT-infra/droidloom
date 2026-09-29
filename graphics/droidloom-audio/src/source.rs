//! Capture source for the PCM sent into the cell.
//!
//! Samples are piped from the reference `PipeWire` CLI client `pw-cat --record`.
//! Android's silent input expects signed 16-bit PCM at 48 000 Hz, mono.
//! One child is spawned per recording stream.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Sample rate of the Android input stream.
pub const SAMPLE_RATE: u32 = 48_000;
/// Channel count of the Android primary input.
pub const CHANNELS: u8 = 1;
/// Bytes per sample: signed 16-bit little-endian.
pub const SAMPLE_BYTES: u8 = 2;
/// PCM bytes that one second of the capture format occupies.
pub const BYTES_PER_SECOND: u64 = SAMPLE_RATE as u64 * CHANNELS as u64 * SAMPLE_BYTES as u64;
/// Transport format as documented and logged.
pub const FORMAT: &str = "s16le 48 kHz mono";
/// Host client that records from the session sound server.
const RECORDER: &str = "pw-cat";
/// Requested source latency.
const LATENCY: &str = "60ms";
/// Copy chunk: 4 KiB for capture.
const CHUNK: usize = 4 * 1024;

/// Failures of a single cell capture stream. None of them stop the service.
#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    /// The host recorder could not be started.
    #[error("cannot start the host recorder: {0}")]
    Spawn(#[source] io::Error),
    /// The cell connection or the pipe from the recorder failed.
    #[error(transparent)]
    Stream(#[from] io::Error),
    /// The host recorder exited with an error.
    #[error("host recorder exited with {0}")]
    Recorder(std::process::ExitStatus),
}

/// Capture time of a byte count at the transport format.
pub fn duration_of(bytes: u64) -> Duration {
    Duration::from_micros(bytes.saturating_mul(1_000_000) / BYTES_PER_SECOND)
}

/// Record from the host sound server into the cell stream until the peer closes it.
pub fn record<W: Write>(sink: W) -> Result<u64, SourceError> {
    record_with(record_command(), sink)
}

/// The host recorder invocation for the transport format.
fn record_command() -> Command {
    let mut command = Command::new(RECORDER);
    command.args([
        "--record",
        "--raw",
        "--format",
        "s16",
        "--rate",
        &SAMPLE_RATE.to_string(),
        "--channels",
        &CHANNELS.to_string(),
        "--latency",
        LATENCY,
        "-",
    ]);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    command
}

fn record_with<W: Write>(mut command: Command, mut sink: W) -> Result<u64, SourceError> {
    let mut recorder = command.spawn().map_err(SourceError::Spawn)?;
    let mut pipe = recorder
        .stdout
        .take()
        .ok_or_else(|| SourceError::Spawn(io::Error::other("host recorder has no standard output")))?;
    let forwarded = pump(&mut pipe, &mut sink);
    drop(pipe);
    let _ = recorder.kill();
    let status = recorder.wait()?;
    if !status.success() {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if status.signal().is_none() {
                return Err(SourceError::Recorder(status));
            }
        }
    }
    Ok(forwarded?)
}

fn pump<W: Write>(source: &mut impl Read, sink: &mut W) -> io::Result<u64> {
    let mut buffer = vec![0_u8; CHUNK];
    let mut forwarded = 0_u64;
    loop {
        match source.read(&mut buffer) {
            Ok(0) => return Ok(forwarded),
            Ok(bytes) => {
                if let Err(error) = sink.write_all(&buffer[..bytes]) {
                    if error.kind() == io::ErrorKind::BrokenPipe {
                        return Ok(forwarded);
                    }
                    return Err(error);
                }
                forwarded += bytes as u64;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documented_input_format_matches_the_stream_parameters() {
        assert_eq!(FORMAT, format!("s16le {} kHz mono", SAMPLE_RATE / 1000));
        assert_eq!(CHANNELS, 1);
        assert_eq!(BYTES_PER_SECOND, 96_000);
        assert_eq!(duration_of(BYTES_PER_SECOND), Duration::from_secs(1));
        assert_eq!(duration_of(0), Duration::ZERO);
    }
}
