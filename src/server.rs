//! TCP server and per-connection session handling.
//!
//! Uses a thread per connection (mirroring PostgreSQL's process-per-backend
//! model) sharing one [`Database`] behind a mutex. This keeps the first
//! iteration dependency-free; a later iteration can move to async I/O and
//! finer-grained locking.

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::auth::ScramServer;
use crate::bind;
use crate::executor::{self, ExecResult, FieldDescription};
use crate::protocol::{FrontendMessage, MessageBuilder, Startup, read_message, read_startup};
use crate::sql::Parser;
use crate::sql::ast::{Copy as CopyStmt, CopyDirection, CopyFormat, Expr, Insert, Statement};
use crate::sql::serialize;
use crate::storage::Database;
use crate::types::{DataType, Value};
use crate::wal::Wal;

/// State shared across all connections: the database and (optional) WAL.
struct Shared {
    db: Mutex<Database>,
    /// `None` when running purely in memory (no `PGRS_DATA` configured).
    wal: Mutex<Option<Wal>>,
    /// Live backends keyed by their advertised pid, used to route
    /// cancellation requests and asynchronous notifications.
    backends: Mutex<HashMap<i32, BackendHandle>>,
    /// `LISTEN` registrations: channel name → set of listening backend pids.
    listeners: Mutex<HashMap<String, Vec<i32>>>,
}

/// Shared handles for one live backend, reachable from other connections.
struct BackendHandle {
    /// The cancel secret a client must echo in a CancelRequest.
    secret: i32,
    /// Set by a matching CancelRequest; checked at statement boundaries.
    cancel: Arc<AtomicBool>,
    /// Pending asynchronous notifications, drained when the session next
    /// reaches an idle (ReadyForQuery) point.
    notifications: Arc<Mutex<Vec<Notification>>>,
}

/// One `NOTIFY` payload destined for a listening backend.
#[derive(Clone)]
struct Notification {
    sender_pid: i32,
    channel: String,
    payload: String,
}

/// A backend severity/code/message destined for a NoticeResponse.
struct Notice {
    severity: &'static str,
    code: &'static str,
    message: String,
}

static NEXT_BACKEND_PID: AtomicI32 = AtomicI32::new(1);

/// The reported `server_version`, overridable via `PGRS_SERVER_VERSION` to
/// emulate a different PostgreSQL major version (compatibility mode).
fn server_version() -> String {
    std::env::var("PGRS_SERVER_VERSION")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "16.0 (postgres-rs 0.1.0)".to_string())
}

/// Bind to `addr` and serve connections until the process is stopped.
///
/// If `data_dir` is `Some`, the WAL there is replayed to restore state and all
/// subsequent mutations are persisted; otherwise the database is in-memory.
pub fn run(addr: &str, data_dir: Option<String>) -> io::Result<()> {
    let mut db = Database::new();

    let wal = match data_dir {
        Some(dir) => {
            let (wal, existing) = Wal::open(&dir)?;
            let replayed = replay(&mut db, &existing);
            println!("recovered {replayed} statement(s) from WAL in {dir}");
            Some(wal)
        }
        None => None,
    };

    let shared = Arc::new(Shared {
        db: Mutex::new(db),
        wal: Mutex::new(wal),
        backends: Mutex::new(HashMap::new()),
        listeners: Mutex::new(HashMap::new()),
    });
    let listener = TcpListener::bind(addr)?;
    println!("postgres-rs listening on {addr}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let shared = Arc::clone(&shared);
                thread::spawn(move || {
                    let peer = stream.peer_addr().ok();
                    if let Err(e) = handle_connection(stream, shared) {
                        // A clean disconnect surfaces as UnexpectedEof; don't
                        // log those as errors.
                        if e.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("connection {peer:?} ended: {e}");
                        }
                    }
                });
            }
            Err(e) => eprintln!("accept error: {e}"),
        }
    }
    Ok(())
}

/// A prepared statement created by Parse.
struct Prepared {
    statements: Vec<Statement>,
    param_oids: Vec<i32>,
}

/// A bound portal created by Bind, ready to Execute.
struct Portal {
    statements: Vec<Statement>,
    result_formats: Vec<i16>,
}

/// An in-progress transaction: a private working copy of the database plus the
/// SQL of mutations to flush to the WAL on COMMIT.
struct Transaction {
    /// Working copy the transaction reads and writes; discarded on ROLLBACK.
    db: Database,
    /// Serialized mutations, written to the WAL only when the transaction
    /// commits (so uncommitted work never reaches disk).
    buffered: Vec<String>,
    /// Named savepoints with a snapshot of the working copy and buffered WAL.
    savepoints: Vec<Savepoint>,
    /// Set once a statement in the block errors; further statements are
    /// rejected until COMMIT/ROLLBACK (matching PostgreSQL).
    failed: bool,
}

struct Savepoint {
    name: String,
    db: Database,
    buffered_len: usize,
}

/// Per-connection session state.
struct Session {
    /// 'I' idle, 'T' in transaction, 'E' failed transaction.
    tx_status: u8,
    /// `Some` while inside a `BEGIN ... COMMIT/ROLLBACK` block.
    tx: Option<Transaction>,
    prepared: HashMap<String, Prepared>,
    portals: HashMap<String, Portal>,
    /// In the extended protocol, once a message errors we discard input until
    /// the next Sync.
    skip_until_sync: bool,
    /// This backend's advertised pid (cancellation/notification key).
    pid: i32,
    /// Cancellation flag, shared with the backend handle in [`Shared`].
    cancel: Arc<AtomicBool>,
    /// Pending asynchronous notifications for this backend.
    notifications: Arc<Mutex<Vec<Notification>>>,
    /// Notices accumulated by the current statement, flushed before its result.
    notices: Vec<Notice>,
}

impl Session {
    fn new(pid: i32, cancel: Arc<AtomicBool>, notifications: Arc<Mutex<Vec<Notification>>>) -> Self {
        Session {
            tx_status: b'I',
            tx: None,
            prepared: HashMap::new(),
            portals: HashMap::new(),
            skip_until_sync: false,
            pid,
            cancel,
            notifications,
            notices: Vec::new(),
        }
    }
}

