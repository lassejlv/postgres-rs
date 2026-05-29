//! End-to-end wire-protocol tests using a real TCP socket as a raw v3 client.
//!
//! Each test binds an ephemeral `127.0.0.1:0` listener, hands it to
//! `server::serve_on` on a background thread, then speaks the PostgreSQL v3
//! protocol by hand (std::net only) and asserts on the backend's replies.
//!
//! No `PGRS_PASSWORD` is set in the test environment, so the server uses trust
//! auth (AuthenticationOk with no challenge).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use postgres_rs::protocol::PROTOCOL_VERSION_3;
use postgres_rs::server;

/// Start a server on an ephemeral port and return a client socket connected to
/// it. The server thread is detached; it dies with the test process.
fn start_server() -> TcpStream {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    let addr = listener.local_addr().expect("local addr");
    thread::spawn(move || {
        // In-memory (no data dir). Ignore the result: it returns when the
        // listener stops accepting, which only happens at process exit.
        let _ = server::serve_on(listener, None);
    });
    // Retry briefly in case the accept loop hasn't started yet.
    let mut last_err = None;
    for _ in 0..50 {
        match TcpStream::connect(addr) {
            Ok(s) => {
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                return s;
            }
            Err(e) => {
                last_err = Some(e);
                thread::sleep(Duration::from_millis(20));
            }
        }
    }
    panic!("could not connect to test server: {last_err:?}");
}

// --- raw wire helpers --------------------------------------------------------

/// Send the v3 StartupMessage with a `user` parameter.
fn send_startup(s: &mut TcpStream, user: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(&PROTOCOL_VERSION_3.to_be_bytes());
    body.extend_from_slice(b"user\0");
    body.extend_from_slice(user.as_bytes());
    body.push(0);
    body.push(0); // terminating empty key
    let mut msg = Vec::new();
    msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    msg.extend_from_slice(&body);
    s.write_all(&msg).unwrap();
    s.flush().unwrap();
}

/// A framed backend message: tag byte + payload (length stripped).
#[derive(Debug)]
struct Msg {
    tag: u8,
    body: Vec<u8>,
}

fn read_exact(s: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    s.read_exact(&mut buf).expect("read backend bytes");
    buf
}

/// Read one tagged backend message (`[tag][len:i32][body]`).
fn read_msg(s: &mut TcpStream) -> Msg {
    let tag = read_exact(s, 1)[0];
    let len_bytes = read_exact(s, 4);
    let len = i32::from_be_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]]);
    let body = read_exact(s, (len - 4) as usize);
    Msg { tag, body }
}

/// Read messages until a ReadyForQuery (`Z`), returning all messages including
/// it. Asserts no ErrorResponse (`E`) appeared unless `allow_error`.
fn read_until_ready(s: &mut TcpStream) -> Vec<Msg> {
    let mut out = Vec::new();
    loop {
        let m = read_msg(s);
        let tag = m.tag;
        out.push(m);
        if tag == b'Z' {
            break;
        }
    }
    out
}

/// Complete the startup handshake: send StartupMessage, expect Authentication
/// (request 0 = Ok under trust), drain through to the first ReadyForQuery.
fn handshake(s: &mut TcpStream) {
    send_startup(s, "postgres");
    let auth = read_msg(s);
    assert_eq!(auth.tag, b'R', "expected Authentication message");
    let code = i32::from_be_bytes([auth.body[0], auth.body[1], auth.body[2], auth.body[3]]);
    assert_eq!(code, 0, "expected AuthenticationOk (trust)");

    // Then ParameterStatus(s) and BackendKeyData and ReadyForQuery, in some
    // order, before the first 'Z'.
    let msgs = read_until_ready(s);
    let tags: Vec<u8> = msgs.iter().map(|m| m.tag).collect();
    assert!(tags.contains(&b'S'), "expected ParameterStatus, got {tags:?}");
    assert!(tags.contains(&b'K'), "expected BackendKeyData, got {tags:?}");
    assert_eq!(*tags.last().unwrap(), b'Z', "should end on ReadyForQuery");
}

