//! PostgreSQL frontend/backend wire protocol (version 3.0).
//!
//! This module deals only in bytes and message framing. Higher-level session
//! logic (auth flow, dispatching queries) lives in [`crate::server`].
//!
//! Message framing: after the startup phase, every message is
//! `[type: u8][length: i32 big-endian][payload]`, where `length` counts
//! itself but not the type byte.

use std::io::{self, Read};

/// Protocol version 3.0, sent in the startup packet (`0x00030000`).
pub const PROTOCOL_VERSION_3: i32 = 196608;
const SSL_REQUEST_CODE: i32 = 80877103;
const GSS_ENC_REQUEST_CODE: i32 = 80877104;
const CANCEL_REQUEST_CODE: i32 = 80877102;

/// The result of reading the very first packet on a connection.
#[derive(Debug)]
pub enum Startup {
    /// A normal startup with the given parameter key/value pairs.
    Params(Vec<(String, String)>),
    /// Client requested an SSL/TLS upgrade.
    SslRequest,
    /// Client requested GSSAPI encryption.
    GssEncRequest,
    /// Client is asking to cancel a running query on another backend.
    /// (Fields retained for when query cancellation is implemented.)
    CancelRequest {
        #[allow(dead_code)]
        pid: i32,
        #[allow(dead_code)]
        secret: i32,
    },
}

/// Read the startup packet. Unlike later messages, it carries no type byte.
pub fn read_startup<R: Read>(r: &mut R) -> io::Result<Startup> {
    let len = read_i32(r)?;
    if !(8..=10_000_000).contains(&len) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bad startup length",
        ));
    }
    let mut body = vec![0u8; (len - 4) as usize];
    r.read_exact(&mut body)?;
    if body.len() < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "startup packet too short",
        ));
    }
    let code = i32::from_be_bytes([body[0], body[1], body[2], body[3]]);

    match code {
        SSL_REQUEST_CODE => Ok(Startup::SslRequest),
        GSS_ENC_REQUEST_CODE => Ok(Startup::GssEncRequest),
        CANCEL_REQUEST_CODE => {
            if body.len() < 12 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cancel request too short",
                ));
            }
            let pid = i32::from_be_bytes([body[4], body[5], body[6], body[7]]);
            let secret = i32::from_be_bytes([body[8], body[9], body[10], body[11]]);
            Ok(Startup::CancelRequest { pid, secret })
        }
        PROTOCOL_VERSION_3 => {
            // Remainder is a sequence of NUL-terminated key/value strings,
            // ending with a zero-length key.
            let mut params = Vec::new();
            let mut cur = &body[4..];
            loop {
                let (key, rest) = read_cstr_slice(cur)?;
                if key.is_empty() {
                    break;
                }
                let (val, rest2) = read_cstr_slice(rest)?;
                params.push((key, val));
                cur = rest2;
            }
            Ok(Startup::Params(params))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unsupported startup/protocol code {other}"),
        )),
    }
}

