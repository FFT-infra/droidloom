//! Bounded `PipeWire` capture through the installed `GStreamer` plugin.

use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;

use crate::protocol::{self, FRAME_BYTES, HEIGHT, Kind, Status, WIDTH};

const FRAME_DEADLINE: Duration = Duration::from_secs(5);

fn failure(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

/// Check cancellation without consuming input or changing socket blocking mode.
pub fn ensure_consumer_connected(stream: &UnixStream) -> io::Result<()> {
    let mut byte = [0_u8; 1];
    match rustix::net::recv(
        stream,
        byte.as_mut_slice(),
        rustix::net::RecvFlags::PEEK | rustix::net::RecvFlags::DONTWAIT,
    ) {
        Ok((_, 0)) => Err(io::Error::new(
            io::ErrorKind::ConnectionAborted,
            "camera consumer disconnected",
        )),
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected data after camera open",
        )),
        Err(rustix::io::Errno::AGAIN) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn caps() -> gst::Caps {
    gst::Caps::builder("video/x-raw")
        .field("format", "RGBA")
        .field("width", i32::try_from(WIDTH).unwrap())
        .field("height", i32::try_from(HEIGHT).unwrap())
        .field("framerate", gst::Fraction::new(30, 1))
        .build()
}

/// Discover only unambiguous libcamera colour streams, without opening a sensor.
pub fn sources() -> io::Result<[Option<String>; 2]> {
    let provider =
        gst::DeviceProviderFactory::by_name("pipewiredeviceprovider").ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "PipeWire GStreamer device provider is missing",
            )
        })?;
    provider.start().map_err(failure)?;
    let mut sources = [None, None];
    let mut counts = [0_u32; 2];
    for device in provider.devices() {
        let Some(properties) = device.properties() else {
            continue;
        };
        if properties.get::<String>("device.api").ok().as_deref() != Some("libcamera")
            || device.caps().is_none_or(|c| !c.can_intersect(&caps()))
        {
            continue;
        }
        let index = match properties
            .get::<String>("api.libcamera.location")
            .ok()
            .as_deref()
        {
            Some("back") => 0,
            Some("front") => 1,
            _ => continue,
        };
        if let Ok(name) = properties.get::<String>("node.name") {
            counts[index] += 1;
            sources[index] = Some(name);
        }
    }
    provider.stop();
    for (index, count) in counts.iter().enumerate() {
        if *count != 1 {
            sources[index] = None;
        }
    }
    Ok(sources)
}

struct Pipeline(gst::Pipeline);

impl Drop for Pipeline {
    fn drop(&mut self) {
        // Stop the source before releasing its buffers. The libcamera simple
        // pipeline must cancel all pending requests; never keep a sensor warm.
        if let Err(error) = self.0.set_state(gst::State::Null) {
            eprintln!("droidloom-camera: stopping capture failed: {error}");
        }
    }
}

fn pipeline(source: &gst::Element) -> io::Result<(Pipeline, gst_app::AppSink)> {
    let pipeline = Pipeline(gst::Pipeline::new());
    let rate = gst::ElementFactory::make("videorate")
        .property("drop-only", true)
        .build()
        .map_err(failure)?;
    let sink = gst_app::AppSink::builder()
        .caps(&caps())
        .max_buffers(2)
        .drop(true)
        .sync(false)
        .wait_on_eos(false)
        .build();
    pipeline
        .0
        .add_many([source, &rate, sink.upcast_ref()])
        .map_err(failure)?;
    gst::Element::link_many([source, &rate, sink.upcast_ref()]).map_err(failure)?;
    Ok((pipeline, sink))
}

fn pack_sample(sample: &gst::Sample, output: &mut [u8]) -> io::Result<()> {
    let caps = sample
        .caps()
        .ok_or_else(|| failure("camera sample has no caps"))?;
    let info = gst_video::VideoInfo::from_caps(caps).map_err(failure)?;
    if info.width() != u32::try_from(WIDTH).unwrap()
        || info.height() != u32::try_from(HEIGHT).unwrap()
        || info.format() != gst_video::VideoFormat::Rgba
    {
        return Err(failure("camera changed its negotiated format"));
    }
    let buffer = sample
        .buffer()
        .ok_or_else(|| failure("camera sample has no buffer"))?;
    let frame =
        gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info).map_err(failure)?;
    let stride = usize::try_from(frame.plane_stride()[0]).map_err(failure)?;
    protocol::pack_rgba(frame.plane_data(0).map_err(failure)?, stride, output)
}

pub fn stream(stream: &mut UnixStream, source_name: &str) -> io::Result<()> {
    let source = gst::ElementFactory::make("pipewiresrc")
        .property("target-object", source_name)
        .build()
        .map_err(failure)?;
    stream_source(stream, &source)
}

