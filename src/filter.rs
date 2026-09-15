//! The long-running filter process git runs for every tracked path.
//!
//! Git starts one process per git command, performs a handshake, and then
//! sends each file to clean or smudge as a list of key-value packets followed
//! by the content. The filter answers with a status, the result, and a second
//! status list. Clean hashes the content and answers with its pointer. A
//! pointer passes through clean unchanged. Smudge answers with exactly what it
//! was given, so a checkout writes pointers and never downloads.
//!
//! The filter never touches the network and never stores anything, which is
//! the property the whole tool is built on.

use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};

use anyhow::{Context, Result, bail};

use crate::hash::Hasher;
use crate::pktline::{self, Packet};
use crate::pointer::{MAX_LEN, Pointer};

/// Smudged content larger than this is spooled to a temporary file instead of memory.
const SPOOL_THRESHOLD: usize = 64 << 20;

/// Runs the filter over the process's standard streams until git closes them.
pub fn run<R: Read, W: Write>(input: R, output: W, on_start: impl FnOnce()) -> Result<()> {
    let mut reader = BufReader::with_capacity(1 << 16, input);
    let mut writer = BufWriter::with_capacity(1 << 16, output);
    let mut buf = Vec::with_capacity(pktline::MAX_PAYLOAD);

    handshake(&mut reader, &mut writer, &mut buf)?;
    on_start();

    while let Some(header) = pktline::read_text_list(&mut reader, &mut buf)? {
        let command = value_of(&header, "command");
        let pathname = value_of(&header, "pathname").unwrap_or("");
        match command {
            Some("clean") => {
                clean(&mut reader, &mut writer, &mut buf).with_context(|| format!("cleaning {pathname}"))?
            }
            Some("smudge") => {
                smudge(&mut reader, &mut writer, &mut buf).with_context(|| format!("smudging {pathname}"))?
            }
            _ => {
                drain_content(&mut reader, &mut buf)?;
                pktline::write_text(&mut writer, "status=error")?;
                pktline::write_flush(&mut writer)?;
                writer.flush()?;
            }
        }
    }
    Ok(())
}

fn handshake<R: Read, W: Write>(reader: &mut R, writer: &mut W, buf: &mut Vec<u8>) -> Result<()> {
    let welcome = pktline::read_text_list(reader, buf)?.context("git closed the filter before the handshake")?;
    if welcome.first().map(String::as_str) != Some("git-filter-client") {
        bail!("expected git-filter-client, got {welcome:?}");
    }
    if !welcome.iter().any(|line| line == "version=2") {
        bail!("git offered no supported filter protocol version: {welcome:?}");
    }
    pktline::write_text(writer, "git-filter-server")?;
    pktline::write_text(writer, "version=2")?;
    pktline::write_flush(writer)?;
    writer.flush()?;

    let offered = pktline::read_text_list(reader, buf)?.context("git closed the filter during the handshake")?;
    for capability in ["clean", "smudge"] {
        if offered.iter().any(|line| line == &format!("capability={capability}")) {
            pktline::write_text(writer, &format!("capability={capability}"))?;
        }
    }
    pktline::write_flush(writer)?;
    writer.flush()?;
    Ok(())
}

fn value_of<'a>(list: &'a [String], key: &str) -> Option<&'a str> {
    list.iter().find_map(|line| line.strip_prefix(key).and_then(|rest| rest.strip_prefix('=')))
}

