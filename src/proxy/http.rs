//! The server side of the local proxy: HTTP/1.1 request heads and bodies in, streamed
//! responses out. Only what one local client needs: keep-alive, Content-Length and
//! chunked bodies, `Expect: 100-continue`, and a flush after every chunk so
//! server-sent events arrive as they are produced.

use std::io::{self, BufRead, Read, Write};

/// The largest request head accepted.
pub const MAX_HEAD: usize = 64 * 1024;
const MAX_HEADERS: usize = 128;
const MAX_LINE: usize = 8 * 1024;

/// Hop-by-hop headers, which describe one connection and are never forwarded.
const HOP_BY_HOP: [&str; 7] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

pub fn hop_by_hop(name: &str) -> bool {
    HOP_BY_HOP.iter().any(|h| name.eq_ignore_ascii_case(h))
}

pub struct Head {
    pub method: String,
    /// The request target as sent, query included.
    pub target: String,
    pub http10: bool,
    /// Header names and values in order, values byte for byte.
    pub headers: Vec<(String, Vec<u8>)>,
}

pub enum Framing {
    Empty,
    Length(u64),
    Chunked,
}

impl Head {
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_slice())
    }

    pub fn header_str(&self, name: &str) -> Option<&str> {
        self.header(name)
            .and_then(|v| std::str::from_utf8(v).ok())
            .map(str::trim)
    }

    /// Whether any comma-separated value of `name` equals `token`, ignoring case.
    pub fn has_token(&self, name: &str, token: &str) -> bool {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .flat_map(|(_, v)| v.split(|&b| b == b','))
            .any(|part| part.trim_ascii().eq_ignore_ascii_case(token.as_bytes()))
    }

    /// The path without its query.
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    pub fn keep_alive(&self) -> bool {
        if self.http10 {
            self.has_token("connection", "keep-alive")
        } else {
            !self.has_token("connection", "close")
        }
    }

    pub fn framing(&self) -> io::Result<Framing> {
        if self
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case("transfer-encoding"))
        {
            return if self.has_token("transfer-encoding", "chunked") {
                Ok(Framing::Chunked)
            } else {
                Err(invalid("unsupported transfer-encoding"))
            };
        }
        let mut lengths = self
            .headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("content-length"));
        match lengths.next() {
            None => Ok(Framing::Empty),
            Some((_, value)) => {
                let length = std::str::from_utf8(value)
                    .ok()
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .ok_or_else(|| invalid("invalid content-length"))?;
                if lengths.any(|(_, other)| other != value) {
                    return Err(invalid("conflicting content-length"));
                }
                Ok(if length == 0 {
                    Framing::Empty
                } else {
                    Framing::Length(length)
                })
            }
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

/// Reads one request head. `Ok(None)` means the client closed the connection
/// between requests.
pub fn read_head(reader: &mut impl BufRead) -> io::Result<Option<Head>> {
    let mut head: Vec<u8> = Vec::new();
    loop {
        let available = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if available.is_empty() {
            return if head.is_empty() {
                Ok(None)
            } else {
                Err(io::ErrorKind::UnexpectedEof.into())
            };
        }
        let searched = head.len().saturating_sub(3);
        head.extend_from_slice(available);
        let taken = available.len();
        if let Some(end) = head[searched..]
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| searched + p + 4)
        {
            reader.consume(taken - (head.len() - end));
            head.truncate(end);
            break;
        }
        reader.consume(taken);
        if head.len() > MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "request head too large",
            ));
        }
    }
    if head.len() > MAX_HEAD {
        return Err(io::Error::new(
            io::ErrorKind::OutOfMemory,
            "request head too large",
        ));
    }
    let mut slots = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut request = httparse::Request::new(&mut slots);
    match request.parse(&head) {
        Ok(httparse::Status::Complete(_)) => {}
        Ok(httparse::Status::Partial) => return Err(invalid("incomplete request head")),
        Err(httparse::Error::TooManyHeaders) => {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "too many headers",
            ));
        }
        Err(error) => return Err(invalid(&error.to_string())),
    }
    Ok(Some(Head {
        method: request.method.unwrap_or_default().to_owned(),
        target: request.path.unwrap_or_default().to_owned(),
        http10: request.version == Some(0),
        headers: request
            .headers
            .iter()
            .map(|h| (h.name.to_owned(), h.value.to_vec()))
            .collect(),
    }))
}