/// A frontend (client → server) message read after startup.
#[derive(Debug)]
pub enum FrontendMessage {
    /// Simple query protocol: a SQL string to execute.
    Query(String),
    /// Extended protocol: prepare a statement.
    Parse {
        name: String,
        query: String,
        param_types: Vec<i32>,
    },
    /// Extended protocol: bind parameters to a prepared statement.
    Bind {
        portal: String,
        statement: String,
        /// Raw parameter values (text or binary per format codes).
        params: Vec<Option<Vec<u8>>>,
        param_formats: Vec<i16>,
        result_formats: Vec<i16>,
    },
    /// Describe a prepared statement (`'S'`) or portal (`'P'`).
    Describe { kind: u8, name: String },
    /// Execute a portal, up to `max_rows` (0 = unlimited).
    Execute { portal: String, max_rows: i32 },
    /// Close a prepared statement or portal.
    Close { kind: u8, name: String },
    /// End of an extended-protocol message group.
    Sync,
    /// Flush pending output.
    Flush,
    /// A password (cleartext or SASL) response — payload bytes.
    /// (Retained for when authentication beyond trust is implemented.)
    Password(#[allow(dead_code)] Vec<u8>),
    /// `COPY ... FROM STDIN` data chunk (may hold several rows).
    CopyData(Vec<u8>),
    /// Client signals the end of `COPY` data.
    CopyDone,
    /// Client aborts an in-progress `COPY`, with an error message.
    CopyFail(String),
    /// Client is disconnecting.
    Terminate,
    /// An unrecognized message; payload preserved for diagnostics.
    Unknown {
        tag: u8,
        #[allow(dead_code)]
        body: Vec<u8>,
    },
}

/// Read one frontend message. Returns `Ok(None)` on clean EOF.
pub fn read_message<R: Read>(r: &mut R) -> io::Result<Option<FrontendMessage>> {
    let mut tag = [0u8; 1];
    match r.read(&mut tag)? {
        0 => return Ok(None),
        _ => {}
    }
    let tag = tag[0];
    let len = read_i32(r)?;
    if len < 4 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message length < 4",
        ));
    }
    let mut body = vec![0u8; (len - 4) as usize];
    r.read_exact(&mut body)?;

    let msg = match tag {
        b'Q' => {
            let (q, _) = read_cstr_slice(&body)?;
            FrontendMessage::Query(q)
        }
        b'X' => FrontendMessage::Terminate,
        b'P' => parse_parse(&body)?,
        b'B' => parse_bind(&body)?,
        b'D' => {
            let kind = *body.first().unwrap_or(&b'S');
            let rest = body.get(1..).unwrap_or(&[]);
            let (name, _) = read_cstr_slice(rest)?;
            FrontendMessage::Describe { kind, name }
        }
        b'E' => {
            let (portal, rest) = read_cstr_slice(&body)?;
            let max_rows = read_be_i32(rest)?;
            FrontendMessage::Execute { portal, max_rows }
        }
        b'C' => {
            let kind = *body.first().unwrap_or(&b'S');
            let rest = body.get(1..).unwrap_or(&[]);
            let (name, _) = read_cstr_slice(rest)?;
            FrontendMessage::Close { kind, name }
        }
        b'S' => FrontendMessage::Sync,
        b'H' => FrontendMessage::Flush,
        b'p' => FrontendMessage::Password(body),
        b'd' => FrontendMessage::CopyData(body),
        b'c' => FrontendMessage::CopyDone,
        b'f' => {
            let (msg, _) = read_cstr_slice(&body)?;
            FrontendMessage::CopyFail(msg)
        }
        other => FrontendMessage::Unknown { tag: other, body },
    };
    Ok(Some(msg))
}

fn parse_parse(body: &[u8]) -> io::Result<FrontendMessage> {
    let (name, rest) = read_cstr_slice(body)?;
    let (query, mut cur) = read_cstr_slice(rest)?;
    let n = take_count(&mut cur)?;
    let mut param_types = Vec::with_capacity(n);
    for _ in 0..n {
        param_types.push(take_i32(&mut cur)?);
    }
    Ok(FrontendMessage::Parse {
        name,
        query,
        param_types,
    })
}

fn parse_bind(body: &[u8]) -> io::Result<FrontendMessage> {
    let (portal, rest) = read_cstr_slice(body)?;
    let (statement, mut cur) = read_cstr_slice(rest)?;

    let nf = take_count(&mut cur)?;
    let mut param_formats = Vec::with_capacity(nf);
    for _ in 0..nf {
        param_formats.push(take_i16(&mut cur)?);
    }

    let np = take_count(&mut cur)?;
    let mut params = Vec::with_capacity(np);
    for _ in 0..np {
        let plen = take_i32(&mut cur)?;
        if plen < 0 {
            params.push(None);
        } else {
            let plen = plen as usize;
            params.push(Some(take_bytes(&mut cur, plen)?.to_vec()));
        }
    }

    let nr = take_count(&mut cur)?;
    let mut result_formats = Vec::with_capacity(nr);
    for _ in 0..nr {
        result_formats.push(take_i16(&mut cur)?);
    }

    Ok(FrontendMessage::Bind {
        portal,
        statement,
        params,
        param_formats,
        result_formats,
    })
}

