//! MCP stdio framing (ADR-0019).
//!
//! The MCP stdio transport (2024-11-05 through 2025-06-18, verified
//! 2026-09-15) is **newline-delimited**: one UTF-8 JSON-RPC message per
//! line, and messages MUST NOT contain embedded newlines. The F10b brief
//! specified LSP-style `Content-Length: N\r\n\r\n{json}` framing — a
//! factual deviation from the published transport spec (every real MCP
//! client — Claude Code, Cursor, the reference SDKs — speaks
//! newline-delimited JSON, and a Content-Length-only server would be
//! unusable by all of them). The server therefore speaks **both**: the
//! session's first frame decides the mode (a leading `{` ⇒ newline
//! framing; a leading `Content-Length:` header ⇒ LSP framing), and every
//! response is written back in the mode the client chose. Compact JSON
//! serialization never emits raw newlines, so the newline delimiter stays
//! unambiguous in both directions.
//!
//! Both modes are capped ([`MAX_FRAME_BYTES`]): a stdio server is
//! long-lived, and an unbounded line read is a memory-exhaustion footgun
//! the CLI (one-shot, bounded payloads) never has.

use std::io::{self, BufRead, Write};

/// Upper bound on one framed message (32 MiB — three orders above any
/// honest diff payload; rules and notes are kilobytes).
pub const MAX_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Upper bound on one header line in Content-Length mode.
const MAX_HEADER_BYTES: usize = 16 * 1024;

/// How much of the buffer one capped-read pass takes at most.
const CHUNK: usize = 8 * 1024;

/// The session's framing mode, fixed by the first frame (ADR-0019).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// The MCP stdio transport: one JSON-RPC message per `\n`-terminated
    /// line. Stray `\r` before the newline is tolerated (CRLF pipes).
    Newline,
    /// LSP-style `Content-Length: N\r\n\r\n` followed by exactly N body
    /// bytes (which may contain newlines — the length is the delimiter).
    ContentLength,
}

/// One read outcome. [`Frame::Invalid`] keeps the session alive: the
/// server answers with a JSON-RPC `-32700` and stays in sync.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    /// A complete message body.
    Message(String),
    /// Clean end of input (stdin closed).
    Eof,
    /// A structurally unusable frame (over cap, bad header, not UTF-8).
    Invalid(&'static str),
}

/// Buffered reader that yields one framed message at a time.
pub struct FrameReader<R> {
    inner: R,
    framing: Option<Framing>,
}

