//! Versioned PCM packets shared by a future kernel endpoint and the router.
//! Explicit little-endian serialization: no native structs or user pointers cross the boundary.
#![no_std]

pub const VERSION: u16 = 1;
pub const HEADER_BYTES: usize = 32;
pub const MAX_FRAMES: u32 = 4800;
pub const DISCONTINUITY: u32 = 1;
pub const SILENT: u32 = 2;
const MAGIC: [u8; 4] = *b"MXPC";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Encoding {
    Pcm16 = 1,
    Float32 = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub encoding: Encoding,
}

impl PcmFormat {
    /// The first endpoint milestone deliberately exposes only stereo at 48 kHz.
    pub fn validate(self) -> Result<(), Error> {
        if self.sample_rate != 48000 || self.channels != 2 {
            return Err(Error::Format);
        }
        Ok(())
    }
    pub fn sample_bytes(self) -> usize {
        match self.encoding {
            Encoding::Pcm16 => 2,
            Encoding::Float32 => 4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub format: PcmFormat,
    pub frames: u32,
    /// Packet sequence, wrapping at u64::MAX. Restart sets DISCONTINUITY.
    pub sequence: u64,
    pub flags: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Truncated,
    Magic,
    Version,
    HeaderSize,
    Format,
    Frames,
    Flags,
    Length,
    Destination,
}

impl Header {
    fn validate(self) -> Result<(), Error> {
        self.format.validate()?;
        if self.frames == 0 || self.frames > MAX_FRAMES {
            return Err(Error::Frames);
        }
        if self.flags & !(SILENT | DISCONTINUITY) != 0 {
            return Err(Error::Flags);
        }
        Ok(())
    }

    pub fn payload_bytes(self) -> Result<usize, Error> {
        self.validate()?;
        if self.flags & SILENT != 0 {
            return Ok(0);
        }
        Ok(self.frames as usize * self.format.channels as usize * self.format.sample_bytes())
    }

    pub fn write(self, destination: &mut [u8]) -> Result<(), Error> {
        self.validate()?;
        let out = destination
            .get_mut(..HEADER_BYTES)
            .ok_or(Error::Destination)?;
        out[..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&VERSION.to_le_bytes());
        out[6..8].copy_from_slice(&(HEADER_BYTES as u16).to_le_bytes());
        out[8..12].copy_from_slice(&self.format.sample_rate.to_le_bytes());
        out[12..14].copy_from_slice(&self.format.channels.to_le_bytes());
        out[14..16].copy_from_slice(&(self.format.encoding as u16).to_le_bytes());
        out[16..20].copy_from_slice(&self.frames.to_le_bytes());
        out[20..28].copy_from_slice(&self.sequence.to_le_bytes());
        out[28..32].copy_from_slice(&self.flags.to_le_bytes());
        Ok(())
    }

    pub fn read(bytes: &[u8]) -> Result<Self, Error> {
        let b = bytes.get(..HEADER_BYTES).ok_or(Error::Truncated)?;
        if b[..4] != MAGIC {
            return Err(Error::Magic);
        }
        if u16::from_le_bytes([b[4], b[5]]) != VERSION {
            return Err(Error::Version);
        }
        if u16::from_le_bytes([b[6], b[7]]) as usize != HEADER_BYTES {
            return Err(Error::HeaderSize);
        }
        let encoding = match u16::from_le_bytes([b[14], b[15]]) {
            1 => Encoding::Pcm16,
            3 => Encoding::Float32,
            _ => return Err(Error::Format),
        };
        let header = Self {
            format: PcmFormat {
                sample_rate: u32::from_le_bytes(b[8..12].try_into().unwrap()),
                channels: u16::from_le_bytes([b[12], b[13]]),
                encoding,
            },
            frames: u32::from_le_bytes(b[16..20].try_into().unwrap()),
            sequence: u64::from_le_bytes(b[20..28].try_into().unwrap()),
            flags: u32::from_le_bytes(b[28..32].try_into().unwrap()),
        };
        header.validate()?;
        Ok(header)
    }
}

pub struct Packet<'a> {
    pub header: Header,
    payload: &'a [u8],
}

impl<'a> Packet<'a> {
    /// One complete packet, not an arbitrary stream fragment.
    pub fn read(bytes: &'a [u8]) -> Result<Self, Error> {
        let header = Header::read(bytes)?;
        let payload = &bytes[HEADER_BYTES..];
        if payload.len() != header.payload_bytes()? {
            return Err(Error::Length);
        }
        Ok(Self { header, payload })
    }