fn handle_connection(stream: TcpStream, shared: Arc<Shared>) -> io::Result<()> {
    stream.set_nodelay(true).ok();
    let read_half = stream.try_clone()?;
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(stream);

    // --- startup / negotiation ---
    let params = loop {
        match read_startup(&mut reader)? {
            Startup::SslRequest | Startup::GssEncRequest => {
                // We don't implement TLS yet: decline, client retries in clear.
                writer.write_all(b"N")?;
                writer.flush()?;
            }
            Startup::CancelRequest { pid, secret } => {
                // A cancel request is its own short-lived connection: flag the
                // target backend (if the secret matches) and disconnect.
                if let Some(handle) = shared.backends.lock().expect("backends mutex").get(&pid) {
                    if handle.secret == secret {
                        handle.cancel.store(true, Ordering::SeqCst);
                    }
                }
                return Ok(());
            }
            Startup::Params(p) => break p,
        }
    };

    // --- authentication ---
    // The `user` startup parameter is needed for MD5 (which hashes
    // password+username). Default to "postgres" if the client omitted it.
    let username = params
        .iter()
        .find(|(k, _)| k == "user")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| "postgres".to_string());

    if !authenticate(&mut reader, &mut writer, &username)? {
        send_error(&mut writer, "28P01", "password authentication failed")?;
        writer.flush()?;
        return Ok(());
    }

    let pid = NEXT_BACKEND_PID.fetch_add(1, Ordering::Relaxed);
    let secret = weak_secret();
    let cancel = Arc::new(AtomicBool::new(false));
    let notifications = Arc::new(Mutex::new(Vec::new()));
    shared.backends.lock().expect("backends mutex").insert(
        pid,
        BackendHandle {
            secret,
            cancel: Arc::clone(&cancel),
            notifications: Arc::clone(&notifications),
        },
    );
    // Ensure the backend is unregistered (and its LISTENs dropped) however this
    // connection ends.
    let _guard = ConnGuard {
        shared: &shared,
        pid,
    };

    send_authentication_ok(&mut writer)?;
    send_initial_parameters(&mut writer)?;
    send_backend_key_data(&mut writer, pid, secret)?;
    send_ready_for_query(&mut writer, b'I')?;
    writer.flush()?;

    // --- main message loop ---
    let mut session = Session::new(pid, cancel, notifications);
    loop {
        let Some(msg) = read_message(&mut reader)? else {
            break;
        };

        // While recovering from an extended-protocol error, swallow everything
        // until Sync.
        if session.skip_until_sync && !matches!(msg, FrontendMessage::Sync) {
            continue;
        }

        match msg {
            FrontendMessage::Query(sql) => {
                handle_simple_query(&mut reader, &mut writer, &shared, &mut session, &sql)?;
            }
            FrontendMessage::Parse {
                name,
                query,
                param_types,
            } => {
                handle_parse(&mut writer, &mut session, name, query, param_types)?;
            }
            FrontendMessage::Bind {
                portal,
                statement,
                params,
                param_formats,
                result_formats,
            } => {
                handle_bind(
                    &mut writer,
                    &mut session,
                    portal,
                    statement,
                    params,
                    param_formats,
                    result_formats,
                )?;
            }
            FrontendMessage::Describe { kind, name } => {
                handle_describe(&mut writer, &shared, &mut session, kind, &name)?;
            }
            FrontendMessage::Execute { portal, max_rows } => {
                handle_execute(&mut writer, &shared, &mut session, &portal, max_rows)?;
            }
            FrontendMessage::Close { kind, name } => {
                if kind == b'S' {
                    session.prepared.remove(&name);
                } else {
                    session.portals.remove(&name);
                }
                send_simple(&mut writer, b'3')?; // CloseComplete
            }
            FrontendMessage::Sync => {
                session.skip_until_sync = false;
                deliver_notifications(&mut writer, &session)?;
                send_ready_for_query(&mut writer, session.tx_status)?;
            }
            FrontendMessage::Flush => {}
            FrontendMessage::Password(_) => {
                // Trust auth never asks for one; ignore stray password messages.
            }
            FrontendMessage::CopyData(_) | FrontendMessage::CopyDone => {
                // COPY data outside an active COPY: ignore, as PostgreSQL does
                // for late data after a failed COPY.
            }
            FrontendMessage::CopyFail(_) => {
                send_error(&mut writer, "57014", "COPY from stdin failed")?;
                session.skip_until_sync = true;
            }
            FrontendMessage::Terminate => break,
            FrontendMessage::Unknown { tag, .. } => {
                send_error(
                    &mut writer,
                    "08P01",
                    &format!("unsupported protocol message '{}'", tag as char),
                )?;
                session.skip_until_sync = true;
            }
        }
        writer.flush()?;
    }
    Ok(())
}

/// Removes a backend from the shared registries when its connection ends,
/// regardless of how the session loop exits.
struct ConnGuard<'a> {
    shared: &'a Shared,
    pid: i32,
}

impl Drop for ConnGuard<'_> {
    fn drop(&mut self) {
        self.shared
            .backends
            .lock()
            .expect("backends mutex")
            .remove(&self.pid);
        self.shared
            .listeners
            .lock()
            .expect("listeners mutex")
            .values_mut()
            .for_each(|pids| pids.retain(|p| *p != self.pid));
    }
}

/// Drain and send any pending asynchronous notifications as NotificationResponse
/// (`'A'`) messages. Called whenever the session reaches an idle point.
fn deliver_notifications<W: Write>(w: &mut W, session: &Session) -> io::Result<()> {
    let pending: Vec<Notification> =
        std::mem::take(&mut *session.notifications.lock().expect("notifications mutex"));
    for n in pending {
        let mut b = MessageBuilder::new(b'A');
        b.put_i32(n.sender_pid);
        b.put_cstr(&n.channel);
        b.put_cstr(&n.payload);
        w.write_all(&b.finish())?;
    }
    Ok(())
}

/// Flush any notices accumulated by the just-run statement as NoticeResponse
/// (`'N'`) messages, which clients surface as warnings.
fn flush_notices<W: Write>(w: &mut W, session: &mut Session) -> io::Result<()> {
    for notice in session.notices.drain(..) {
        let mut b = MessageBuilder::new(b'N');
        b.put_u8(b'S').put_cstr(notice.severity);
        b.put_u8(b'V').put_cstr(notice.severity);
        b.put_u8(b'C').put_cstr(notice.code);
        b.put_u8(b'M').put_cstr(&notice.message);
        b.put_u8(0);
        w.write_all(&b.finish())?;
    }
    Ok(())
}

/// If a CancelRequest flagged this backend, consume the flag and return a
/// query-canceled error to abort the current batch.
fn check_canceled(session: &Session) -> Result<(), String> {
    if session.cancel.swap(false, Ordering::SeqCst) {
        Err("canceling statement due to user request".to_string())
    } else {
        Ok(())
    }
}

// --- simple query protocol ---------------------------------------------------

fn handle_simple_query<R: Read, W: Write>(
    reader: &mut R,
    w: &mut W,
    shared: &Shared,
    session: &mut Session,
    sql: &str,
) -> io::Result<()> {
    let statements = match Parser::parse_sql(sql) {
        Ok(s) => s,
        Err(e) => {
            send_error(w, "42601", &e)?;
            mark_error(session);
            return send_ready_for_query(w, session.tx_status);
        }
    };

    // A truly empty query string gets a dedicated response.
    if statements.iter().all(|s| matches!(s, Statement::Empty)) {
        send_simple(w, b'I')?; // EmptyQueryResponse
        return ready_for_query(w, session);
    }

    for stmt in statements {
        if matches!(stmt, Statement::Empty) {
            continue;
        }
        // A pending cancellation aborts the batch before the next statement.
        if let Err(e) = check_canceled(session) {
            send_error(w, sqlstate_for(&e), &e)?;
            mark_error(session);
            return ready_for_query(w, session);
        }
        // `COPY ... FROM STDIN`/`TO STDOUT` switch to the COPY sub-protocol,
        // which needs the connection's reader, so they're driven separately.
        if let Statement::Copy(copy) = &stmt {
            if let Err(e) = run_copy(reader, w, shared, session, copy)? {
                flush_notices(w, session)?;
                send_error(w, sqlstate_for(&e), &e)?;
                mark_error(session);
                return ready_for_query(w, session);
            }
            continue;
        }
        match run_statement(shared, session, &stmt) {
            Ok(res) => {
                flush_notices(w, session)?;
                send_result(w, res, &[])?;
            }
            Err(e) => {
                flush_notices(w, session)?;
                send_error(w, sqlstate_for(&e), &e)?;
                mark_error(session);
                // Abort the remainder of the simple-query batch.
                return ready_for_query(w, session);
            }
        }
    }

    ready_for_query(w, session)
}

/// Send ReadyForQuery, first delivering any pending asynchronous notifications
/// (the wire point at which PostgreSQL clients expect them).
fn ready_for_query<W: Write>(w: &mut W, session: &Session) -> io::Result<()> {
    deliver_notifications(w, session)?;
    send_ready_for_query(w, session.tx_status)
}

// --- COPY sub-protocol --------------------------------------------------------