/// Error helper for a message body that ended sooner than the framing implied.
fn short_message() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "message body truncated")
}

/// Read a big-endian i16 *count* (must be non-negative) from the front of
/// `cur`. A negative count is malformed; reject it rather than letting a huge
/// `as usize` cast overflow an allocation.
fn take_count(cur: &mut &[u8]) -> io::Result<usize> {
    let n = take_i16(cur)?;
    if n < 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "negative field count",
        ));
    }
    Ok(n as usize)
}

/// Read a big-endian i16 from the front of `cur`, advancing it.
fn take_i16(cur: &mut &[u8]) -> io::Result<i16> {
    let b = take_bytes(cur, 2)?;
    Ok(i16::from_be_bytes([b[0], b[1]]))
}

/// Read a big-endian i32 from the front of `cur`, advancing it.
fn take_i32(cur: &mut &[u8]) -> io::Result<i32> {
    let b = take_bytes(cur, 4)?;
    Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// Split `n` bytes off the front of `cur`, advancing it; errs if too short.
fn take_bytes<'a>(cur: &mut &'a [u8], n: usize) -> io::Result<&'a [u8]> {
    if cur.len() < n {
        return Err(short_message());
    }
    let (head, tail) = cur.split_at(n);
    *cur = tail;
    Ok(head)
}

/// Read a big-endian i32 from a fixed slice (must be at least 4 bytes).
fn read_be_i32(b: &[u8]) -> io::Result<i32> {
    if b.len() < 4 {
        return Err(short_message());
    }
    Ok(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

// --- backend message builder -------------------------------------------------

/// Builds a single backend message body, then frames it with its tag+length.
pub struct MessageBuilder {
    tag: u8,
    body: Vec<u8>,
}

impl MessageBuilder {
    pub fn new(tag: u8) -> Self {
        MessageBuilder {
            tag,
            body: Vec::with_capacity(32),
        }
    }

    pub fn put_u8(&mut self, v: u8) -> &mut Self {
        self.body.push(v);
        self
    }

    pub fn put_i16(&mut self, v: i16) -> &mut Self {
        self.body.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn put_i32(&mut self, v: i32) -> &mut Self {
        self.body.extend_from_slice(&v.to_be_bytes());
        self
    }

    pub fn put_bytes(&mut self, b: &[u8]) -> &mut Self {
        self.body.extend_from_slice(b);
        self
    }

    /// Append a NUL-terminated C string.
    pub fn put_cstr(&mut self, s: &str) -> &mut Self {
        self.body.extend_from_slice(s.as_bytes());
        self.body.push(0);
        self
    }

    /// Serialize into `[tag][len][body]`.
    pub fn finish(self) -> Vec<u8> {
        let len = (self.body.len() + 4) as i32;
        let mut out = Vec::with_capacity(self.body.len() + 5);
        out.push(self.tag);
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(&self.body);
        out
    }
}

// --- low-level read helpers --------------------------------------------------

fn read_i32<R: Read>(r: &mut R) -> io::Result<i32> {
    let mut buf = [0u8; 4];
    r.read_exact(&mut buf)?;
    Ok(i32::from_be_bytes(buf))
}

/// Read a NUL-terminated string from the front of `buf`, returning it and the
/// remaining slice.
fn read_cstr_slice(buf: &[u8]) -> io::Result<(String, &[u8])> {
    let nul = buf
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing NUL terminator"))?;
    let s = String::from_utf8_lossy(&buf[..nul]).into_owned();
    Ok((s, &buf[nul + 1..]))
}