/// Reads content until its flush and answers with the pointer for it.
///
/// The first kilobyte is held back until the content is known to be longer
/// than a pointer, because content that short and shaped like a pointer is
/// one and passes through unchanged.
fn clean<R: Read, W: Write>(reader: &mut R, writer: &mut W, buf: &mut Vec<u8>) -> Result<()> {
    let mut head: Vec<u8> = Vec::with_capacity(MAX_LEN);
    let mut hasher = Hasher::new();
    let mut hashing = false;
    let mut size = 0u64;
    loop {
        match pktline::read_packet(reader, buf)? {
            None => bail!("git closed the filter inside content"),
            Some(Packet::Flush) => break,
            Some(Packet::Data) => {
                size += buf.len() as u64;
                if !hashing {
                    if head.len() + buf.len() < MAX_LEN {
                        head.extend_from_slice(buf);
                        continue;
                    }
                    hashing = true;
                    hasher.update(&head);
                    head.clear();
                }
                hasher.update(buf);
            }
        }
    }
    if size == 0 {
        return respond(writer, &[]);
    }
    if !hashing {
        if Pointer::is_pointer(&head) {
            return respond(writer, &head);
        }
        hasher.update(&head);
    }
    let pointer = Pointer { oid: hasher.finish(), size };
    respond(writer, &pointer.to_bytes())
}

/// Reads content until its flush and answers with the same content.
fn smudge<R: Read, W: Write>(reader: &mut R, writer: &mut W, buf: &mut Vec<u8>) -> Result<()> {
    let mut memory: Vec<u8> = Vec::new();
    let mut spool: Option<std::fs::File> = None;
    loop {
        match pktline::read_packet(reader, buf)? {
            None => bail!("git closed the filter inside content"),
            Some(Packet::Flush) => break,
            Some(Packet::Data) => {
                if let Some(file) = spool.as_mut() {
                    file.write_all(buf)?;
                } else if memory.len() + buf.len() > SPOOL_THRESHOLD {
                    let mut file = tempfile::tempfile().context("creating a spool file")?;
                    file.write_all(&memory)?;
                    file.write_all(buf)?;
                    memory = Vec::new();
                    spool = Some(file);
                } else {
                    memory.extend_from_slice(buf);
                }
            }
        }
    }
    match spool {
        None => respond(writer, &memory),
        Some(mut file) => {
            file.seek(SeekFrom::Start(0))?;
            pktline::write_text(writer, "status=success")?;
            pktline::write_flush(writer)?;
            let mut chunk = vec![0u8; pktline::MAX_PAYLOAD];
            loop {
                let n = file.read(&mut chunk)?;
                if n == 0 {
                    break;
                }
                pktline::write_packet(writer, &chunk[..n])?;
            }
            pktline::write_flush(writer)?;
            pktline::write_flush(writer)?;
            writer.flush()?;
            Ok(())
        }
    }
}

fn respond<W: Write>(writer: &mut W, content: &[u8]) -> Result<()> {
    pktline::write_text(writer, "status=success")?;
    pktline::write_flush(writer)?;
    pktline::write_content(writer, content)?;
    pktline::write_flush(writer)?;
    pktline::write_flush(writer)?;
    writer.flush()?;
    Ok(())
}

