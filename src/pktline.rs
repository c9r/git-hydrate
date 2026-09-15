//! Git's pkt-line framing, which the long-running filter protocol speaks.
//!
//! A packet is a four-digit lowercase hex length that counts itself, followed
//! by that many bytes less four. A length of zero is a flush packet, which
//! terminates a list or a body of content.

use std::io::{self, Read, Write};

/// The most payload one packet carries, which is the 65520-byte packet limit less its length prefix.
pub const MAX_PAYLOAD: usize = 65516;

/// One packet read from the stream.
pub enum Packet {
    /// A flush packet, `0000`.
    Flush,
    /// A data packet whose payload now fills the caller's buffer.
    Data,
}

/// Reads one packet, filling `buf` with the payload of a data packet.
///
/// Returns `None` at a clean end of stream, meaning git closed the pipe
/// between packets, which is how the filter learns it should exit.
pub fn read_packet<R: Read>(reader: &mut R, buf: &mut Vec<u8>) -> io::Result<Option<Packet>> {
    let mut prefix = [0u8; 4];
    match reader.read_exact(&mut prefix) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let text = std::str::from_utf8(&prefix).map_err(|_| invalid("packet length is not hex"))?;
    let len = usize::from_str_radix(text, 16).map_err(|_| invalid("packet length is not hex"))?;
    if len == 0 {
        return Ok(Some(Packet::Flush));
    }
    if len < 4 {
        return Err(invalid("packet length below four is not a data packet"));
    }
    let payload = len - 4;
    if payload > MAX_PAYLOAD {
        return Err(invalid("packet exceeds the maximum length"));
    }
    buf.resize(payload, 0);
    reader.read_exact(buf)?;
    Ok(Some(Packet::Data))
}

/// Reads a list of text packets up to and including its flush packet, with
/// each packet's trailing newline removed. Returns `None` at end of stream.
pub fn read_text_list<R: Read>(reader: &mut R, buf: &mut Vec<u8>) -> io::Result<Option<Vec<String>>> {
    let mut list = Vec::new();
    loop {
        match read_packet(reader, buf)? {
            None => {
                if list.is_empty() {
                    return Ok(None);
                }
                return Err(invalid("stream ended inside a list"));
            }
            Some(Packet::Flush) => return Ok(Some(list)),
            Some(Packet::Data) => {
                let mut text = String::from_utf8(buf.clone()).map_err(|_| invalid("text packet is not UTF-8"))?;
                if text.ends_with('\n') {
                    text.pop();
                }
                list.push(text);
            }
        }
    }
}

/// Writes one data packet.
pub fn write_packet<W: Write>(writer: &mut W, payload: &[u8]) -> io::Result<()> {
    debug_assert!(payload.len() <= MAX_PAYLOAD);
    write!(writer, "{:04x}", payload.len() + 4)?;
    writer.write_all(payload)
}

/// Writes a text packet, which is the text plus a trailing newline.
pub fn write_text<W: Write>(writer: &mut W, text: &str) -> io::Result<()> {
    let mut line = Vec::with_capacity(text.len() + 1);
    line.extend_from_slice(text.as_bytes());
    line.push(b'\n');
    write_packet(writer, &line)
}

/// Writes a flush packet.
pub fn write_flush<W: Write>(writer: &mut W) -> io::Result<()> {
    writer.write_all(b"0000")
}

/// Writes content as a run of data packets, without the terminating flush.
pub fn write_content<W: Write>(writer: &mut W, content: &[u8]) -> io::Result<()> {
    for chunk in content.chunks(MAX_PAYLOAD) {
        write_packet(writer, chunk)?;
    }
    Ok(())
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_packets() {
        let mut out = Vec::new();
        write_text(&mut out, "command=clean").unwrap();
        write_flush(&mut out).unwrap();
        write_content(&mut out, &vec![7u8; MAX_PAYLOAD + 1]).unwrap();
        write_flush(&mut out).unwrap();
        assert!(out.starts_with(b"0012command=clean\n0000"));

        let mut reader = out.as_slice();
        let mut buf = Vec::new();
        assert_eq!(read_text_list(&mut reader, &mut buf).unwrap(), Some(vec!["command=clean".to_string()]));
        assert!(matches!(read_packet(&mut reader, &mut buf).unwrap(), Some(Packet::Data)));
        assert_eq!(buf.len(), MAX_PAYLOAD);
        assert!(matches!(read_packet(&mut reader, &mut buf).unwrap(), Some(Packet::Data)));
        assert_eq!(buf, vec![7u8]);
        assert!(matches!(read_packet(&mut reader, &mut buf).unwrap(), Some(Packet::Flush)));
        assert!(read_packet(&mut reader, &mut buf).unwrap().is_none());
        assert!(read_text_list(&mut reader, &mut buf).unwrap().is_none());
    }

    #[test]
    fn rejects_malformed_lengths() {
        let mut buf = Vec::new();
        assert!(read_packet(&mut &b"zzzz"[..], &mut buf).is_err());
        assert!(read_packet(&mut &b"0003"[..], &mut buf).is_err());
        assert!(read_packet(&mut &b"fff5"[..], &mut buf).is_err());
    }
}
