//! Capture source for the PCM sent into the cell.
//!
//! Samples are piped from the reference `PipeWire` CLI client `pw-cat --record`.
//! Android's silent input expects signed 16-bit PCM at 48 000 Hz, mono.
//! One child is spawned per recording stream.

use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt;
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
    let mut pipe = recorder.stdout.take().ok_or_else(|| {
        SourceError::Spawn(io::Error::other("host recorder has no standard output"))
    })?;
    let forwarded = pump(&mut pipe, &mut sink);
    drop(pipe);
    // A recorder that already failed must not be mistaken for our cleanup.
    // On Unix, Child::kill sends SIGKILL (9). Only that requested termination
    // is expected when the peer closes; other signal exits remain failures.
    let (status, stopped_by_bridge) = if let Some(status) = recorder.try_wait()? {
        (status, false)
    } else {
        let stopped = recorder.kill().is_ok();
        (recorder.wait()?, stopped)
    };
    if !(status.success() || stopped_by_bridge && status.signal() == Some(9)) {
        return Err(SourceError::Recorder(status));
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
    fn recorder_signal_failure_is_not_a_successful_empty_capture() {
        use std::os::unix::process::ExitStatusExt;

        let mut command = Command::new("sh");
        command.args(["-c", "kill -TERM $$"]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let result = record_with(command, io::sink());
        assert!(
            matches!(result, Err(SourceError::Recorder(status)) if status.signal().is_some()),
            "a recorder killed by a signal must not report successful capture: {result:?}"
        );
    }

    #[test]
    fn recorder_successful_eof_preserves_samples() {
        let mut command = Command::new("sh");
        command.args(["-c", "printf pcm"]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut samples = Vec::new();
        assert_eq!(record_with(command, &mut samples).unwrap(), 3);
        assert_eq!(samples, b"pcm");
    }

    #[test]
    fn closed_peer_stops_the_owned_recorder_without_failing_capture() {
        struct ClosedPeer;
        impl Write for ClosedPeer {
            fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
                Err(io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let mut command = Command::new("sh");
        command.args(["-c", "printf pcm; exec sleep 30"]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        assert_eq!(record_with(command, ClosedPeer).unwrap(), 0);
    }

    #[test]
    fn recorder_nonzero_exit_is_reported() {
        let mut command = Command::new("sh");
        command.args(["-c", "exit 7"]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        assert!(matches!(
            record_with(command, io::sink()),
            Err(SourceError::Recorder(status)) if status.code() == Some(7)
        ));
    }

    #[test]
    fn documented_input_format_matches_the_stream_parameters() {
        assert_eq!(FORMAT, format!("s16le {} kHz mono", SAMPLE_RATE / 1000));
        assert_eq!(CHANNELS, 1);
        assert_eq!(BYTES_PER_SECOND, 96_000);
        assert_eq!(duration_of(BYTES_PER_SECOND), Duration::from_secs(1));
        assert_eq!(duration_of(0), Duration::ZERO);
    }
}
