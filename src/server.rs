//! TCP server and per-connection session handling.
//!
//! Uses a thread per connection (mirroring PostgreSQL's process-per-backend
//! model) sharing one [`Database`] behind a mutex. This keeps the first
//! iteration dependency-free; a later iteration can move to async I/O and
//! finer-grained locking.

use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::bind;
use crate::executor::{self, ExecResult, FieldDescription};
use crate::protocol::{FrontendMessage, MessageBuilder, Startup, read_message, read_startup};
use crate::sql::Parser;
use crate::sql::ast::Statement;
use crate::sql::serialize;
use crate::storage::Database;
use crate::types::{DataType, Value};
use crate::wal::Wal;

/// State shared across all connections: the database and (optional) WAL.
struct Shared {
    db: Mutex<Database>,
    /// `None` when running purely in memory (no `PGRS_DATA` configured).
    wal: Mutex<Option<Wal>>,
}

static NEXT_BACKEND_PID: AtomicI32 = AtomicI32::new(1);

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

    let shared = Arc::new(Shared { db: Mutex::new(db), wal: Mutex::new(wal) });
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
    /// Set once a statement in the block errors; further statements are
    /// rejected until COMMIT/ROLLBACK (matching PostgreSQL).
    failed: bool,
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
}

impl Session {
    fn new() -> Self {
        Session {
            tx_status: b'I',
            tx: None,
            prepared: HashMap::new(),
            portals: HashMap::new(),
            skip_until_sync: false,
        }
    }
}

fn handle_connection(stream: TcpStream, shared: Arc<Shared>) -> io::Result<()> {
    stream.set_nodelay(true).ok();
    let read_half = stream.try_clone()?;
    let mut reader = BufReader::new(read_half);
    let mut writer = BufWriter::new(stream);

    // --- startup / negotiation ---
    let _params = loop {
        match read_startup(&mut reader)? {
            Startup::SslRequest | Startup::GssEncRequest => {
                // We don't implement TLS yet: decline, client retries in clear.
                writer.write_all(b"N")?;
                writer.flush()?;
            }
            Startup::CancelRequest { .. } => {
                // No running queries to cancel in this simple model.
                return Ok(());
            }
            Startup::Params(p) => break p,
        }
    };

    // --- authentication (trust) ---
    let pid = NEXT_BACKEND_PID.fetch_add(1, Ordering::Relaxed);
    let secret = weak_secret();
    send_authentication_ok(&mut writer)?;
    send_initial_parameters(&mut writer)?;
    send_backend_key_data(&mut writer, pid, secret)?;
    send_ready_for_query(&mut writer, b'I')?;
    writer.flush()?;

    // --- main message loop ---
    let mut session = Session::new();
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
                handle_simple_query(&mut writer, &shared, &mut session, &sql)?;
            }
            FrontendMessage::Parse { name, query, param_types } => {
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
                send_ready_for_query(&mut writer, session.tx_status)?;
            }
            FrontendMessage::Flush => {}
            FrontendMessage::Password(_) => {
                // Trust auth never asks for one; ignore stray password messages.
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

// --- simple query protocol ---------------------------------------------------

fn handle_simple_query<W: Write>(
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
        return send_ready_for_query(w, session.tx_status);
    }

    for stmt in statements {
        if matches!(stmt, Statement::Empty) {
            continue;
        }
        match run_statement(shared, session, &stmt) {
            Ok(res) => send_result(w, res, &[])?,
            Err(e) => {
                send_error(w, sqlstate_for(&e), &e)?;
                mark_error(session);
                // Abort the remainder of the simple-query batch.
                return send_ready_for_query(w, session.tx_status);
            }
        }
    }

    send_ready_for_query(w, session.tx_status)
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
            session.prepared.insert(name, Prepared { statements, param_oids });
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
        send_error(w, "26000", &format!("prepared statement \"{statement}\" does not exist"))?;
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

    session.portals.insert(portal, Portal { statements, result_formats });
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
            send_error(w, "26000", &format!("prepared statement \"{name}\" does not exist"))?;
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
        send_error(w, "34000", &format!("portal \"{portal_name}\" does not exist"))?;
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
        let result = run_statement(shared, session, stmt);
        match result {
            Ok(res) => send_execute_result(w, res, &formats)?,
            Err(e) => {
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
                session.tx = Some(Transaction { db: snapshot, buffered: Vec::new(), failed: false });
                session.tx_status = b'T';
            }
            // A nested BEGIN is a no-op warning in PostgreSQL; we stay in-tx.
            Ok(ExecResult::Command("BEGIN".into()))
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
            None => Ok(ExecResult::Command("COMMIT".into())),
        },
        Statement::Rollback => {
            session.tx = None;
            session.tx_status = b'I';
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
    matches!(
        stmt,
        Statement::CreateTable(_)
            | Statement::DropTable(_)
            | Statement::CreateIndex(_)
            | Statement::DropIndex(_)
            | Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_)
    )
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

fn send_initial_parameters<W: Write>(w: &mut W) -> io::Result<()> {
    let params = [
        ("server_version", "16.0 (postgres-rs 0.1.0)"),
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
        Shared { db: Mutex::new(Database::new()), wal: Mutex::new(None) }
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
        shared.db.lock().unwrap().table("t").map(|t| t.rows.len()).unwrap_or(0)
    }

    #[test]
    fn rollback_undoes_changes() {
        let s = shared();
        let mut sess = Session::new();
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
        let mut sess = Session::new();
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
    fn error_aborts_transaction() {
        let s = shared();
        let mut sess = Session::new();
        run(&s, &mut sess, "CREATE TABLE t (id integer)").unwrap();
        run(&s, &mut sess, "BEGIN").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (1)").unwrap();

        // A failing statement aborts the transaction.
        let stmt = Parser::parse_sql("SELECT * FROM missing").unwrap().remove(0);
        assert!(run_statement(&s, &mut sess, &stmt).is_err());
        mark_error(&mut sess);
        assert_eq!(sess.tx_status, b'E');

        // Subsequent statements are rejected until the block ends.
        let stmt = Parser::parse_sql("INSERT INTO t VALUES (2)").unwrap().remove(0);
        assert!(run_statement(&s, &mut sess, &stmt).is_err());

        // COMMIT of an aborted transaction discards all of its work.
        run(&s, &mut sess, "COMMIT").unwrap();
        assert_eq!(committed_rows(&s), 0);
        assert_eq!(sess.tx_status, b'I');
    }
}