impl<R: BufRead> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            framing: None,
        }
    }

    /// The session's framing, once the first frame has picked it.
    pub fn framing(&self) -> Option<Framing> {
        self.framing
    }

    /// Read the next frame, skipping blank separator lines. `Ok(Frame::Eof)`
    /// ends the session.
    pub fn next_frame(&mut self) -> io::Result<Frame> {
        loop {
            let framing = match self.framing {
                Some(framing) => framing,
                None => match self.detect()? {
                    Some(framing) => {
                        self.framing = Some(framing);
                        framing
                    }
                    None => return Ok(Frame::Eof),
                },
            };
            let frame = match framing {
                Framing::Newline => self.next_line()?,
                Framing::ContentLength => self.next_content_length()?,
            };
            match frame {
                // Blank lines are framing noise, not empty messages.
                Frame::Message(text) if text.trim().is_empty() => continue,
                frame => return Ok(frame),
            }
        }
    }

    /// Detect the session mode from the first non-whitespace byte: `{`
    /// starts a newline-delimited message (every MCP JSON-RPC message is an
    /// object); a header-shaped first line (`Name: value`, e.g.
    /// `Content-Length:` or a `Content-Type:` sent ahead of it) starts an
    /// LSP-style frame. Anything else is treated as newline-delimited so it
    /// fails JSON parsing loudly (a `-32700` response) instead of guessing
    /// a mode. `Ok(None)` = clean EOF before any frame.
    fn detect(&mut self) -> io::Result<Option<Framing>> {
        loop {
            let buf = self.inner.fill_buf()?;
            if buf.is_empty() {
                return Ok(None);
            }
            match buf[0] {
                b'\r' | b'\n' | b' ' | b'\t' => self.inner.consume(1),
                _ => break,
            }
        }
        let buf = self.inner.fill_buf()?;
        if buf.is_empty() {
            return Ok(None);
        }
        if buf[0] == b'{' {
            Ok(Some(Framing::Newline))
        } else if looks_like_header(buf) {
            Ok(Some(Framing::ContentLength))
        } else {
            Ok(Some(Framing::Newline))
        }
    }

    /// One newline-delimited message, capped; on over-cap the rest of the
    /// line is drained so the next read stays in sync. A final line without
    /// its `\n` (writer died mid-flush) is still a message.
    fn next_line(&mut self) -> io::Result<Frame> {
        let (bytes, newline) = match self.take_line(MAX_FRAME_BYTES)? {
            Some(out) => out,
            None => return Ok(Frame::Eof),
        };
        if bytes.len() > MAX_FRAME_BYTES {
            if !newline {
                self.drain_line()?;
            }
            return Ok(Frame::Invalid("frame exceeds the 32 MiB session cap"));
        }
        let mut bytes = bytes;
        while bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        match String::from_utf8(bytes) {
            Ok(text) => Ok(Frame::Message(text)),
            Err(_) => Ok(Frame::Invalid("frame is not valid UTF-8")),
        }
    }

    /// One `Content-Length`-framed message: headers terminated by a blank
    /// line, then exactly N body bytes.
    fn next_content_length(&mut self) -> io::Result<Frame> {
        // Some LSP clients separate frames with `\r\n`; headers never
        // start with CR/LF, so leading ones are skipped safely.
        self.skip_eol()?;
        let mut length: Option<usize> = None;
        loop {
            let (raw, newline) = match self.take_line(MAX_HEADER_BYTES)? {
                Some(out) => out,
                None => return Ok(Frame::Eof),
            };
            if !newline {
                if raw.len() > MAX_HEADER_BYTES {
                    self.drain_line()?;
                    return Ok(Frame::Invalid("header line exceeds the 16 KiB cap"));
                }
                // EOF mid-headers: the session is over.
                return Ok(Frame::Eof);
            }
            let mut line: Vec<u8> = raw;
            while line.last() == Some(&b'\r') {
                line.pop();
            }
            let text = String::from_utf8_lossy(&line);
            if text.is_empty() {
                break;
            }
            if let Some(value) = header_value(&text, "content-length") {
                match value.trim().parse::<usize>() {
                    Ok(len) => length = Some(len),
                    Err(_) => {
                        return Ok(Frame::Invalid("Content-Length header is not a number"));
                    }
                }
            }
        }
        let Some(length) = length else {
            return Ok(Frame::Invalid("frame has no Content-Length header"));
        };
        if length > MAX_FRAME_BYTES {
            self.drain_exact(length)?;
            return Ok(Frame::Invalid("frame exceeds the 32 MiB session cap"));
        }
        let mut body = vec![0u8; length];
        if let Err(err) = self.inner.read_exact(&mut body) {
            if err.kind() == io::ErrorKind::UnexpectedEof {
                return Ok(Frame::Invalid("truncated frame"));
            }
            return Err(err);
        }
        match String::from_utf8(body) {
            Ok(text) => Ok(Frame::Message(text)),
            Err(_) => Ok(Frame::Invalid("frame is not valid UTF-8")),
        }
    }

    /// Read up to `cap` bytes or the next `\n` (excluded). Returns
    /// `Some((bytes, newline_seen))` — `newline_seen == false` means the
    /// cap was hit mid-line — or `None` on EOF with nothing read.
    fn take_line(&mut self, cap: usize) -> io::Result<Option<(Vec<u8>, bool)>> {
        let mut buf = Vec::new();
        loop {
            let available = self.inner.fill_buf()?;
            if available.is_empty() {
                return Ok(if buf.is_empty() {
                    None
                } else {
                    Some((buf, false))
                });
            }
            let take = available.len().min(CHUNK);
            let end = &available[..take];
            match end.iter().position(|&b| b == b'\n') {
                Some(pos) => {
                    buf.extend_from_slice(&end[..pos]);
                    self.inner.consume(pos + 1);
                    return Ok(Some((buf, true)));
                }
                None => {
                    buf.extend_from_slice(end);
                    self.inner.consume(take);
                }
            }
            if buf.len() > cap {
                return Ok(Some((buf, false)));
            }
        }
    }

    /// Consume and discard through the next `\n` (unbounded — resync after
    /// an over-cap line).
    fn drain_line(&mut self) -> io::Result<()> {
        loop {
            let (found, len) = {
                let available = self.inner.fill_buf()?;
                if available.is_empty() {
                    return Ok(());
                }
                match available.iter().position(|&b| b == b'\n') {
                    Some(pos) => (true, pos + 1),
                    None => (false, available.len()),
                }
            };
            self.inner.consume(len);
            if found {
                return Ok(());
            }
        }
    }

    /// Consume and discard exactly `len` bytes (resync after an over-cap
    /// Content-Length frame).
    fn drain_exact(&mut self, len: usize) -> io::Result<()> {
        let mut remaining = len;
        let mut scratch = [0u8; CHUNK];
        while remaining > 0 {
            let take = remaining.min(scratch.len());
            if let Err(err) = self.inner.read_exact(&mut scratch[..take]) {
                if err.kind() == io::ErrorKind::UnexpectedEof {
                    return Ok(());
                }
                return Err(err);
            }
            remaining -= take;
        }
        Ok(())
    }

    /// Consume leading CR/LF bytes only.
    fn skip_eol(&mut self) -> io::Result<()> {
        loop {
            let buf = self.inner.fill_buf()?;
            if buf.is_empty() {
                return Ok(());
            }
            match buf[0] {
                b'\r' | b'\n' => self.inner.consume(1),
                _ => return Ok(()),
            }
        }
    }
}