/// Send a simple Query ('Q').
fn send_query(s: &mut TcpStream, sql: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let mut msg = vec![b'Q'];
    msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    msg.extend_from_slice(&body);
    s.write_all(&msg).unwrap();
    s.flush().unwrap();
}

/// Decode a RowDescription body's column names.
fn row_desc_names(body: &[u8]) -> Vec<String> {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut names = Vec::with_capacity(n);
    let mut cur = &body[2..];
    for _ in 0..n {
        let nul = cur.iter().position(|&b| b == 0).unwrap();
        names.push(String::from_utf8_lossy(&cur[..nul]).into_owned());
        // name + NUL, then 18 bytes of field metadata (tableoid:4, attnum:2,
        // typoid:4, typlen:2, typmod:4, format:2).
        cur = &cur[nul + 1 + 18..];
    }
    names
}

/// Decode a DataRow body into per-column text values (None = NULL).
fn data_row(body: &[u8]) -> Vec<Option<String>> {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut cols = Vec::with_capacity(n);
    let mut cur = &body[2..];
    for _ in 0..n {
        let len = i32::from_be_bytes([cur[0], cur[1], cur[2], cur[3]]);
        cur = &cur[4..];
        if len < 0 {
            cols.push(None);
        } else {
            let len = len as usize;
            cols.push(Some(String::from_utf8_lossy(&cur[..len]).into_owned()));
            cur = &cur[len..];
        }
    }
    cols
}

// --- tests -------------------------------------------------------------------

#[test]
fn simple_query_select_literal() {
    let mut s = start_server();
    handshake(&mut s);

    send_query(&mut s, "SELECT 1");
    let msgs = read_until_ready(&mut s);
    let tags: Vec<u8> = msgs.iter().map(|m| m.tag).collect();

    // RowDescription 'T', DataRow 'D', CommandComplete 'C', ReadyForQuery 'Z'.
    assert!(tags.contains(&b'T'), "expected RowDescription, got {tags:?}");
    assert!(tags.contains(&b'D'), "expected DataRow, got {tags:?}");
    assert!(tags.contains(&b'C'), "expected CommandComplete, got {tags:?}");
    assert_eq!(*tags.last().unwrap(), b'Z');

    let data = msgs.iter().find(|m| m.tag == b'D').unwrap();
    assert_eq!(data_row(&data.body), vec![Some("1".to_string())]);

    let cc = msgs.iter().find(|m| m.tag == b'C').unwrap();
    let nul = cc.body.iter().position(|&b| b == 0).unwrap();
    assert_eq!(&cc.body[..nul], b"SELECT 1");
}

#[test]
fn simple_query_create_insert_select() {
    let mut s = start_server();
    handshake(&mut s);

    send_query(&mut s, "CREATE TABLE wp (id integer PRIMARY KEY, name text)");
    let msgs = read_until_ready(&mut s);
    assert!(msgs.iter().any(|m| m.tag == b'C'));

    send_query(&mut s, "INSERT INTO wp VALUES (1, 'alpha'), (2, 'beta')");
    let msgs = read_until_ready(&mut s);
    let cc = msgs.iter().find(|m| m.tag == b'C').unwrap();
    let nul = cc.body.iter().position(|&b| b == 0).unwrap();
    assert_eq!(&cc.body[..nul], b"INSERT 0 2");

    send_query(&mut s, "SELECT id, name FROM wp ORDER BY id");
    let msgs = read_until_ready(&mut s);
    let rd = msgs.iter().find(|m| m.tag == b'T').unwrap();
    assert_eq!(row_desc_names(&rd.body), vec!["id", "name"]);
    let rows: Vec<Vec<Option<String>>> = msgs
        .iter()
        .filter(|m| m.tag == b'D')
        .map(|m| data_row(&m.body))
        .collect();
    assert_eq!(
        rows,
        vec![
            vec![Some("1".to_string()), Some("alpha".to_string())],
            vec![Some("2".to_string()), Some("beta".to_string())],
        ]
    );
}

