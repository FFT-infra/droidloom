//! Bounded streaming clipboard transport. Payloads never enter diagnostics.
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Read, Write},
    sync::Arc,
};
use tempfile::NamedTempFile;

pub const ABI: u32 = 1;
pub const MAX_TEXT: usize = 1024 * 1024;
pub const MAX_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_FILES: usize = 64;
const MAX_FRAME: usize = 3 * MAX_TEXT + 65536;
const CHUNK: usize = 65536;

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Blob {
    pub mime: String,
    pub name: String,
    pub size: u64,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Description {
    pub id: String,
    pub base: u64,
    pub text: Option<String>,
    pub html: Option<String>,
    #[serde(default)]
    pub blobs: Vec<Blob>,
}
impl Description {
    pub fn validate(&self) -> io::Result<()> {
        if self.id.is_empty()
            || self.id.len() > 96
            || !self
                .id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || self.text.as_ref().is_some_and(|s| s.len() > MAX_TEXT)
            || self.html.as_ref().is_some_and(|s| s.len() > MAX_TEXT)
            || self.blobs.len() > MAX_FILES
        {
            return Err(invalid());
        }
        let mut total = 0u64;
        for b in &self.blobs {
            total = total.checked_add(b.size).ok_or_else(invalid)?;
            if total > MAX_BYTES
                || b.name.len() > 255
                || b.mime.len() > 128
                || !b.mime.contains('/')
                || b.mime.chars().any(char::is_control)
            {
                return Err(invalid());
            }
        }
        Ok(())
    }
    pub fn empty(&self) -> bool {
        self.text.is_none() && self.html.is_none() && self.blobs.is_empty()
    }
}
#[derive(Clone)]
pub struct Clip {
    pub description: Description,
    pub files: Vec<Arc<NamedTempFile>>,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Message {
    Hello { abi: u32 },
    Sync { revision: u64 },
    Begin { clip: Description },
    End { id: String },
    Ack { id: String },
}
pub enum Received {
    Message(Message),
    Clip(Clip),
}
pub fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid clipboard record")
}

pub fn send_message(w: &mut impl Write, m: &Message) -> io::Result<()> {
    let bytes = serde_json::to_vec(m)?;
    frame(w, 1, &bytes)
}
fn frame(w: &mut impl Write, kind: u8, bytes: &[u8]) -> io::Result<()> {
    if bytes.len() + 1 > MAX_FRAME {
        return Err(invalid());
    }
    w.write_all(&((bytes.len() + 1) as u32).to_be_bytes())?;
    w.write_all(&[kind])?;
    w.write_all(bytes)
}
pub fn send_clip(w: &mut impl Write, clip: &Clip) -> io::Result<()> {
    clip.description.validate()?;
    if clip.files.len() != clip.description.blobs.len() {
        return Err(invalid());
    }
    send_message(
        w,
        &Message::Begin {
            clip: clip.description.clone(),
        },
    )?;
    let mut buf = vec![0u8; CHUNK + 4];
    for (index, file) in clip.files.iter().enumerate() {
        buf[..4].copy_from_slice(&(index as u32).to_be_bytes());
        let mut input = File::open(file.path())?.take(clip.description.blobs[index].size);
        let mut total = 0;
        loop {
            let n = input.read(&mut buf[4..])?;
            if n == 0 {
                break;
            }
            total += n as u64;
            frame(w, 2, &buf[..4 + n])?;
        }
        if total != clip.description.blobs[index].size {
            return Err(invalid());
        }
    }
    send_message(
        w,
        &Message::End {
            id: clip.description.id.clone(),
        },
    )
}

pub struct Receiver {
    pending: Option<(Description, Vec<NamedTempFile>, Vec<u64>)>,
    directory: std::path::PathBuf,
}
impl Receiver {
    pub fn new(directory: std::path::PathBuf) -> Self {
        Self {
            pending: None,
            directory,
        }
    }
    pub fn read(&mut self, r: &mut impl Read) -> io::Result<Option<Received>> {
        let mut length = [0; 4];
        r.read_exact(&mut length)?;
        let length = u32::from_be_bytes(length) as usize;
        if length == 0 || length > MAX_FRAME {
            return Err(invalid());
        }
        let mut bytes = vec![0; length];
        r.read_exact(&mut bytes)?;
        if bytes[0] == 2 {
            if bytes.len() < 5 || bytes.len() > CHUNK + 5 {
                return Err(invalid());
            }
            let index = u32::from_be_bytes(bytes[1..5].try_into().unwrap()) as usize;
            let (desc, files, counts) = self.pending.as_mut().ok_or_else(invalid)?;
            if index >= files.len() {
                return Err(invalid());
            }
            let count = counts[index]
                .checked_add((bytes.len() - 5) as u64)
                .ok_or_else(invalid)?;
            if count > desc.blobs[index].size {
                return Err(invalid());
            }
            files[index].write_all(&bytes[5..])?;
            counts[index] = count;
            return Ok(None);
        }
        if bytes[0] != 1 {
            return Err(invalid());
        }
        let message: Message = serde_json::from_slice(&bytes[1..])?;
        match message {
            Message::Begin { clip } => {
                clip.validate()?;
                let files: io::Result<Vec<_>> = clip
                    .blobs
                    .iter()
                    .map(|b| {
                        let name = safe_name(&b.name);
                        tempfile::Builder::new()
                            .prefix("clip-")
                            .suffix(&format!("-{name}"))
                            .tempfile_in(&self.directory)
                    })
                    .collect();
                let counts = vec![0; clip.blobs.len()];
                self.pending = Some((clip, files?, counts));
                Ok(None)
            }
            Message::End { id } => {
                let (description, files, counts) = self.pending.take().ok_or_else(invalid)?;
                if id != description.id
                    || counts
                        .iter()
                        .zip(&description.blobs)
                        .any(|(n, b)| *n != b.size)
                {
                    return Err(invalid());
                }
                Ok(Some(Received::Clip(Clip {
                    description,
                    files: files.into_iter().map(Arc::new).collect(),
                })))
            }
            other => Ok(Some(Received::Message(other))),
        }
    }
}
pub fn safe_name(name: &str) -> String {
    let s: String = name
        .chars()
        .filter(|c| !c.is_control() && *c != '/' && *c != '\\')
        .take(100)
        .collect();
    if s.is_empty() || s == "." || s == ".." {
        "clipboard.bin".into()
    } else {
        s
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_html_and_binary_round_trip_with_bounded_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
        let data: Vec<u8> = (0..200_000).map(|i| (i % 251) as u8).collect();
        f.write_all(&data).unwrap();
        let clip = Clip {
            description: Description {
                id: "test-1".into(),
                base: 3,
                text: Some("héllo\n世界".into()),
                html: Some("<b>héllo</b>".into()),
                blobs: vec![Blob {
                    mime: "image/png".into(),
                    name: "test.png".into(),
                    size: data.len() as u64,
                }],
            },
            files: vec![Arc::new(f)],
        };
        let mut bytes = Vec::new();
        send_clip(&mut bytes, &clip).unwrap();
        let mut reader = Receiver::new(dir.path().into());
        let mut input = &bytes[..];
        loop {
            if let Some(Received::Clip(got)) = reader.read(&mut input).unwrap() {
                assert_eq!(got.description, clip.description);
                assert_eq!(std::fs::read(got.files[0].path()).unwrap(), data);
                break;
            }
        }
    }
    #[test]
    fn partial_and_oversized_payloads_fail_without_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let mut r = Receiver::new(dir.path().into());
        let mut bytes = Vec::new();
        send_message(
            &mut bytes,
            &Message::Begin {
                clip: Description {
                    id: "x".into(),
                    blobs: vec![Blob {
                        mime: "a/b".into(),
                        name: "x".into(),
                        size: 4,
                    }],
                    ..Default::default()
                },
            },
        )
        .unwrap();
        send_message(&mut bytes, &Message::End { id: "x".into() }).unwrap();
        let mut input = &bytes[..];
        assert!(r.read(&mut input).unwrap().is_none());
        assert!(r.read(&mut input).is_err());
        assert!(r.read(&mut &u32::MAX.to_be_bytes()[..]).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