/// Drive a `COPY ... FROM STDIN` / `TO STDOUT` exchange.
///
/// The outer `io::Result` covers socket failures (which end the connection);
/// the inner `Result` carries a SQL-level error for the caller to report as an
/// ErrorResponse + ReadyForQuery.
fn run_copy<R: Read, W: Write>(
    reader: &mut R,
    w: &mut W,
    shared: &Shared,
    session: &mut Session,
    copy: &CopyStmt,
) -> io::Result<Result<(), String>> {
    let columns = match copy_columns(shared, session, &copy.table, copy.columns.as_ref()) {
        Ok(c) => c,
        Err(e) => return Ok(Err(e)),
    };
    let delimiter = copy.delimiter.unwrap_or(match copy.format {
        CopyFormat::Csv => ',',
        CopyFormat::Text => '\t',
    });
    let null_marker = copy.null.clone().unwrap_or_else(|| match copy.format {
        CopyFormat::Csv => String::new(),
        CopyFormat::Text => "\\N".to_string(),
    });

    match copy.direction {
        CopyDirection::To => copy_to_stdout(w, shared, session, copy, &columns, delimiter, &null_marker),
        CopyDirection::From => {
            copy_from_stdin(reader, w, shared, session, copy, &columns, delimiter, &null_marker)
        }
    }
}

/// Resolve the effective column list for a COPY: the explicit list if given,
/// else every column of the table in declaration order.
fn copy_columns(
    shared: &Shared,
    session: &Session,
    table: &str,
    explicit: Option<&Vec<String>>,
) -> Result<Vec<String>, String> {
    if let Some(cols) = explicit {
        return Ok(cols.clone());
    }
    let names = |db: &Database| {
        db.table(table)
            .map(|t| t.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>())
    };
    let cols = match &session.tx {
        Some(tx) => names(&tx.db),
        None => names(&shared.db.lock().expect("db mutex poisoned")),
    };
    cols.ok_or_else(|| format!("relation \"{table}\" does not exist"))
}

#[allow(clippy::too_many_arguments)]
fn copy_to_stdout<W: Write>(
    w: &mut W,
    shared: &Shared,
    session: &mut Session,
    copy: &CopyStmt,
    columns: &[String],
    delimiter: char,
    null_marker: &str,
) -> io::Result<Result<(), String>> {
    // Reuse the SELECT path so transaction visibility and projection are exact.
    let col_list = columns
        .iter()
        .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT {col_list} FROM {}", copy.table);
    let stmt = match Parser::parse_sql(&sql) {
        Ok(mut s) if !s.is_empty() => s.remove(0),
        Ok(_) => return Ok(Err("COPY TO produced no query".into())),
        Err(e) => return Ok(Err(e)),
    };
    let rows = match run_statement(shared, session, &stmt) {
        Ok(ExecResult::Rows { rows, .. }) => rows,
        Ok(_) => return Ok(Err("COPY TO source did not return rows".into())),
        Err(e) => return Ok(Err(e)),
    };

    // CopyOutResponse: text format (0), one format code per column.
    let mut hdr = MessageBuilder::new(b'H');
    hdr.put_u8(0);
    hdr.put_i16(columns.len() as i16);
    for _ in columns {
        hdr.put_i16(0);
    }
    w.write_all(&hdr.finish())?;

    let csv = copy.format == CopyFormat::Csv;
    if copy.header && csv {
        let line = columns
            .iter()
            .map(|c| encode_copy_field(Some(c), delimiter, null_marker, csv))
            .collect::<Vec<_>>()
            .join(&delimiter.to_string());
        send_copy_data(w, &format!("{line}\n"))?;
    }
    for row in &rows {
        let line = row
            .iter()
            .map(|v| {
                let text = if v.is_null() { None } else { v.to_text() };
                encode_copy_field(text.as_deref(), delimiter, null_marker, csv)
            })
            .collect::<Vec<_>>()
            .join(&delimiter.to_string());
        send_copy_data(w, &format!("{line}\n"))?;
    }
    send_simple(w, b'c')?; // CopyDone
    send_command_complete(w, &format!("COPY {}", rows.len()))?;
    Ok(Ok(()))
}

#[allow(clippy::too_many_arguments)]
fn copy_from_stdin<R: Read, W: Write>(
    reader: &mut R,
    w: &mut W,
    shared: &Shared,
    session: &mut Session,
    copy: &CopyStmt,
    columns: &[String],
    delimiter: char,
    null_marker: &str,
) -> io::Result<Result<(), String>> {
    // CopyInResponse, then flush so the client starts streaming.
    let mut g = MessageBuilder::new(b'G');
    g.put_u8(0);
    g.put_i16(columns.len() as i16);
    for _ in columns {
        g.put_i16(0);
    }
    w.write_all(&g.finish())?;
    w.flush()?;

    // Accumulate all CopyData until CopyDone, then apply as inserts.
    let mut buf: Vec<u8> = Vec::new();
    loop {
        match read_message(reader)? {
            Some(FrontendMessage::CopyData(chunk)) => buf.extend_from_slice(&chunk),
            Some(FrontendMessage::CopyDone) => break,
            Some(FrontendMessage::CopyFail(msg)) => {
                return Ok(Err(format!("COPY from stdin failed: {msg}")));
            }
            Some(FrontendMessage::Flush) => {}
            // A clean EOF or Terminate mid-COPY ends the connection.
            None | Some(FrontendMessage::Terminate) => {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "COPY interrupted"));
            }
            Some(_) => {
                return Ok(Err("unexpected message during COPY FROM STDIN".into()));
            }
        }
    }

    let text = String::from_utf8_lossy(&buf);
    let csv = copy.format == CopyFormat::Csv;
    let mut count: usize = 0;
    let mut first = true;
    for raw_line in text.split('\n') {
        let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
        // Text-format end-of-data marker.
        if !csv && line == "\\." {
            break;
        }
        // A trailing newline yields one empty final segment; skip it.
        if line.is_empty() && raw_line.is_empty() {
            continue;
        }
        if first && copy.header {
            first = false;
            continue;
        }
        first = false;

        let fields = if csv {
            parse_csv_line(line, delimiter, null_marker)
        } else {
            parse_text_line(line, delimiter, null_marker)
        };
        let stmt = Statement::Insert(Insert {
            table: copy.table.clone(),
            columns: copy.columns.clone(),
            default_values: false,
            overriding_system_value: false,
            rows: vec![
                fields
                    .into_iter()
                    .map(|f| match f {
                        Some(s) => Expr::Str(s),
                        None => Expr::Null,
                    })
                    .collect(),
            ],
            select: None,
            on_conflict: None,
            returning: Vec::new(),
        });
        if let Err(e) = run_statement(shared, session, &stmt) {
            return Ok(Err(e));
        }
        count += 1;
    }

    send_command_complete(w, &format!("COPY {count}"))?;
    Ok(Ok(()))
}

/// Send one CopyData (`'d'`) message carrying a formatted row.
fn send_copy_data<W: Write>(w: &mut W, line: &str) -> io::Result<()> {
    let mut b = MessageBuilder::new(b'd');
    b.put_bytes(line.as_bytes());
    w.write_all(&b.finish())
}

/// Format one output field for COPY (text or CSV).
fn encode_copy_field(value: Option<&str>, delimiter: char, null_marker: &str, csv: bool) -> String {
    let Some(s) = value else {
        return null_marker.to_string();
    };
    if csv {
        // Quote if the field contains the delimiter, a quote, or a newline.
        if s.contains(delimiter) || s.contains('"') || s.contains('\n') || s.contains('\r') {
            format!("\"{}\"", s.replace('"', "\"\""))
        } else {
            s.to_string()
        }
    } else {
        // Text format: escape backslash and control characters.
        s.replace('\\', "\\\\")
            .replace('\t', "\\t")
            .replace('\n', "\\n")
            .replace('\r', "\\r")
    }
}

/// Parse a text-format COPY line into per-column fields.
fn parse_text_line(line: &str, delimiter: char, null_marker: &str) -> Vec<Option<String>> {
    line.split(delimiter)
        .map(|tok| {
            if tok == null_marker {
                None
            } else {
                Some(unescape_text_field(tok))
            }
        })
        .collect()
}

