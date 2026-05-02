use std::io::{self, Read, Write};

pub const PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Content = 0,
    Attach = 1,
    Detach = 2,
    Resize = 3,
    Exit = 4,
    Pid = 5,
}

impl TryFrom<u8> for PacketType {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Content),
            1 => Ok(Self::Attach),
            2 => Ok(Self::Detach),
            3 => Ok(Self::Resize),
            4 => Ok(Self::Exit),
            5 => Ok(Self::Pid),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown packet type: {value}"),
            )),
        }
    }
}

/// Wire format: [version: u8][type: u8][len: u32 big-endian][payload: len bytes]
#[derive(Debug, Clone)]
pub struct Packet {
    pub packet_type: PacketType,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn new(packet_type: PacketType, payload: Vec<u8>) -> Self {
        Self {
            packet_type,
            payload,
        }
    }

    pub fn content(data: &[u8]) -> Self {
        Self::new(PacketType::Content, data.to_vec())
    }

    pub fn attach() -> Self {
        Self::new(PacketType::Attach, vec![])
    }

    pub fn detach() -> Self {
        Self::new(PacketType::Detach, vec![])
    }

    pub fn resize(cols: u16, rows: u16) -> Self {
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&cols.to_be_bytes());
        payload.extend_from_slice(&rows.to_be_bytes());
        Self::new(PacketType::Resize, payload)
    }

    pub fn exit(status: i32) -> Self {
        Self::new(PacketType::Exit, status.to_be_bytes().to_vec())
    }

    pub fn parse_resize(&self) -> Option<(u16, u16)> {
        if self.payload.len() < 4 {
            return None;
        }
        let cols = u16::from_be_bytes([self.payload[0], self.payload[1]]);
        let rows = u16::from_be_bytes([self.payload[2], self.payload[3]]);
        Some((cols, rows))
    }

    pub fn parse_exit_status(&self) -> Option<i32> {
        if self.payload.len() < 4 {
            return None;
        }
        Some(i32::from_be_bytes([
            self.payload[0],
            self.payload[1],
            self.payload[2],
            self.payload[3],
        ]))
    }

    pub fn write_to<W: Write>(&self, writer: &mut W) -> io::Result<()> {
        let len = self.payload.len() as u32;
        writer.write_all(&[PROTOCOL_VERSION])?;
        writer.write_all(&[self.packet_type as u8])?;
        writer.write_all(&len.to_be_bytes())?;
        writer.write_all(&self.payload)?;
        writer.flush()
    }

    pub fn read_from<R: Read>(reader: &mut R) -> io::Result<Self> {
        let mut header = [0u8; 6];
        reader.read_exact(&mut header)?;

        let version = header[0];
        if version != PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("protocol version mismatch: expected {PROTOCOL_VERSION}, got {version}"),
            ));
        }

        let packet_type = PacketType::try_from(header[1])?;
        let len = u32::from_be_bytes([header[2], header[3], header[4], header[5]]) as usize;

        const MAX_PAYLOAD: usize = 64 * 1024;
        if len > MAX_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("payload too large: {len} bytes"),
            ));
        }

        let mut payload = vec![0u8; len];
        if len > 0 {
            reader.read_exact(&mut payload)?;
        }

        Ok(Self {
            packet_type,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_packet_roundtrip_content() {
        let packet = Packet::content(b"hello world");
        let mut buffer = Vec::new();
        packet.write_to(&mut buffer).unwrap();

        let mut cursor = Cursor::new(buffer);
        let decoded = Packet::read_from(&mut cursor).unwrap();

        assert_eq!(decoded.packet_type, PacketType::Content);
        assert_eq!(decoded.payload, b"hello world");
    }

    #[test]
    fn test_packet_roundtrip_resize() {
        let packet = Packet::resize(120, 40);
        let mut buffer = Vec::new();
        packet.write_to(&mut buffer).unwrap();

        let mut cursor = Cursor::new(buffer);
        let decoded = Packet::read_from(&mut cursor).unwrap();

        assert_eq!(decoded.packet_type, PacketType::Resize);
        let (cols, rows) = decoded.parse_resize().unwrap();
        assert_eq!(cols, 120);
        assert_eq!(rows, 40);
    }

    #[test]
    fn test_packet_roundtrip_exit() {
        let packet = Packet::exit(42);
        let mut buffer = Vec::new();
        packet.write_to(&mut buffer).unwrap();

        let mut cursor = Cursor::new(buffer);
        let decoded = Packet::read_from(&mut cursor).unwrap();

        assert_eq!(decoded.packet_type, PacketType::Exit);
        assert_eq!(decoded.parse_exit_status(), Some(42));
    }

    #[test]
    fn test_packet_roundtrip_empty() {
        let packet = Packet::attach();
        let mut buffer = Vec::new();
        packet.write_to(&mut buffer).unwrap();

        let mut cursor = Cursor::new(buffer);
        let decoded = Packet::read_from(&mut cursor).unwrap();

        assert_eq!(decoded.packet_type, PacketType::Attach);
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn test_bad_version() {
        let data = [0xFF, 0x00, 0x00, 0x00, 0x00, 0x00];
        let mut cursor = Cursor::new(data);
        let result = Packet::read_from(&mut cursor);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("version mismatch"));
    }

    #[test]
    fn test_bad_packet_type() {
        let data = [PROTOCOL_VERSION, 0xFF, 0x00, 0x00, 0x00, 0x00];
        let mut cursor = Cursor::new(data);
        let result = Packet::read_from(&mut cursor);
        assert!(result.is_err());
    }

    #[test]
    fn test_payload_too_large() {
        // 128KB payload length
        let data = [PROTOCOL_VERSION, 0x00, 0x00, 0x02, 0x00, 0x00];
        let mut cursor = Cursor::new(data);
        let result = Packet::read_from(&mut cursor);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("too large"));
    }

    #[test]
    fn test_multiple_packets_in_stream() {
        let mut buffer = Vec::new();
        Packet::content(b"first").write_to(&mut buffer).unwrap();
        Packet::content(b"second").write_to(&mut buffer).unwrap();
        Packet::detach().write_to(&mut buffer).unwrap();

        let mut cursor = Cursor::new(buffer);

        let p1 = Packet::read_from(&mut cursor).unwrap();
        assert_eq!(p1.payload, b"first");

        let p2 = Packet::read_from(&mut cursor).unwrap();
        assert_eq!(p2.payload, b"second");

        let p3 = Packet::read_from(&mut cursor).unwrap();
        assert_eq!(p3.packet_type, PacketType::Detach);
    }
}