fn drain_content<R: Read>(reader: &mut R, buf: &mut Vec<u8>) -> io::Result<()> {
    loop {
        match pktline::read_packet(reader, buf)? {
            None => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "git closed the filter inside content")),
            Some(Packet::Flush) => return Ok(()),
            Some(Packet::Data) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the byte stream git would send: the handshake, then one command with its content.
    fn session(commands: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        pktline::write_text(&mut out, "git-filter-client").unwrap();
        pktline::write_text(&mut out, "version=2").unwrap();
        pktline::write_flush(&mut out).unwrap();
        pktline::write_text(&mut out, "capability=clean").unwrap();
        pktline::write_text(&mut out, "capability=smudge").unwrap();
        pktline::write_text(&mut out, "capability=delay").unwrap();
        pktline::write_flush(&mut out).unwrap();
        for (command, content) in commands {
            pktline::write_text(&mut out, &format!("command={command}")).unwrap();
            pktline::write_text(&mut out, "pathname=dir/file.bin").unwrap();
            pktline::write_flush(&mut out).unwrap();
            pktline::write_content(&mut out, content).unwrap();
            pktline::write_flush(&mut out).unwrap();
        }
        out
    }

    /// One command's answer: the status list, the content, and the trailing list.
    type Answer = (Vec<String>, Vec<u8>, Vec<String>);

    /// Parses the filter's output back into its handshake lists and per-command results.
    fn parse_output(output: &[u8], commands: usize) -> (Vec<String>, Vec<String>, Vec<Answer>) {
        let mut reader = output;
        let mut buf = Vec::new();
        let welcome = pktline::read_text_list(&mut reader, &mut buf).unwrap().unwrap();
        let capabilities = pktline::read_text_list(&mut reader, &mut buf).unwrap().unwrap();
        let mut results = Vec::new();
        for _ in 0..commands {
            let status = pktline::read_text_list(&mut reader, &mut buf).unwrap().unwrap();
            let mut content = Vec::new();
            loop {
                match pktline::read_packet(&mut reader, &mut buf).unwrap().unwrap() {
                    Packet::Flush => break,
                    Packet::Data => content.extend_from_slice(&buf),
                }
            }
            let trailer = pktline::read_text_list(&mut reader, &mut buf).unwrap().unwrap();
            results.push((status, content, trailer));
        }
        assert!(pktline::read_packet(&mut reader, &mut buf).unwrap().is_none());
        (welcome, capabilities, results)
    }

    fn run_session(commands: &[(&str, &[u8])]) -> Vec<Answer> {
        let input = session(commands);
        let mut output = Vec::new();
        run(input.as_slice(), &mut output, || {}).unwrap();
        let (welcome, capabilities, results) = parse_output(&output, commands.len());
        assert_eq!(welcome, vec!["git-filter-server", "version=2"]);
        assert_eq!(capabilities, vec!["capability=clean", "capability=smudge"]);
        results
    }

    #[test]
    fn clean_turns_content_into_a_pointer() {
        let content = vec![0x5au8; 200_000];
        let mut hasher = Hasher::new();
        hasher.update(&content);
        let expected = Pointer { oid: hasher.finish(), size: content.len() as u64 };
        let results = run_session(&[("clean", &content)]);
        assert_eq!(results[0].0, vec!["status=success"]);
        assert_eq!(results[0].1, expected.to_bytes());
        assert!(results[0].2.is_empty());
    }

    #[test]
    fn clean_pointerizes_small_content_and_passes_pointers_and_empty_through() {
        let small = b"just a few bytes\n".to_vec();
        let mut hasher = Hasher::new();
        hasher.update(&small);
        let pointer = Pointer { oid: hasher.finish(), size: small.len() as u64 }.to_bytes();
        let results = run_session(&[("clean", &small), ("clean", &pointer), ("clean", b"")]);
        assert_eq!(results[0].1, pointer);
        assert_eq!(results[1].1, pointer);
        assert_eq!(results[2].1, b"");
    }

    #[test]
    fn smudge_is_the_identity() {
        let pointer = Pointer { oid: "0".repeat(64), size: 9 }.to_bytes();
        let big = vec![1u8; pktline::MAX_PAYLOAD * 2 + 3];
        let results = run_session(&[("smudge", &pointer), ("smudge", &big), ("smudge", b"")]);
        assert_eq!(results[0].1, pointer);
        assert_eq!(results[1].1, big);
        assert_eq!(results[2].1, b"");
    }

    #[test]
    fn unknown_commands_get_an_error_status() {
        let input = session(&[("list_available_blobs", b"")]);
        let mut output = Vec::new();
        run(input.as_slice(), &mut output, || {}).unwrap();
        let mut reader = output.as_slice();
        let mut buf = Vec::new();
        pktline::read_text_list(&mut reader, &mut buf).unwrap();
        pktline::read_text_list(&mut reader, &mut buf).unwrap();
        assert_eq!(pktline::read_text_list(&mut reader, &mut buf).unwrap().unwrap(), vec!["status=error"]);
    }
}