fn read_line(reader: &mut impl BufRead) -> io::Result<Vec<u8>> {
    let mut line = Vec::new();
    reader
        .by_ref()
        .take(MAX_LINE as u64)
        .read_until(b'\n', &mut line)?;
    if !line.ends_with(b"\n") {
        return Err(invalid("malformed chunked body"));
    }
    line.pop();
    if line.ends_with(b"\r") {
        line.pop();
    }
    Ok(line)
}

/// Decodes a chunked request body.
pub struct Chunked<R> {
    inner: R,
    remaining: u64,
    done: bool,
}

impl<R: BufRead> Chunked<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            remaining: 0,
            done: false,
        }
    }
}

impl<R: BufRead> Read for Chunked<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.done || buf.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            let line = read_line(&mut self.inner)?;
            let size = line.split(|&b| b == b';').next().unwrap_or_default();
            let size = std::str::from_utf8(size)
                .ok()
                .and_then(|s| u64::from_str_radix(s.trim(), 16).ok())
                .ok_or_else(|| invalid("malformed chunk size"))?;
            if size == 0 {
                // Trailers, then the empty line that ends the body.
                while !read_line(&mut self.inner)?.is_empty() {}
                self.done = true;
                return Ok(0);
            }
            self.remaining = size;
        }
        let wanted = buf
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let read = self.inner.read(&mut buf[..wanted])?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        self.remaining -= read as u64;
        if self.remaining == 0 && !read_line(&mut self.inner)?.is_empty() {
            return Err(invalid("malformed chunk terminator"));
        }
        Ok(read)
    }
}

/// Exactly `remaining` bytes, or `UnexpectedEof` when the client stops sooner, so
/// a body cut short is never forwarded as if it were complete.
struct Exact<R> {
    inner: R,
    remaining: u64,
}

impl<R: Read> Read for Exact<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.remaining == 0 || buf.is_empty() {
            return Ok(0);
        }
        let wanted = buf
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let read = self.inner.read(&mut buf[..wanted])?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "request body ended before its content-length",
            ));
        }
        self.remaining -= read as u64;
        Ok(read)
    }
}

/// A reader over the request body that ends where the body ends.
pub fn body_reader<'a, R: BufRead + 'a>(reader: R, framing: &Framing) -> Box<dyn Read + 'a> {
    match framing {
        Framing::Empty => Box::new(io::empty()),
        Framing::Length(length) => Box::new(Exact {
            inner: reader,
            remaining: *length,
        }),
        Framing::Chunked => Box::new(Chunked::new(reader)),
    }
}

fn reason(status: u16) -> &'static str {
    ureq::http::StatusCode::from_u16(status)
        .ok()
        .and_then(|s| s.canonical_reason())
        .unwrap_or("Unknown")
}

pub fn write_head(
    out: &mut impl Write,
    status: u16,
    headers: &[(String, Vec<u8>)],
) -> io::Result<()> {
    let mut head = format!("HTTP/1.1 {status} {}\r\n", reason(status)).into_bytes();
    for (name, value) in headers {
        head.extend_from_slice(name.as_bytes());
        head.extend_from_slice(b": ");
        head.extend_from_slice(value);
        head.extend_from_slice(b"\r\n");
    }
    head.extend_from_slice(b"\r\n");
    out.write_all(&head)?;
    out.flush()
}

/// Writes one chunk and flushes it, so the client sees it at once.
pub fn write_chunk(out: &mut impl Write, data: &[u8]) -> io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let mut chunk = format!("{:x}\r\n", data.len()).into_bytes();
    chunk.extend_from_slice(data);
    chunk.extend_from_slice(b"\r\n");
    out.write_all(&chunk)?;
    out.flush()
}