fn stream_source(stream: &mut UnixStream, source: &gst::Element) -> io::Result<()> {
    ensure_consumer_connected(stream)?;
    let (pipeline, sink) = pipeline(source)?;
    protocol::write_packet(stream, Kind::Opened, Status::Ok, 0, &[])?;
    // The peer can cancel while waiting for a lease or during pipeline setup.
    // Neither a closed receive side nor a queued EOF may activate the sensor.
    ensure_consumer_connected(stream)?;
    pipeline.0.set_state(gst::State::Playing).map_err(failure)?;
    stream.set_read_timeout(Some(Duration::from_millis(1)))?;
    let clock = gst::SystemClock::obtain();
    let mut output = vec![0; FRAME_BYTES];
    let mut last_frame = Instant::now();
    let mut previous_timestamp = 0;
    loop {
        let mut unexpected = [0_u8; 1];
        match stream.read(&mut unexpected) {
            Ok(0) => return Ok(()),
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected data after camera open",
                ));
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return Ok(()),
            Err(e) => return Err(e),
        }
        if let Some(sample) = sink.try_pull_sample(gst::ClockTime::from_mseconds(100)) {
            pack_sample(&sample, &mut output)?;
            // This is monotonic host delivery time, not a claimed sensor clock.
            // Android and the host share CLOCK_MONOTONIC in the cell.
            let timestamp = clock.time().nseconds();
            if timestamp <= previous_timestamp {
                return Err(failure("camera delivery clock did not advance"));
            }
            if let Err(error) =
                protocol::write_packet(stream, Kind::Frame, Status::Ok, timestamp, &output)
            {
                return if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) {
                    Ok(())
                } else {
                    Err(error)
                };
            }
            previous_timestamp = timestamp;
            last_frame = Instant::now();
        } else if sink.is_eos() {
            return Err(failure("camera source ended"));
        } else if last_frame.elapsed() >= FRAME_DEADLINE {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "camera stopped delivering frames",
            ));
        }
        if let Some(message) = pipeline
            .0
            .bus()
            .and_then(|b| b.pop_filtered(&[gst::MessageType::Error]))
        {
            if let gst::MessageView::Error(error) = message.view() {
                return Err(failure(error.error()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Shutdown;

    #[test]
    fn consumer_check_is_nonblocking_and_does_not_consume_input() {
        use std::io::Write;

        let (mut server, mut client) = UnixStream::pair().unwrap();
        ensure_consumer_connected(&server).unwrap();
        client.write_all(b"unexpected").unwrap();
        assert_eq!(
            ensure_consumer_connected(&server).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        let mut bytes = [0; 10];
        server.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"unexpected");
        client.shutdown(Shutdown::Both).unwrap();
        assert_eq!(
            ensure_consumer_connected(&server).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
    }

    #[test]
    fn preparing_pipeline_does_not_start_capture() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .build()
            .expect("GStreamer base plugins are required for camera tests");
        let (prepared, _sink) = pipeline(&source).unwrap();
        let initial_state = source.current_state();
        prepared.0.set_state(gst::State::Playing).unwrap();
        let started_state = source.current_state();
        drop(prepared);
        assert_eq!(
            initial_state,
            gst::State::Null,
            "pipeline preparation started its source"
        );
        assert_eq!(
            started_state,
            gst::State::Playing,
            "positive activation control did not start"
        );
    }

    #[test]
    fn synthetic_capture_delivers_rgba_and_stops_when_consumer_closes() {
        gst::init().unwrap();
        let source = gst::ElementFactory::make("videotestsrc")
            .property("is-live", true)
            .property_from_str("pattern", "red")
            .build()
            .expect("GStreamer base plugins are required for camera tests");
        let (mut server, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        server
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let worker = std::thread::spawn(move || stream_source(&mut server, &source));
        let mut header = [0; 24];
        client.read_exact(&mut header).unwrap();
        assert_eq!(&header[..8], b"DCAR\x01\x00\x02\x00");
        client.read_exact(&mut header).unwrap();
        assert_eq!(&header[..8], b"DCAR\x01\x00\x03\x00");
        assert_eq!(
            u32::from_le_bytes(header[8..12].try_into().unwrap()) as usize,
            FRAME_BYTES
        );
        assert_ne!(u64::from_le_bytes(header[16..24].try_into().unwrap()), 0);
        let mut frame = vec![0; FRAME_BYTES];
        client.read_exact(&mut frame).unwrap();
        assert!(frame.chunks_exact(4).all(|pixel| pixel == [255, 0, 0, 255]));
        client.shutdown(Shutdown::Both).unwrap();
        let result = worker.join().unwrap();
        assert!(result.is_ok(), "capture shutdown: {result:?}");
    }
}