fn unescape_text_field(tok: &str) -> String {
    let mut out = String::with_capacity(tok.len());
    let mut chars = tok.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('t') => out.push('\t'),
                Some('n') => out.push('\n'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Parse a CSV-format COPY line, honoring double-quoted fields.
fn parse_csv_line(line: &str, delimiter: char, null_marker: &str) -> Vec<Option<String>> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut was_quoted = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
            was_quoted = true;
        } else if c == delimiter {
            fields.push(finish_csv_field(&cur, was_quoted, null_marker));
            cur.clear();
            was_quoted = false;
        } else {
            cur.push(c);
        }
    }
    fields.push(finish_csv_field(&cur, was_quoted, null_marker));
    fields
}

fn finish_csv_field(value: &str, was_quoted: bool, null_marker: &str) -> Option<String> {
    if !was_quoted && value == null_marker {
        None
    } else {
        Some(value.to_string())
    }
}

// --- extended query protocol -------------------------------------------------

fn handle_parse<W: Write>(
    w: &mut W,
    session: &mut Session,
    name: String,
    query: String,
    param_oids: Vec<i32>,
) -> io::Result<()> {
    match Parser::parse_sql(&query) {
        Ok(statements) => {
            session.prepared.insert(
                name,
                Prepared {
                    statements,
                    param_oids,
                },
            );
            send_simple(w, b'1') // ParseComplete
        }
        Err(e) => {
            send_error(w, "42601", &e)?;
            session.skip_until_sync = true;
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_bind<W: Write>(
    w: &mut W,
    session: &mut Session,
    portal: String,
    statement: String,
    raw_params: Vec<Option<Vec<u8>>>,
    param_formats: Vec<i16>,
    result_formats: Vec<i16>,
) -> io::Result<()> {
    let Some(prepared) = session.prepared.get(&statement) else {
        send_error(
            w,
            "26000",
            &format!("prepared statement \"{statement}\" does not exist"),
        )?;
        session.skip_until_sync = true;
        return Ok(());
    };

    // Decode each parameter using its format code and (optional) declared OID.
    let mut values = Vec::with_capacity(raw_params.len());
    for (i, raw) in raw_params.iter().enumerate() {
        let fmt = format_at(&param_formats, i);
        let oid = prepared.param_oids.get(i).copied().unwrap_or(0);
        match bind::decode_param(raw, fmt, oid) {
            Ok(v) => values.push(v),
            Err(e) => {
                send_error(w, "22P02", &e)?;
                session.skip_until_sync = true;
                return Ok(());
            }
        }
    }

    // Substitute parameters into a private copy of the statements.
    let mut statements = prepared.statements.clone();
    for stmt in &mut statements {
        if let Err(e) = bind::bind_statement(stmt, &values) {
            send_error(w, "08P01", &e)?;
            session.skip_until_sync = true;
            return Ok(());
        }
    }

    session.portals.insert(
        portal,
        Portal {
            statements,
            result_formats,
        },
    );
    send_simple(w, b'2') // BindComplete
}

fn handle_describe<W: Write>(
    w: &mut W,
    shared: &Shared,
    session: &mut Session,
    kind: u8,
    name: &str,
) -> io::Result<()> {
    if kind == b'S' {
        // Statement: ParameterDescription, then RowDescription or NoData.
        let Some(prepared) = session.prepared.get(name) else {
            send_error(
                w,
                "26000",
                &format!("prepared statement \"{name}\" does not exist"),
            )?;
            session.skip_until_sync = true;
            return Ok(());
        };
        send_parameter_description(w, &prepared.param_oids)?;
        let first = prepared.statements.first();
        describe_row_shape(w, shared, session.tx.as_ref().map(|t| &t.db), first)
    } else {
        // Portal: RowDescription or NoData.
        let Some(portal) = session.portals.get(name) else {
            send_error(w, "34000", &format!("portal \"{name}\" does not exist"))?;
            session.skip_until_sync = true;
            return Ok(());
        };
        let first = portal.statements.first();
        describe_row_shape(w, shared, session.tx.as_ref().map(|t| &t.db), first)
    }
}

fn describe_row_shape<W: Write>(
    w: &mut W,
    shared: &Shared,
    tx_db: Option<&Database>,
    stmt: Option<&Statement>,
) -> io::Result<()> {
    let fields = match stmt {
        Some(s) => match tx_db {
            // Inside a transaction, describe against its working copy.
            Some(db) => executor::describe_statement(db, s).ok().flatten(),
            None => {
                let guard = shared.db.lock().expect("db mutex poisoned");
                executor::describe_statement(&guard, s).ok().flatten()
            }
        },
        None => None,
    };
    match fields {
        Some(fields) => send_row_description(w, &fields, &[]),
        None => send_simple(w, b'n'), // NoData
    }
}

fn handle_execute<W: Write>(
    w: &mut W,
    shared: &Shared,
    session: &mut Session,
    portal_name: &str,
    _max_rows: i32,
) -> io::Result<()> {
    let Some(portal) = session.portals.get(portal_name) else {
        send_error(
            w,
            "34000",
            &format!("portal \"{portal_name}\" does not exist"),
        )?;
        session.skip_until_sync = true;
        return Ok(());
    };
    let statements = portal.statements.clone();
    let formats = portal.result_formats.clone();

    for stmt in &statements {
        if matches!(stmt, Statement::Empty) {
            send_simple(w, b'I')?; // EmptyQueryResponse
            continue;
        }
        if let Err(e) = check_canceled(session) {
            send_error(w, sqlstate_for(&e), &e)?;
            mark_error(session);
            session.skip_until_sync = true;
            return Ok(());
        }
        let result = run_statement(shared, session, stmt);
        match result {
            Ok(res) => {
                flush_notices(w, session)?;
                send_execute_result(w, res, &formats)?;
            }
            Err(e) => {
                flush_notices(w, session)?;
                send_error(w, sqlstate_for(&e), &e)?;
                mark_error(session);
                session.skip_until_sync = true;
                return Ok(());
            }
        }
    }
    Ok(())
}

// --- execution + transactions + durability -----------------------------------

/// Execute one statement, honoring transaction state.
///
/// - `BEGIN`/`COMMIT`/`ROLLBACK` manage a per-session [`Transaction`].
/// - Inside a transaction, statements run against the transaction's private
///   working copy; mutations are buffered and only applied + WAL-logged on
///   `COMMIT`. `ROLLBACK` discards the working copy.
/// - Outside a transaction (autocommit), statements run against the shared
///   database and mutations are logged immediately.
fn run_statement(
    shared: &Shared,
    session: &mut Session,
    stmt: &Statement,
) -> Result<ExecResult, String> {
    match stmt {
        Statement::Begin => {
            if session.tx.is_none() {
                let snapshot = shared.db.lock().expect("db mutex poisoned").clone();
                session.tx = Some(Transaction {
                    db: snapshot,
                    buffered: Vec::new(),
                    savepoints: Vec::new(),
                    failed: false,
                });
                session.tx_status = b'T';
            } else {
                // A nested BEGIN is a no-op; PostgreSQL emits a warning.
                session.notices.push(Notice {
                    severity: "WARNING",
                    code: "25001",
                    message: "there is already a transaction in progress".into(),
                });
            }
            Ok(ExecResult::Command("BEGIN".into()))
        }
        Statement::Listen { channel } => {
            let mut listeners = shared.listeners.lock().expect("listeners mutex");
            let pids = listeners.entry(channel.clone()).or_default();
            if !pids.contains(&session.pid) {
                pids.push(session.pid);
            }
            Ok(ExecResult::Command("LISTEN".into()))
        }
        Statement::Unlisten { channel } => {
            let mut listeners = shared.listeners.lock().expect("listeners mutex");
            match channel {
                Some(channel) => {
                    if let Some(pids) = listeners.get_mut(channel) {
                        pids.retain(|p| *p != session.pid);
                    }
                }
                None => listeners
                    .values_mut()
                    .for_each(|pids| pids.retain(|p| *p != session.pid)),
            }
            Ok(ExecResult::Command("UNLISTEN".into()))
        }
        Statement::Notify { channel, payload } => {
            let note = Notification {
                sender_pid: session.pid,
                channel: channel.clone(),
                payload: payload.clone().unwrap_or_default(),
            };
            let listeners = shared.listeners.lock().expect("listeners mutex");
            let backends = shared.backends.lock().expect("backends mutex");
            if let Some(pids) = listeners.get(channel) {
                for pid in pids {
                    if let Some(handle) = backends.get(pid) {
                        handle
                            .notifications
                            .lock()
                            .expect("notifications mutex")
                            .push(note.clone());
                    }
                }
            }
            Ok(ExecResult::Command("NOTIFY".into()))
        }
        Statement::Commit => match session.tx.take() {
            Some(tx) if tx.failed => {
                // Committing an aborted transaction rolls it back.
                session.tx_status = b'I';
                Ok(ExecResult::Command("ROLLBACK".into()))
            }
            Some(tx) => {
                // Publish the working copy, then durably log the mutations.
                *shared.db.lock().expect("db mutex poisoned") = tx.db;
                if let Some(wal) = shared.wal.lock().expect("wal mutex poisoned").as_mut() {
                    for sql in &tx.buffered {
                        if let Err(e) = wal.append(sql) {
                            eprintln!("warning: WAL append failed: {e}");
                        }
                    }
                }
                session.tx_status = b'I';
                Ok(ExecResult::Command("COMMIT".into()))
            }
            None => {
                session.notices.push(Notice {
                    severity: "WARNING",
                    code: "25P01",
                    message: "there is no transaction in progress".into(),
                });
                Ok(ExecResult::Command("COMMIT".into()))
            }
        },
        Statement::Rollback => {
            if session.tx.take().is_none() {
                session.notices.push(Notice {
                    severity: "WARNING",
                    code: "25P01",
                    message: "there is no transaction in progress".into(),
                });
            }
            session.tx_status = b'I';
            Ok(ExecResult::Command("ROLLBACK".into()))
        }
        Statement::Savepoint { name } => {
            let tx = session
                .tx
                .as_mut()
                .ok_or_else(|| "SAVEPOINT can only be used in transaction blocks".to_string())?;
            tx.savepoints.retain(|sp| sp.name != *name);
            tx.savepoints.push(Savepoint {
                name: name.clone(),
                db: tx.db.clone(),
                buffered_len: tx.buffered.len(),
            });
            Ok(ExecResult::Command("SAVEPOINT".into()))
        }
        Statement::ReleaseSavepoint { name } => {
            let tx = session.tx.as_mut().ok_or_else(|| {
                "RELEASE SAVEPOINT can only be used in transaction blocks".to_string()
            })?;
            let pos = tx
                .savepoints
                .iter()
                .rposition(|sp| sp.name == *name)
                .ok_or_else(|| format!("savepoint \"{name}\" does not exist"))?;
            tx.savepoints.truncate(pos);
            Ok(ExecResult::Command("RELEASE".into()))
        }
        Statement::RollbackToSavepoint { name } => {
            let tx = session.tx.as_mut().ok_or_else(|| {
                "ROLLBACK TO SAVEPOINT can only be used in transaction blocks".to_string()
            })?;
            let pos = tx
                .savepoints
                .iter()
                .rposition(|sp| sp.name == *name)
                .ok_or_else(|| format!("savepoint \"{name}\" does not exist"))?;
            let sp = &tx.savepoints[pos];
            tx.db = sp.db.clone();
            tx.buffered.truncate(sp.buffered_len);
            tx.savepoints.truncate(pos + 1);
            tx.failed = false;
            session.tx_status = b'T';
            Ok(ExecResult::Command("ROLLBACK".into()))
        }
        _ if session.tx.is_some() => {
            // Run against the transaction's working copy.
            let res = {
                let tx = session.tx.as_mut().unwrap();
                if tx.failed {
                    return Err(
                        "current transaction is aborted, commands ignored until end of transaction block".into(),
                    );
                }
                let res = executor::execute(&mut tx.db, stmt.clone());
                match &res {
                    Ok(_) if is_mutation(stmt) => {
                        tx.buffered.push(serialize::statement_to_sql(stmt));
                    }
                    Err(_) => tx.failed = true,
                    _ => {}
                }
                res
            };
            if res.is_err() {
                session.tx_status = b'E';
            }
            res
        }
        _ => execute_autocommit(shared, stmt),
    }
}

/// Execute one statement against the shared database and, if it mutates state
/// and succeeds, durably append it to the WAL before releasing the lock.
/// Holding the db lock across the append keeps WAL order == execution order.
fn execute_autocommit(shared: &Shared, stmt: &Statement) -> Result<ExecResult, String> {
    let mut db = shared.db.lock().expect("db mutex poisoned");
    let result = executor::execute(&mut db, stmt.clone());

    if result.is_ok() && is_mutation(stmt) {
        if let Some(wal) = shared.wal.lock().expect("wal mutex poisoned").as_mut() {
            let sql = serialize::statement_to_sql(stmt);
            if let Err(e) = wal.append(&sql) {
                eprintln!("warning: WAL append failed: {e}");
            }
        }
    }
    result
}

/// Record that a statement errored, transitioning a transaction to the failed
/// state. Outside a transaction, autocommit errors leave the session idle.
fn mark_error(session: &mut Session) {
    if session.tx.is_some() {
        session.tx.as_mut().unwrap().failed = true;
        session.tx_status = b'E';
    }
}

/// Replay WAL contents into a fresh database at startup. Returns the number of
/// statements applied. Replay does not re-log (no WAL is attached yet).
fn replay(db: &mut Database, contents: &str) -> usize {
    if contents.trim().is_empty() {
        return 0;
    }
    let statements = match Parser::parse_sql(contents) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("warning: failed to parse WAL, starting empty: {e}");
            return 0;
        }
    };
    let mut applied = 0;
    for stmt in statements {
        if matches!(stmt, Statement::Empty) {
            continue;
        }
        match executor::execute(db, stmt) {
            Ok(_) => applied += 1,
            Err(e) => eprintln!("warning: WAL replay error (skipped): {e}"),
        }
    }
    applied
}

/// Whether a statement changes persistent state and must be logged.
fn is_mutation(stmt: &Statement) -> bool {
    match stmt {
        Statement::CreateTable(_)
        | Statement::CreateExtension(_)
        | Statement::CreateRole(_)
        | Statement::CreateSequence(_)
        | Statement::CreateSchema(_)
        | Statement::CreateDatabase(_)
        | Statement::CreateTablespace(_)
        | Statement::CreateCollation(_)
        | Statement::CreateType(_)
        | Statement::CreateDomain(_)
        | Statement::CreateView(_)
        | Statement::CreateMaterializedView(_)
        | Statement::CreateFunction(_)
        | Statement::CreateTrigger(_)
        | Statement::CreateRule(_)
        | Statement::CreateAggregate(_)
        | Statement::DropFunction(_)
        | Statement::DropTrigger(_)
        | Statement::DropRule(_)
        | Statement::DropAggregate(_)
        | Statement::DropTable(_)
        | Statement::DropExtension(_)
        | Statement::DropRole(_)
        | Statement::DropSequence(_)
        | Statement::DropSchema(_)
        | Statement::DropDatabase(_)
        | Statement::DropTablespace(_)
        | Statement::DropCollation(_)
        | Statement::DropType(_)
        | Statement::DropDomain(_)
        | Statement::DropView(_)
        | Statement::DropMaterializedView(_)
        | Statement::RefreshMaterializedView(_)
        | Statement::AlterTable(_)
        | Statement::AlterRole(_)
        | Statement::AlterSequence(_)
        | Statement::SecurityLabel(_)
        | Statement::AlterSystem(_)
        | Statement::CreateIndex(_)
        | Statement::DropIndex(_)
        | Statement::Comment(_)
        | Statement::Grant(_)
        | Statement::Revoke(_)
        | Statement::Insert(_)
        | Statement::Truncate(_)
        | Statement::AlterDatabase(_)
        | Statement::Update(_)
        | Statement::Delete(_)
        | Statement::Merge(_) => true,
        Statement::Explain(e) if e.analyze => is_mutation(&e.statement),
        _ => false,
    }
}

// --- result serialization ----------------------------------------------------

/// Simple-query result: includes RowDescription before any rows.
fn send_result<W: Write>(w: &mut W, res: ExecResult, formats: &[i16]) -> io::Result<()> {
    match res {
        ExecResult::Rows { fields, rows, tag } => {
            send_row_description(w, &fields, formats)?;
            send_data_rows(w, &fields, &rows, formats)?;
            send_command_complete(w, &tag)
        }
        ExecResult::Command(tag) => send_command_complete(w, &tag),
        ExecResult::Empty => send_simple(w, b'I'),
    }
}

/// Extended-protocol Execute result: RowDescription was already sent at
/// Describe time, so we emit only data rows + completion.
fn send_execute_result<W: Write>(w: &mut W, res: ExecResult, formats: &[i16]) -> io::Result<()> {
    match res {
        ExecResult::Rows { fields, rows, tag } => {
            send_data_rows(w, &fields, &rows, formats)?;
            send_command_complete(w, &tag)
        }
        ExecResult::Command(tag) => send_command_complete(w, &tag),
        ExecResult::Empty => send_simple(w, b'I'),
    }
}

fn send_row_description<W: Write>(
    w: &mut W,
    fields: &[FieldDescription],
    formats: &[i16],
) -> io::Result<()> {
    let mut b = MessageBuilder::new(b'T');
    b.put_i16(fields.len() as i16);
    for (i, f) in fields.iter().enumerate() {
        b.put_cstr(&f.name);
        b.put_i32(0); // table OID (unknown)
        b.put_i16(0); // column attribute number
        b.put_i32(f.data_type.oid());
        b.put_i16(f.data_type.type_size());
        b.put_i32(-1); // type modifier
        b.put_i16(format_at(formats, i)); // format code
    }
    w.write_all(&b.finish())
}

fn send_data_rows<W: Write>(
    w: &mut W,
    fields: &[FieldDescription],
    rows: &[Vec<Value>],
    formats: &[i16],
) -> io::Result<()> {
    for row in rows {
        let mut b = MessageBuilder::new(b'D');
        b.put_i16(row.len() as i16);
        for (i, val) in row.iter().enumerate() {
            let fmt = format_at(formats, i);
            let dt = fields.get(i).map(|f| f.data_type).unwrap_or(DataType::Text);
            match encode_value(val, dt, fmt) {
                Some(bytes) => {
                    b.put_i32(bytes.len() as i32);
                    b.put_bytes(&bytes);
                }
                None => {
                    b.put_i32(-1); // SQL NULL
                }
            }
        }
        w.write_all(&b.finish())?;
    }
    Ok(())
}

/// Encode one value in the requested format. Returns `None` for NULL.
fn encode_value(v: &Value, dt: DataType, format: i16) -> Option<Vec<u8>> {
    if v.is_null() {
        return None;
    }
    if format == 0 {
        return v.to_text().map(String::into_bytes);
    }
    // Binary format, encoded according to the column's declared type.
    Some(match dt {
        DataType::Bool => vec![if v.is_true() { 1 } else { 0 }],
        DataType::Int2 => (as_i64(v) as i16).to_be_bytes().to_vec(),
        DataType::Int4 => (as_i64(v) as i32).to_be_bytes().to_vec(),
        DataType::Int8 => as_i64(v).to_be_bytes().to_vec(),
        DataType::Float4 => (as_f64(v) as f32).to_be_bytes().to_vec(),
        DataType::Float8 => as_f64(v).to_be_bytes().to_vec(),
        // numeric and the text-stored types fall back to their text form
        // (best-effort; most clients request text format for these).
        _ => v.to_text().unwrap_or_default().into_bytes(),
    })
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        Value::Float(f) => *f as i64,
        Value::Bool(b) => *b as i64,
        Value::Text(s) => s.parse().unwrap_or(0),
        Value::Null => 0,
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        Value::Bool(b) => *b as i64 as f64,
        Value::Text(s) => s.parse().unwrap_or(0.0),
        Value::Null => 0.0,
    }
}