pub fn finish_chunks(out: &mut impl Write) -> io::Result<()> {
    out.write_all(b"0\r\n\r\n")?;
    out.flush()
}

/// A complete response with a body, such as an error from the proxy itself.
pub fn write_simple(
    out: &mut impl Write,
    status: u16,
    content_type: &str,
    body: &[u8],
    close: bool,
) -> io::Result<()> {
    let mut headers = vec![
        ("content-type".to_string(), content_type.as_bytes().to_vec()),
        (
            "content-length".to_string(),
            body.len().to_string().into_bytes(),
        ),
    ];
    if close {
        headers.push(("connection".into(), b"close".to_vec()));
    }
    write_head(out, status, &headers)?;
    out.write_all(body)?;
    out.flush()
}

/// An error in the Anthropic API's shape, so clients report it like any other.
pub fn error_body(kind: &str, message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "type": "error",
        "error": {"type": kind, "message": message},
    }))
    .expect("serialize error")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    #[test]
    fn reads_heads_across_reads_and_keeps_the_body() {
        let input = b"POST /v1/messages?beta=true HTTP/1.1\r\nHost: x\r\nX-Api-Key: k\x80\r\nContent-Length: 5\r\n\r\nhelloGET / HTTP/1.0\r\n\r\n";
        // A one-byte buffer forces the terminator search across reads.
        let mut reader = BufReader::with_capacity(1, &input[..]);
        let head = read_head(&mut reader).unwrap().unwrap();
        assert_eq!(head.method, "POST");
        assert_eq!(head.target, "/v1/messages?beta=true");
        assert_eq!(head.path(), "/v1/messages");
        assert_eq!(head.header("x-api-key"), Some(&b"k\x80"[..]));
        assert!(head.keep_alive());
        let framing = head.framing().unwrap();
        let mut body = Vec::new();
        body_reader(&mut reader, &framing)
            .read_to_end(&mut body)
            .unwrap();
        assert_eq!(body, b"hello");
        let next = read_head(&mut reader).unwrap().unwrap();
        assert!(next.http10 && !next.keep_alive());
        assert!(read_head(&mut reader).unwrap().is_none());
    }

    #[test]
    fn decodes_chunked_bodies_with_extensions_and_trailers() {
        let input = b"5;ext=1\r\nhello\r\n6\r\n world\r\n0\r\nx-trailer: y\r\n\r\nrest";
        let mut reader = BufReader::new(&input[..]);
        let mut body = Vec::new();
        Chunked::new(&mut reader).read_to_end(&mut body).unwrap();
        assert_eq!(body, b"hello world");
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b"rest");
        let mut broken = Vec::new();
        assert!(
            Chunked::new(BufReader::new(&b"5\r\nhel"[..]))
                .read_to_end(&mut broken)
                .is_err()
        );
    }

    #[test]
    fn a_body_shorter_than_its_length_is_an_error() {
        let mut body = Vec::new();
        let error = body_reader(BufReader::new(&b"hel"[..]), &Framing::Length(5))
            .read_to_end(&mut body)
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        let mut reader = BufReader::new(&b"hello world"[..]);
        let mut exact = Vec::new();
        body_reader(&mut reader, &Framing::Length(5))
            .read_to_end(&mut exact)
            .unwrap();
        assert_eq!(exact, b"hello");
        let mut rest = Vec::new();
        reader.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, b" world");
    }

    #[test]
    fn rejects_oversized_heads_and_bad_framing() {
        let huge = format!("GET / HTTP/1.1\r\nx: {}\r\n\r\n", "a".repeat(MAX_HEAD));
        let error = read_head(&mut BufReader::new(huge.as_bytes()))
            .err()
            .unwrap();
        assert_eq!(error.kind(), io::ErrorKind::OutOfMemory);
        let head = read_head(&mut BufReader::new(
            &b"POST / HTTP/1.1\r\ncontent-length: 1\r\ncontent-length: 2\r\n\r\n"[..],
        ))
        .unwrap()
        .unwrap();
        assert!(head.framing().is_err());
    }
}