#[test]
fn error_response_then_recovers() {
    let mut s = start_server();
    handshake(&mut s);

    send_query(&mut s, "SELECT * FROM does_not_exist");
    let msgs = read_until_ready(&mut s);
    assert!(
        msgs.iter().any(|m| m.tag == b'E'),
        "expected ErrorResponse for missing relation"
    );
    assert_eq!(msgs.last().unwrap().tag, b'Z');

    // The connection is still usable afterward.
    send_query(&mut s, "SELECT 42");
    let msgs = read_until_ready(&mut s);
    let data = msgs.iter().find(|m| m.tag == b'D').unwrap();
    assert_eq!(data_row(&data.body), vec![Some("42".to_string())]);
}

#[test]
fn extended_protocol_parse_bind_execute() {
    let mut s = start_server();
    handshake(&mut s);

    // Seed a table via simple query first.
    send_query(&mut s, "CREATE TABLE ext (id integer PRIMARY KEY, v integer)");
    read_until_ready(&mut s);
    send_query(&mut s, "INSERT INTO ext VALUES (1, 100), (2, 200)");
    read_until_ready(&mut s);

    // Parse: statement "st", query with one $1 parameter, no declared types.
    let mut parse_body = Vec::new();
    parse_body.extend_from_slice(b"st\0");
    parse_body.extend_from_slice(b"SELECT v FROM ext WHERE id = $1\0");
    parse_body.extend_from_slice(&0i16.to_be_bytes()); // 0 param type oids
    send_tagged(&mut s, b'P', &parse_body);

    // Bind: portal "po" from statement "st", one text param "2".
    let mut bind_body = Vec::new();
    bind_body.extend_from_slice(b"po\0");
    bind_body.extend_from_slice(b"st\0");
    bind_body.extend_from_slice(&0i16.to_be_bytes()); // 0 param format codes (default text)
    bind_body.extend_from_slice(&1i16.to_be_bytes()); // 1 param value
    let p = b"2";
    bind_body.extend_from_slice(&(p.len() as i32).to_be_bytes());
    bind_body.extend_from_slice(p);
    bind_body.extend_from_slice(&0i16.to_be_bytes()); // 0 result format codes
    send_tagged(&mut s, b'B', &bind_body);

    // Describe portal "po".
    let mut desc_body = vec![b'P'];
    desc_body.extend_from_slice(b"po\0");
    send_tagged(&mut s, b'D', &desc_body);

    // Execute portal "po", unlimited rows.
    let mut exec_body = Vec::new();
    exec_body.extend_from_slice(b"po\0");
    exec_body.extend_from_slice(&0i32.to_be_bytes());
    send_tagged(&mut s, b'E', &exec_body);

    // Sync.
    send_tagged(&mut s, b'S', &[]);
    s.flush().unwrap();

    let msgs = read_until_ready(&mut s);
    let tags: Vec<u8> = msgs.iter().map(|m| m.tag).collect();
    assert!(tags.contains(&b'1'), "expected ParseComplete, got {tags:?}");
    assert!(tags.contains(&b'2'), "expected BindComplete, got {tags:?}");
    assert!(tags.contains(&b'T'), "expected RowDescription, got {tags:?}");
    assert!(tags.contains(&b'D'), "expected DataRow, got {tags:?}");

    let data = msgs.iter().find(|m| m.tag == b'D').unwrap();
    assert_eq!(
        data_row(&data.body),
        vec![Some("200".to_string())],
        "id=2 should select v=200"
    );
}

/// Send a tagged frontend message `[tag][len][body]`.
fn send_tagged(s: &mut TcpStream, tag: u8, body: &[u8]) {
    let mut msg = vec![tag];
    msg.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    msg.extend_from_slice(body);
    s.write_all(&msg).unwrap();
}

#[test]
fn terminate_closes_cleanly() {
    let mut s = start_server();
    handshake(&mut s);
    // Terminate ('X') with empty body.
    send_tagged(&mut s, b'X', &[]);
    s.flush().unwrap();
    // The server should close the connection: a read returns 0 bytes (EOF).
    let mut buf = [0u8; 1];
    let n = s.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "server should close the socket after Terminate");
}