// --- small backend messages --------------------------------------------------

fn send_authentication_ok<W: Write>(w: &mut W) -> io::Result<()> {
    let mut b = MessageBuilder::new(b'R');
    b.put_i32(0); // AuthenticationOk
    w.write_all(&b.finish())
}

/// Dispatch to the configured authentication method and return whether the
/// client successfully authenticated. `Ok(false)` means the caller should send
/// an ErrorResponse (28P01) and close the connection.
///
/// Method selection (`PGRS_AUTH_METHOD`):
/// - unset: legacy behavior — SCRAM if `PGRS_PASSWORD` is set and non-empty,
///   otherwise trust (no challenge).
/// - `trust`:    accept immediately, no challenge.
/// - `password`: AuthenticationCleartextPassword, compare to `PGRS_PASSWORD`.
/// - `md5`:      AuthenticationMD5Password, compare the salted MD5 digest.
/// - `scram`:    SCRAM-SHA-256 against `PGRS_PASSWORD`.
fn authenticate<R: io::Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    username: &str,
) -> io::Result<bool> {
    let method = std::env::var("PGRS_AUTH_METHOD")
        .ok()
        .filter(|m| !m.is_empty());
    let password = std::env::var("PGRS_PASSWORD").ok().unwrap_or_default();

    match method.as_deref() {
        // Legacy default: SCRAM when a password is configured, else trust.
        None => {
            if !password.is_empty() {
                scram_authenticate(reader, writer, &password)
            } else {
                Ok(true)
            }
        }
        Some("trust") => Ok(true),
        Some("password") => cleartext_authenticate(reader, writer, &password),
        Some("md5") => md5_authenticate(reader, writer, &password, username),
        Some("scram") => scram_authenticate(reader, writer, &password),
        // An unrecognized value is treated as trust rather than locking out.
        Some(_) => Ok(true),
    }
}

