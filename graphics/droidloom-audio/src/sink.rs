//! Playback sink for the PCM that arrives from the cell.
//!
//! Samples are piped into the reference `PipeWire` CLI client instead of a linked
//! client library: the bridge takes no build dependency, and the pipe gives it
//! its pacing. The pipe buffer is bounded, so a stalled sink stops the reader,
//! which stops reading the cell socket, which back-pressures the writer instead
//! of accumulating latency on the host. One child is spawned per stream, so a
//! sink that dies takes only that stream down.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

/// Sample rate of the Android primary output.
pub const SAMPLE_RATE: u32 = 48_000;
/// Channel count of the Android primary output mix.
pub const CHANNELS: u8 = 2;
/// Bytes per sample: signed 16-bit little-endian.
pub const SAMPLE_BYTES: u8 = 2;
/// PCM bytes that one second of the transport format occupies.
pub const BYTES_PER_SECOND: u64 = SAMPLE_RATE as u64 * CHANNELS as u64 * SAMPLE_BYTES as u64;
/// Transport format as documented and logged.
pub const FORMAT: &str = "s16le 48 kHz stereo";
/// Host client that renders the stream on the session sound server.
const PLAYER: &str = "pw-cat";
/// Requested sink latency. Small enough for application audio, large enough to
/// keep ordinary scheduler jitter out of the device callback.
const LATENCY: &str = "60ms";
/// Copy chunk, about one device period at the transport format: a stream gains
/// at most one chunk of buffering in the bridge itself.
const CHUNK: usize = 8 * 1024;

/// Failures of a single cell stream. None of them stop the service.
#[derive(Debug, thiserror::Error)]
pub enum SinkError {
    /// The host player could not be started.
    #[error("cannot start the host player: {0}")]
    Spawn(#[source] io::Error),
    /// The cell connection or the pipe to the player failed.
    #[error(transparent)]
    Stream(#[from] io::Error),
    /// The host player refused the stream.
    #[error("host player exited with {0}")]
    Player(std::process::ExitStatus),
}

/// Playing time of a byte count at the transport format.
pub fn duration_of(bytes: u64) -> Duration {
    Duration::from_micros(bytes.saturating_mul(1_000_000) / BYTES_PER_SECOND)
}

/// Play one cell stream until the peer closes it.
///
/// Returns the number of PCM bytes forwarded. The stream is complete when the
/// peer closes its end and the host player accepts every byte.
pub fn play<R: Read>(source: R) -> Result<u64, SinkError> {
    play_with(playback_command(), source)
}

/// The host player invocation for the transport format.
fn playback_command() -> Command {
    let mut command = Command::new(PLAYER);
    command.args([
        "--playback",
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
    // Player diagnostics belong to the bridge's journal; the sink has no
    // standard output of its own.
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    command
}

fn play_with<R: Read>(mut command: Command, mut source: R) -> Result<u64, SinkError> {
    let mut player = command.spawn().map_err(SinkError::Spawn)?;
    let mut pipe = player
        .stdin
        .take()
        .ok_or_else(|| SinkError::Spawn(io::Error::other("host player has no standard input")))?;
    let forwarded = pump(&mut pipe, &mut source);
    // Closing the pipe ends the stream even when the cell connection failed
    // midway; the player is always reaped, and its own verdict outranks the
    // broken pipe that a player failure produces.
    drop(pipe);
    let status = player.wait()?;
    if !status.success() {
        return Err(SinkError::Player(status));
    }
    Ok(forwarded?)
}

fn pump<R: Read>(sink: &mut impl Write, source: &mut R) -> io::Result<u64> {
    let mut buffer = vec![0_u8; CHUNK];
    let mut forwarded = 0_u64;
    loop {
        match source.read(&mut buffer) {
            Ok(0) => return Ok(forwarded),
            Ok(bytes) => {
                sink.write_all(&buffer[..bytes])?;
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
    use std::fs;
    use std::io::Cursor;

    // A player that consumes standard input proves the pump and the stream
    // accounting without touching the session sound server.
    fn draining_player(destination: &std::path::Path) -> Command {
        let mut command = Command::new("sh");
        command.args(["-c", &format!("cat > '{}'", destination.display())]);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[test]
    fn documented_format_matches_the_stream_parameters() {
        assert_eq!(FORMAT, format!("s16le {} kHz stereo", SAMPLE_RATE / 1000));
        assert_eq!(CHANNELS, 2);
        assert_eq!(BYTES_PER_SECOND, 192_000);
        assert_eq!(duration_of(BYTES_PER_SECOND), Duration::from_secs(1));
        assert_eq!(duration_of(0), Duration::ZERO);
    }

    #[test]
    fn every_byte_reaches_the_player() {
        let directory = tempfile::tempdir().unwrap();
        let received = directory.path().join("received.pcm");
        let pcm: Vec<u8> = (0..BYTES_PER_SECOND)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect();
        let bytes = play_with(draining_player(&received), Cursor::new(pcm.clone())).unwrap();
        assert_eq!(bytes, BYTES_PER_SECOND);
        assert_eq!(fs::read(&received).unwrap(), pcm);
    }

    #[test]
    fn a_player_that_refuses_the_stream_is_reported() {
        let mut command = Command::new("false");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let error = play_with(command, Cursor::new(vec![0_u8; 4])).unwrap_err();
        let SinkError::Player(status) = error else {
            panic!("expected the player status, got {error}");
        };
        assert!(!status.success());
    }

    #[test]
    fn an_empty_stream_is_a_complete_stream() {
        let directory = tempfile::tempdir().unwrap();
        let received = directory.path().join("received.pcm");
        let bytes = play_with(draining_player(&received), Cursor::new(Vec::new())).unwrap();
        assert_eq!(bytes, 0);
        assert_eq!(fs::read(&received).unwrap(), Vec::<u8>::new());
    }
}