/// Write messages in the session's framing mode.
pub struct FramedWriter<W> {
    inner: W,
    framing: Framing,
}

impl<W: Write> FramedWriter<W> {
    pub fn new(inner: W, framing: Framing) -> Self {
        Self { inner, framing }
    }

    /// Adopt the reader-detected mode (a no-op after the first message —
    /// the first frame fixes the mode for the session).
    pub fn set_framing(&mut self, framing: Framing) {
        self.framing = framing;
    }

    /// One message out, framed per mode, flushed. `json` must be compact
    /// (no embedded newlines) — [`serde_json::to_string`] guarantees that.
    pub fn write_message(&mut self, json: &str) -> io::Result<()> {
        let written = match self.framing {
            Framing::Newline => self
                .inner
                .write_all(json.as_bytes())
                .and_then(|()| self.inner.write_all(b"\n")),
            Framing::ContentLength => self
                .inner
                .write_all(format!("Content-Length: {}\r\n\r\n", json.len()).as_bytes())
                .and_then(|()| self.inner.write_all(json.as_bytes())),
        };
        written.and_then(|()| self.inner.flush())
    }
}

/// True when the buffer starts with an ASCII header name followed by `:`
/// (`Content-Length:`, `Content-Type:`, …) — the shape of an LSP-style
/// frame's first line, and never the shape of a JSON message.
fn looks_like_header(buf: &[u8]) -> bool {
    for (index, byte) in buf.iter().enumerate() {
        match byte {
            b':' => return index > 0,
            byte if byte.is_ascii_alphanumeric() || *byte == b'-' => {}
            _ => return false,
        }
    }
    false
}