/// Run a cleartext password exchange (AuthenticationCleartextPassword, code 3).
/// The client replies with a 'p' message carrying a NUL-terminated password.
fn cleartext_authenticate<R: io::Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    expected: &str,
) -> io::Result<bool> {
    let mut b = MessageBuilder::new(b'R');
    b.put_i32(3);
    writer.write_all(&b.finish())?;
    writer.flush()?;

    let Some(FrontendMessage::Password(body)) = read_message(reader)? else {
        return Ok(false);
    };
    Ok(strip_nul(&body) == expected.as_bytes())
}

/// Run an MD5 password exchange (AuthenticationMD5Password, code 5). The 4-byte
/// salt is derived from the existing non-RNG `weak_secret()`/clock source.
fn md5_authenticate<R: io::Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    password: &str,
    username: &str,
) -> io::Result<bool> {
    let salt = weak_secret().to_be_bytes(); // 4 bytes

    let mut b = MessageBuilder::new(b'R');
    b.put_i32(5);
    b.put_bytes(&salt);
    writer.write_all(&b.finish())?;
    writer.flush()?;

    // The client replies with a NUL-terminated "md5<hex>" string.
    let Some(FrontendMessage::Password(body)) = read_message(reader)? else {
        return Ok(false);
    };
    let expected = crate::auth::md5_password_digest(password, username, &salt);
    Ok(strip_nul(&body) == expected.as_bytes())
}

/// Strip a single trailing NUL terminator from a password message body.
fn strip_nul(body: &[u8]) -> &[u8] {
    match body.strip_suffix(&[0]) {
        Some(s) => s,
        None => body,
    }
}