    /// Convert into the engine's interleaved float format; sanitize nonfinite samples.
    pub fn copy_samples(&self, destination: &mut [f32]) -> Result<(), Error> {
        if destination.len() != self.header.frames as usize * self.header.format.channels as usize {
            return Err(Error::Destination);
        }
        if self.header.flags & SILENT != 0 {
            destination.fill(0.0);
            return Ok(());
        }
        match self.header.format.encoding {
            Encoding::Pcm16 => {
                for (out, bytes) in destination.iter_mut().zip(self.payload.as_chunks::<2>().0) {
                    *out = i16::from_le_bytes(*bytes) as f32 / 32768.0;
                }
            }
            Encoding::Float32 => {
                for (out, bytes) in destination.iter_mut().zip(self.payload.as_chunks::<4>().0) {
                    let sample = f32::from_le_bytes(*bytes);
                    *out = if sample.is_finite() { sample } else { 0.0 };
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn header(encoding: Encoding) -> Header {
        Header {
            format: PcmFormat {
                sample_rate: 48000,
                channels: 2,
                encoding,
            },
            frames: 1,
            sequence: 7,
            flags: 0,
        }
    }
    #[test]
    fn round_trip_and_explicit_wire_layout() {
        let h = header(Encoding::Float32);
        let mut bytes = [0; HEADER_BYTES];
        h.write(&mut bytes).unwrap();
        assert_eq!(&bytes[..8], b"MXPC\x01\x00\x20\x00");
        assert_eq!(Header::read(&bytes), Ok(h));
        assert_eq!(h.payload_bytes(), Ok(8));
    }
    #[test]
    fn rejects_every_truncated_header_without_panicking() {
        for length in 0..HEADER_BYTES {
            assert_eq!(
                Header::read(&[0; HEADER_BYTES][..length]),
                Err(Error::Truncated)
            );
        }
    }
    #[test]
    fn rejects_unnegotiated_formats_versions_flags_and_sizes() {
        let h = header(Encoding::Pcm16);
        for (index, value, error) in [
            (0, 0, Error::Magic),
            (4, 2, Error::Version),
            (6, 31, Error::HeaderSize),
            (12, 8, Error::Format),
            (14, 99, Error::Format),
            (16, 0, Error::Frames),
            (28, 4, Error::Flags),
        ] {
            let mut bytes = [0; HEADER_BYTES];
            h.write(&mut bytes).unwrap();
            bytes[index] = value;
            assert_eq!(Header::read(&bytes), Err(error));
        }
        let mut huge = h;
        huge.frames = u32::MAX;
        assert_eq!(huge.payload_bytes(), Err(Error::Frames));
    }
    #[test]
    fn rejects_incomplete_and_extra_payload() {
        let mut bytes = [0; HEADER_BYTES + 5];
        header(Encoding::Pcm16).write(&mut bytes).unwrap();
        assert!(matches!(
            Packet::read(&bytes[..HEADER_BYTES + 3]),
            Err(Error::Length)
        ));
        assert!(matches!(Packet::read(&bytes), Err(Error::Length)));
    }
    #[test]
    fn pcm16_preserves_channel_order() {
        let mut bytes = [0; HEADER_BYTES + 4];
        header(Encoding::Pcm16).write(&mut bytes).unwrap();
        bytes[HEADER_BYTES..].copy_from_slice(&[0, 128, 255, 127]);
        let packet = Packet::read(&bytes).unwrap();
        let mut samples = [0.0; 2];
        packet.copy_samples(&mut samples).unwrap();
        assert_eq!(samples, [-1.0, 32767.0 / 32768.0]);
        assert_eq!(packet.copy_samples(&mut [0.0; 1]), Err(Error::Destination));
    }
    #[test]
    fn float_sanitizes_nan_and_silent_packets_have_no_payload() {
        let mut bytes = [0; HEADER_BYTES + 8];
        header(Encoding::Float32).write(&mut bytes).unwrap();
        bytes[HEADER_BYTES..HEADER_BYTES + 4].copy_from_slice(&f32::NAN.to_le_bytes());
        bytes[HEADER_BYTES + 4..].copy_from_slice(&0.25_f32.to_le_bytes());
        let mut samples = [1.0; 2];
        Packet::read(&bytes)
            .unwrap()
            .copy_samples(&mut samples)
            .unwrap();
        assert_eq!(samples, [0.0, 0.25]);
        let mut h = header(Encoding::Float32);
        h.flags = SILENT | DISCONTINUITY;
        h.write(&mut bytes).unwrap();
        Packet::read(&bytes[..HEADER_BYTES])
            .unwrap()
            .copy_samples(&mut samples)
            .unwrap();
        assert_eq!(samples, [0.0; 2]);
        assert!(matches!(Packet::read(&bytes), Err(Error::Length)));
    }
}