/// The value of a `Name: value` header line when the (lowercased) name
/// matches, ASCII-case-insensitively.
fn header_value<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let (key, value) = line.split_once(':')?;
    if key.trim().eq_ignore_ascii_case(name) {
        Some(value)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    fn frames(input: impl AsRef<[u8]>) -> Vec<Frame> {
        let mut reader = FrameReader::new(BufReader::new(input.as_ref()));
        let mut out = Vec::new();
        while let Ok(frame) = reader.next_frame() {
            match frame {
                Frame::Eof => break,
                frame => out.push(frame),
            }
        }
        out
    }

    #[test]
    fn newline_framing_splits_on_lines_and_skips_blanks() {
        let frames = frames("\n{\"id\":1}\r\n\n{\"id\":2}\n");
        assert_eq!(
            frames,
            vec![
                Frame::Message("{\"id\":1}".to_owned()),
                Frame::Message("{\"id\":2}".to_owned()),
            ]
        );
    }

    #[test]
    fn content_length_framing_reads_exact_bodies_with_newlines() {
        let input = "Content-Length: 14\r\n\r\n{\"a\":1,\n\"b\":2}Content-Length: 5\r\n\r\n[1,2]";
        let frames = frames(input);
        assert_eq!(
            frames,
            vec![
                Frame::Message("{\"a\":1,\n\"b\":2}".to_owned()),
                Frame::Message("[1,2]".to_owned()),
            ]
        );
    }

    /// A body may itself start with `{` and the session is still in
    /// Content-Length mode — detection runs once, on the first frame only.
    #[test]
    fn detection_is_first_frame_only() {
        let mut reader =
            FrameReader::new(BufReader::new(b"Content-Length: 2\r\n\r\n{}".as_slice()));
        assert_eq!(
            reader.next_frame().expect("read"),
            Frame::Message("{}".to_owned())
        );
        assert_eq!(reader.framing(), Some(Framing::ContentLength));
    }

    #[test]
    fn garbage_is_read_as_a_line_and_fails_loudly() {
        let frames = frames("NOT-JSON\n{\"id\":1}\n");
        assert_eq!(
            frames,
            vec![
                Frame::Message("NOT-JSON".to_owned()),
                Frame::Message("{\"id\":1}".to_owned()),
            ]
        );
    }

    #[test]
    fn over_cap_line_is_refused_and_resynced() {
        let mut input = vec![b'a'; MAX_FRAME_BYTES + 1];
        input.push(b'\n');
        input.extend_from_slice(b"{\"id\":1}\n");
        let frames = frames(String::from_utf8(input).expect("plain ascii"));
        assert_eq!(
            frames[0],
            Frame::Invalid("frame exceeds the 32 MiB session cap")
        );
        // The session resyncs at the next line.
        assert_eq!(frames[1], Frame::Message("{\"id\":1}".to_owned()));
    }

    #[test]
    fn unknown_headers_are_ignored_in_content_length_mode() {
        let frames = frames("Content-Type: application/json\r\nContent-Length: 2\r\n\r\n{}");
        assert_eq!(frames, vec![Frame::Message("{}".to_owned())]);
    }

    #[test]
    fn invalid_utf8_is_reported_not_panicked() {
        let frames = frames(b"{\"a\":\"\xff\"}\n{\"id\":1}\n");
        assert_eq!(frames[0], Frame::Invalid("frame is not valid UTF-8"));
        assert_eq!(frames[1], Frame::Message("{\"id\":1}".to_owned()));
    }

    #[test]
    fn missing_content_length_header_is_invalid() {
        let frames = frames("Content-Type: application/json\r\n\r\n{}");
        assert_eq!(
            frames,
            vec![Frame::Invalid("frame has no Content-Length header")]
        );
    }

    #[test]
    fn truncated_body_is_invalid() {
        let frames = frames("Content-Length: 10\r\n\r\n{");
        assert_eq!(frames, vec![Frame::Invalid("truncated frame")]);
    }

    #[test]
    fn empty_input_is_clean_eof() {
        let mut reader = FrameReader::new(BufReader::new("".as_bytes()));
        assert_eq!(reader.next_frame().expect("read"), Frame::Eof);
    }

    #[test]
    fn unterminated_final_line_is_still_a_message() {
        let frames = frames("{\"id\":1}");
        assert_eq!(frames, vec![Frame::Message("{\"id\":1}".to_owned())]);
    }

    #[test]
    fn writer_mirrors_both_modes() {
        let mut out = Vec::new();
        let mut writer = FramedWriter::new(&mut out, Framing::Newline);
        writer.write_message("{\"id\":1}").expect("write");
        writer.set_framing(Framing::ContentLength);
        writer.write_message("{\"id\":2}").expect("write");
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "{\"id\":1}\nContent-Length: 8\r\n\r\n{\"id\":2}"
        );
    }

    #[test]
    fn header_value_is_case_insensitive() {
        assert_eq!(
            header_value("CONTENT-LENGTH: 42", "content-length"),
            Some(" 42")
        );
        assert_eq!(header_value("Content-Type: x", "content-length"), None);
        assert_eq!(header_value("no-colon", "content-length"), None);
    }
}