/// Run a SCRAM-SHA-256 exchange. Returns `Ok(true)` if the client proved the
/// password, `Ok(false)` if authentication failed (caller reports the error).
fn scram_authenticate<R: io::Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    password: &str,
) -> io::Result<bool> {
    // AuthenticationSASL (code 10): advertise the mechanism list.
    let mut b = MessageBuilder::new(b'R');
    b.put_i32(10);
    b.put_cstr("SCRAM-SHA-256");
    b.put_u8(0); // end of mechanism list
    writer.write_all(&b.finish())?;
    writer.flush()?;

    // SASLInitialResponse arrives as a password ('p') message.
    let Some(FrontendMessage::Password(body)) = read_message(reader)? else {
        return Ok(false);
    };
    let Some(client_first) = parse_sasl_initial(&body) else {
        return Ok(false);
    };

    let mut scram = ScramServer::new(password);
    let server_first = match scram.server_first(&client_first) {
        Ok(v) => v,
        Err(_) => return Ok(false),
    };
    // AuthenticationSASLContinue (code 11).
    let mut b = MessageBuilder::new(b'R');
    b.put_i32(11);
    b.put_bytes(&server_first);
    writer.write_all(&b.finish())?;
    writer.flush()?;

    // SASLResponse: the client-final message (whole payload).
    let Some(FrontendMessage::Password(client_final)) = read_message(reader)? else {
        return Ok(false);
    };
    match scram.server_final(&client_final) {
        Ok(server_final) => {
            // AuthenticationSASLFinal (code 12) with the server signature.
            let mut b = MessageBuilder::new(b'R');
            b.put_i32(12);
            b.put_bytes(&server_final);
            writer.write_all(&b.finish())?;
            Ok(true)
        }
        Err(_) => Ok(false),
    }
}

/// Extract the client-first payload from a SASLInitialResponse body:
/// `mechanism-name \0 Int32(len) client-first-bytes`.
fn parse_sasl_initial(body: &[u8]) -> Option<Vec<u8>> {
    let nul = body.iter().position(|&b| b == 0)?;
    let rest = &body[nul + 1..];
    if rest.len() < 4 {
        return None;
    }
    let len = i32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
    let data = &rest[4..];
    if len < 0 {
        return Some(Vec::new());
    }
    let len = (len as usize).min(data.len());
    Some(data[..len].to_vec())
}

fn send_initial_parameters<W: Write>(w: &mut W) -> io::Result<()> {
    let version = server_version();
    let params = [
        ("server_version", version.as_str()),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("DateStyle", "ISO, MDY"),
        ("TimeZone", "UTC"),
        ("integer_datetimes", "on"),
        ("standard_conforming_strings", "on"),
        ("application_name", ""),
    ];
    for (k, v) in params {
        let mut b = MessageBuilder::new(b'S');
        b.put_cstr(k);
        b.put_cstr(v);
        w.write_all(&b.finish())?;
    }
    Ok(())
}

fn send_backend_key_data<W: Write>(w: &mut W, pid: i32, secret: i32) -> io::Result<()> {
    let mut b = MessageBuilder::new(b'K');
    b.put_i32(pid);
    b.put_i32(secret);
    w.write_all(&b.finish())
}

fn send_ready_for_query<W: Write>(w: &mut W, status: u8) -> io::Result<()> {
    let mut b = MessageBuilder::new(b'Z');
    b.put_u8(status);
    w.write_all(&b.finish())
}

fn send_command_complete<W: Write>(w: &mut W, tag: &str) -> io::Result<()> {
    let mut b = MessageBuilder::new(b'C');
    b.put_cstr(tag);
    w.write_all(&b.finish())
}

fn send_parameter_description<W: Write>(w: &mut W, oids: &[i32]) -> io::Result<()> {
    let mut b = MessageBuilder::new(b't');
    b.put_i16(oids.len() as i16);
    for &oid in oids {
        b.put_i32(oid);
    }
    w.write_all(&b.finish())
}

/// Send a message with a tag and an empty body (ParseComplete, BindComplete,
/// CloseComplete, NoData, EmptyQueryResponse).
fn send_simple<W: Write>(w: &mut W, tag: u8) -> io::Result<()> {
    w.write_all(&MessageBuilder::new(tag).finish())
}

fn send_error<W: Write>(w: &mut W, code: &str, message: &str) -> io::Result<()> {
    let mut b = MessageBuilder::new(b'E');
    b.put_u8(b'S').put_cstr("ERROR");
    b.put_u8(b'V').put_cstr("ERROR");
    b.put_u8(b'C').put_cstr(code);
    b.put_u8(b'M').put_cstr(message);
    b.put_u8(0); // field terminator
    w.write_all(&b.finish())
}

// --- helpers -----------------------------------------------------------------

/// Format code for column/parameter `i`: zero codes means all-text, one code
/// applies to all, otherwise it's per-position.
fn format_at(formats: &[i16], i: usize) -> i16 {
    match formats.len() {
        0 => 0,
        1 => formats[0],
        _ => formats.get(i).copied().unwrap_or(0),
    }
}

/// Map an error message to a plausible SQLSTATE so clients categorize it.
fn sqlstate_for(msg: &str) -> &'static str {
    if msg.contains("does not exist") {
        "42P01" // undefined_table/column
    } else if msg.contains("already exists") {
        "42P07" // duplicate_table
    } else if msg.contains("division by zero") {
        "22012"
    } else if msg.contains("not-null") {
        "23502"
    } else if msg.contains("syntax") || msg.contains("expected") || msg.contains("unexpected") {
        "42601"
    } else if msg.contains("invalid input syntax") || msg.contains("cannot coerce") {
        "22P02"
    } else if msg.contains("canceling statement") {
        "57014" // query_canceled
    } else {
        "XX000" // internal_error
    }
}

