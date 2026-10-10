//! Startup/recovery FD delegation only. No touch/key records pass through here.
use droidloom_composer_aidl::denial::DenialPresentationSink;
use droidloom_denial_ipc::SeqPacket;
use droidloom_denial_protocol::AndroidMessage;
use std::{
    os::fd::AsFd,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

pub(super) struct InputHandoff {
    generation: Arc<AtomicU64>,
    sink: DenialPresentationSink,
    path: PathBuf,
}

impl InputHandoff {
    pub fn new(sink: DenialPresentationSink, path: impl Into<PathBuf>) -> Self {
        Self {
            generation: Arc::new(AtomicU64::new(0)),
            sink,
            path: path.into(),
        }
    }
    pub fn request(&self, generation: u64) -> Result<(), String> {
        if generation == 0 || generation <= self.generation.load(Ordering::Acquire) {
            return Ok(());
        }
        self.generation.store(generation, Ordering::Release);
        let current = Arc::clone(&self.generation);
        let sink = self.sink.clone();
        let path = self.path.clone();
        std::thread::Builder::new()
            .name("droidloom-input-handoff".into())
            .spawn(move || {
                while current.load(Ordering::Acquire) == generation {
                    if let Ok(input) = SeqPacket::connect_nonblocking(&path) {
                        if current.load(Ordering::Acquire) != generation {
                            return;
                        }
                        match sink.socket().send_android(
                            &AndroidMessage::BindInputChannel {
                                generation,
                                wire: droidloom_input_protocol::PROTOCOL_MAJOR,
                            },
                            &[input.as_fd()],
                        ) {
                            Ok(()) => return, // SCM_RIGHTS owns the endpoint now; keep no duplicate.
                            Err(error) => {
                                eprintln!("Droidloom native input handoff: {error}");
                                return;
                            }
                        }
                    }
                    // Only while Android is starting/recovering; no idle worker remains.
                    std::thread::sleep(Duration::from_millis(250));
                }
            })
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}
