//! Fixed-size camera transport shared with the Android camera producer.

use std::io::{self, Read, Write};

pub const WIDTH: usize = 640;
pub const HEIGHT: usize = 480;
pub const ROW_BYTES: usize = WIDTH * 4;
pub const FRAME_BYTES: usize = ROW_BYTES * HEIGHT;
pub const VERSION: u16 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Catalogue,
    Open(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    Catalogue = 1,
    Opened = 2,
    Frame = 3,
    Error = 4,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Status {
    Ok = 0,
    Protocol = 1,
    Unavailable = 2,
    Busy = 3,
    Capture = 4,
    Timeout = 5,
}

pub fn read_request(reader: &mut impl Read) -> io::Result<Request> {
    let mut bytes = [0_u8; 16];
    reader.read_exact(&mut bytes)?;
    if &bytes[..4] != b"DCAM"
        || u16::from_le_bytes(bytes[4..6].try_into().unwrap()) != VERSION
        || bytes[12..] != [0; 4]
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid camera request header",
        ));
    }
    let camera = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
    match u16::from_le_bytes(bytes[6..8].try_into().unwrap()) {
        1 if camera == 0 => Ok(Request::Catalogue),
        2 if camera < 2 => Ok(Request::Open(camera)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported camera request",
        )),
    }
}

pub fn write_packet(
    writer: &mut impl Write,
    kind: Kind,
    status: Status,
    timestamp_ns: u64,
    payload: &[u8],
) -> io::Result<()> {
    let valid = match kind {
        Kind::Catalogue => status == Status::Ok && timestamp_ns == 0 && payload.len() == 4,
        Kind::Opened => status == Status::Ok && timestamp_ns == 0 && payload.is_empty(),
        Kind::Frame => status == Status::Ok && timestamp_ns != 0 && payload.len() == FRAME_BYTES,
        Kind::Error => status != Status::Ok && timestamp_ns == 0 && payload.is_empty(),
    };
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid camera response",
        ));
    }
    let mut header = [0_u8; 24];
    header[..4].copy_from_slice(b"DCAR");
    header[4..6].copy_from_slice(&VERSION.to_le_bytes());
    header[6..8].copy_from_slice(&(kind as u16).to_le_bytes());
    header[8..12].copy_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
    header[12..16].copy_from_slice(&(status as u32).to_le_bytes());
    header[16..24].copy_from_slice(&timestamp_ns.to_le_bytes());
    writer.write_all(&header)?;
    writer.write_all(payload)
}

/// Strip source row padding; a `GstVideoMeta` stride need not be tightly packed.
pub fn pack_rgba(source: &[u8], stride: usize, destination: &mut [u8]) -> io::Result<()> {
    let required = stride
        .checked_mul(HEIGHT - 1)
        .and_then(|v| v.checked_add(ROW_BYTES));
    if stride < ROW_BYTES
        || required.is_none_or(|size| size > source.len())
        || destination.len() != FRAME_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid RGBA frame layout",
        ));
    }
    for (row, out) in destination.chunks_exact_mut(ROW_BYTES).enumerate() {
        out.copy_from_slice(&source[row * stride..row * stride + ROW_BYTES]);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(op: u16, camera: u32) -> Vec<u8> {
        [
            b"DCAM".as_slice(),
            &VERSION.to_le_bytes(),
            &op.to_le_bytes(),
            &camera.to_le_bytes(),
            &[0; 4],
        ]
        .concat()
    }

    #[test]
    fn requests_are_exact_and_bounded() {
        assert_eq!(
            read_request(&mut request(1, 0).as_slice()).unwrap(),
            Request::Catalogue
        );
        assert_eq!(
            read_request(&mut request(2, 1).as_slice()).unwrap(),
            Request::Open(1)
        );
        for bytes in [
            request(1, 1),
            request(2, 2),
            request(3, 0),
            vec![0; 16],
            request(2, 0)[..15].to_vec(),
        ] {
            assert!(read_request(&mut bytes.as_slice()).is_err());
        }
        let mut bytes = request(2, 0);
        bytes[4] = 2;
        assert!(read_request(&mut bytes.as_slice()).is_err());
        bytes = request(2, 0);
        bytes[15] = 1;
        assert!(read_request(&mut bytes.as_slice()).is_err());
    }

    #[test]
    fn response_matches_android_wire_layout() {
        let mut packet = Vec::new();
        write_packet(
            &mut packet,
            Kind::Catalogue,
            Status::Ok,
            0,
            &3_u32.to_le_bytes(),
        )
        .unwrap();
        assert_eq!(
            &packet[..16],
            b"DCAR\x01\x00\x01\x00\x04\x00\x00\x00\x00\x00\x00\x00"
        );
        assert_eq!(&packet[16..], &[0, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0]);
        assert!(write_packet(&mut packet, Kind::Frame, Status::Ok, 1, &[0; 4]).is_err());
        assert!(write_packet(&mut packet, Kind::Error, Status::Ok, 0, &[]).is_err());
        let frame = vec![0; FRAME_BYTES];
        packet.clear();
        write_packet(&mut packet, Kind::Frame, Status::Ok, 123, &frame).unwrap();
        assert_eq!(packet.len(), 24 + FRAME_BYTES);
        assert_eq!(&packet[16..24], &123_u64.to_le_bytes());
        packet.clear();
        write_packet(&mut packet, Kind::Error, Status::Busy, 0, &[]).unwrap();
        assert_eq!(&packet[12..16], &3_u32.to_le_bytes());
    }

    #[test]
    fn padded_rows_preserve_order_without_leaking_padding() {
        let stride = ROW_BYTES + 16;
        let mut source = vec![0xfe; stride * HEIGHT];
        for row in 0..HEIGHT {
            source[row * stride..row * stride + ROW_BYTES].fill(u8::try_from(row % 251).unwrap());
        }
        let mut packed = vec![0; FRAME_BYTES];
        pack_rgba(&source, stride, &mut packed).unwrap();
        for (row, bytes) in packed.chunks_exact(ROW_BYTES).enumerate() {
            assert!(bytes.iter().all(|b| *b == u8::try_from(row % 251).unwrap()));
        }
        assert!(pack_rgba(&source[..ROW_BYTES], stride, &mut packed).is_err());
        assert!(pack_rgba(&source, ROW_BYTES - 1, &mut packed).is_err());
        assert!(pack_rgba(&source, usize::MAX, &mut packed).is_err());
        assert!(pack_rgba(&source, stride, &mut packed[..FRAME_BYTES - 1]).is_err());
    }
}