/// A weak per-connection cancel secret. Not cryptographically secure, but the
/// cancel path is not implemented yet, so this only needs to be present.
fn weak_secret() -> i32 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as i32)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Shared` with no WAL (in-memory), for exercising `run_statement`.
    fn shared() -> Shared {
        Shared {
            db: Mutex::new(Database::new()),
            wal: Mutex::new(None),
            backends: Mutex::new(HashMap::new()),
            listeners: Mutex::new(HashMap::new()),
        }
    }

    /// A bare session with dummy cancellation/notification handles.
    fn new_session() -> Session {
        Session::new(1, Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())))
    }

    fn run(shared: &Shared, session: &mut Session, sql: &str) -> Result<(), String> {
        for stmt in Parser::parse_sql(sql).map_err(|e| e.to_string())? {
            if matches!(stmt, Statement::Empty) {
                continue;
            }
            run_statement(shared, session, &stmt)?;
        }
        Ok(())
    }

    /// Number of rows in `t` in the shared (committed) database.
    fn committed_rows(shared: &Shared) -> usize {
        shared
            .db
            .lock()
            .unwrap()
            .table("t")
            .map(|t| t.rows.len())
            .unwrap_or(0)
    }

    #[test]
    fn rollback_undoes_changes() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer)").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (1)").unwrap();
        run(&s, &mut sess, "BEGIN").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (2)").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (3)").unwrap();
        assert_eq!(sess.tx_status, b'T');
        run(&s, &mut sess, "ROLLBACK").unwrap();
        // Only the pre-transaction row remains.
        assert_eq!(committed_rows(&s), 1);
        assert_eq!(sess.tx_status, b'I');
    }

    #[test]
    fn commit_publishes_changes() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer)").unwrap();
        run(&s, &mut sess, "BEGIN").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (1)").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (2)").unwrap();
        // Not yet visible in the shared db.
        assert_eq!(committed_rows(&s), 0);
        run(&s, &mut sess, "COMMIT").unwrap();
        assert_eq!(committed_rows(&s), 2);
    }

    #[test]
    fn savepoint_rolls_back_to_named_snapshot() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer)").unwrap();
        run(&s, &mut sess, "BEGIN").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (1)").unwrap();
        run(&s, &mut sess, "SAVEPOINT a").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (2)").unwrap();
        run(&s, &mut sess, "SAVEPOINT b").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (3)").unwrap();
        run(&s, &mut sess, "ROLLBACK TO SAVEPOINT a").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (4)").unwrap();
        run(&s, &mut sess, "COMMIT").unwrap();

        let rows = {
            let mut db = s.db.lock().unwrap();
            match executor::execute(
                &mut db,
                Parser::parse_sql("SELECT id FROM t ORDER BY id")
                    .unwrap()
                    .remove(0),
            )
            .unwrap()
            {
                ExecResult::Rows { rows, .. } => rows,
                _ => panic!("expected rows"),
            }
        };
        assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(4)]]);
    }

    #[test]
    fn release_savepoint_removes_it() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer)").unwrap();
        run(&s, &mut sess, "BEGIN").unwrap();
        run(&s, &mut sess, "SAVEPOINT a").unwrap();
        run(&s, &mut sess, "RELEASE SAVEPOINT a").unwrap();
        let stmt = Parser::parse_sql("ROLLBACK TO SAVEPOINT a")
            .unwrap()
            .remove(0);
        assert!(run_statement(&s, &mut sess, &stmt).is_err());
        run(&s, &mut sess, "ROLLBACK").unwrap();
    }

    #[test]
    fn error_aborts_transaction() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer)").unwrap();
        run(&s, &mut sess, "BEGIN").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (1)").unwrap();

        // A failing statement aborts the transaction.
        let stmt = Parser::parse_sql("SELECT * FROM missing")
            .unwrap()
            .remove(0);
        assert!(run_statement(&s, &mut sess, &stmt).is_err());
        mark_error(&mut sess);
        assert_eq!(sess.tx_status, b'E');

        // Subsequent statements are rejected until the block ends.
        let stmt = Parser::parse_sql("INSERT INTO t VALUES (2)")
            .unwrap()
            .remove(0);
        assert!(run_statement(&s, &mut sess, &stmt).is_err());

        // COMMIT of an aborted transaction discards all of its work.
        run(&s, &mut sess, "COMMIT").unwrap();
        assert_eq!(committed_rows(&s), 0);
        assert_eq!(sess.tx_status, b'I');
    }

    /// Frame one frontend message (`[tag][len][body]`) for a fake client stream.
    fn frame(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        out.extend(((body.len() + 4) as i32).to_be_bytes());
        out.extend_from_slice(body);
        out
    }

    fn parse_copy(sql: &str) -> CopyStmt {
        match Parser::parse_sql(sql).unwrap().remove(0) {
            Statement::Copy(c) => c,
            other => panic!("expected COPY, got {other:?}"),
        }
    }

    #[test]
    fn copy_from_stdin_inserts_rows() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer, name text)").unwrap();

        let mut input = Vec::new();
        input.extend(frame(b'd', b"1\tAlice\n2\tBob\n"));
        input.extend(frame(b'c', b"")); // CopyDone
        let mut reader = std::io::Cursor::new(input);
        let mut out = Vec::new();

        let copy = parse_copy("COPY t FROM STDIN");
        let res = run_copy(&mut reader, &mut out, &s, &mut sess, &copy).unwrap();
        assert!(res.is_ok(), "copy failed: {res:?}");
        assert_eq!(committed_rows(&s), 2);
        // Server announced CopyInResponse and finished with a COPY tag.
        assert_eq!(out[0], b'G');
        assert!(window_contains(&out, b"COPY 2"));
    }

    #[test]
    fn copy_from_stdin_csv_skips_header_and_handles_nulls() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer, name text)").unwrap();

        let mut input = Vec::new();
        input.extend(frame(b'd', b"id,name\n1,\"Alice, A\"\n2,\n"));
        input.extend(frame(b'c', b""));
        let mut reader = std::io::Cursor::new(input);
        let mut out = Vec::new();

        let copy = parse_copy("COPY t FROM STDIN WITH (FORMAT csv, HEADER)");
        run_copy(&mut reader, &mut out, &s, &mut sess, &copy)
            .unwrap()
            .unwrap();

        let rows = {
            let mut db = s.db.lock().unwrap();
            match executor::execute(
                &mut db,
                Parser::parse_sql("SELECT id, name FROM t ORDER BY id")
                    .unwrap()
                    .remove(0),
            )
            .unwrap()
            {
                ExecResult::Rows { rows, .. } => rows,
                _ => panic!("expected rows"),
            }
        };
        assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Text("Alice, A".into())],
                vec![Value::Int(2), Value::Null],
            ]
        );
    }

    #[test]
    fn copy_to_stdout_streams_rows() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer, name text)").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (1, 'Alice'), (2, 'Bob')").unwrap();

        let mut reader = std::io::Cursor::new(Vec::new());
        let mut out = Vec::new();
        let copy = parse_copy("COPY t TO STDOUT");
        run_copy(&mut reader, &mut out, &s, &mut sess, &copy)
            .unwrap()
            .unwrap();

        assert_eq!(out[0], b'H'); // CopyOutResponse
        assert!(window_contains(&out, b"1\tAlice"));
        assert!(window_contains(&out, b"2\tBob"));
        assert!(window_contains(&out, b"COPY 2"));
    }

    #[test]
    fn notify_reaches_a_listening_backend() {
        let s = shared();
        // Register a listener backend (pid 1) with its own notification queue.
        let queue = Arc::new(Mutex::new(Vec::new()));
        s.backends.lock().unwrap().insert(
            1,
            BackendHandle {
                secret: 0,
                cancel: Arc::new(AtomicBool::new(false)),
                notifications: Arc::clone(&queue),
            },
        );
        let mut listener = Session::new(1, Arc::new(AtomicBool::new(false)), Arc::clone(&queue));
        run(&s, &mut listener, "LISTEN chan").unwrap();

        // A different backend (pid 2) fires the notification.
        let mut notifier = Session::new(
            2,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
        );
        run(&s, &mut notifier, "NOTIFY chan, 'hello'").unwrap();

        let delivered = queue.lock().unwrap();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].channel, "chan");
        assert_eq!(delivered[0].payload, "hello");
        assert_eq!(delivered[0].sender_pid, 2);
    }

    #[test]
    fn unlisten_stops_delivery() {
        let s = shared();
        let queue = Arc::new(Mutex::new(Vec::new()));
        s.backends.lock().unwrap().insert(
            1,
            BackendHandle {
                secret: 0,
                cancel: Arc::new(AtomicBool::new(false)),
                notifications: Arc::clone(&queue),
            },
        );
        let mut sess = Session::new(1, Arc::new(AtomicBool::new(false)), Arc::clone(&queue));
        run(&s, &mut sess, "LISTEN chan").unwrap();
        run(&s, &mut sess, "UNLISTEN chan").unwrap();
        run(&s, &mut sess, "NOTIFY chan").unwrap();
        assert!(queue.lock().unwrap().is_empty());
    }

    #[test]
    fn nested_begin_and_stray_commit_emit_warnings() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "BEGIN").unwrap();
        let stmt = Parser::parse_sql("BEGIN").unwrap().remove(0);
        run_statement(&s, &mut sess, &stmt).unwrap();
        assert!(sess.notices.iter().any(|n| n.code == "25001"));

        sess.notices.clear();
        run(&s, &mut sess, "COMMIT").unwrap();
        let stmt = Parser::parse_sql("COMMIT").unwrap().remove(0);
        run_statement(&s, &mut sess, &stmt).unwrap();
        assert!(sess.notices.iter().any(|n| n.code == "25P01"));
    }

    #[test]
    fn cancel_flag_is_consumed_and_reports_error() {
        let sess = new_session();
        sess.cancel.store(true, Ordering::SeqCst);
        assert!(check_canceled(&sess).is_err());
        // The flag is cleared so a later statement is not spuriously canceled.
        assert!(!sess.cancel.load(Ordering::SeqCst));
        assert!(check_canceled(&sess).is_ok());
    }

    fn window_contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }
}
