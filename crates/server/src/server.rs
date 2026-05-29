//! TCP server and per-connection session handling.
//!
//! Uses a thread per connection (mirroring PostgreSQL's process-per-backend
//! model) sharing one [`Database`] behind a mutex. This keeps the first
//! iteration dependency-free; a later iteration can move to async I/O and
//! finer-grained locking.

use std::collections::{HashMap, HashSet};
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::auth::ScramServer;
use crate::bind;
use crate::executor::{self, ExecResult, FieldDescription};
use crate::hba::{HbaConfig, HbaMethod};
use crate::lock::{LockManager, LockMode, LockObject, TryAcquire};
use crate::protocol::{FrontendMessage, MessageBuilder, Startup, read_message, read_startup};
use crate::sql::Parser;
use crate::sql::ast::{
    Copy as CopyStmt, CopyDirection, CopyFormat, CopyTarget, Expr, Insert, IsolationLevel,
    RowLockingMode, RowLockingWaitPolicy, Select, Statement,
};
use crate::sql::serialize;
use crate::storage::Database;
use crate::types::{DataType, Value};
use crate::wal::Wal;

/// State shared across all connections: the database and (optional) WAL.
struct Shared {
    db: Mutex<Database>,
    /// `None` when running purely in memory (no `PGRS_DATA` configured).
    wal: Mutex<Option<Wal>>,
    /// Page-based on-disk store, present only when `PGRS_DISK` is set. When
    /// present, `CHECKPOINT` flushes the database to it; otherwise CHECKPOINT
    /// is a no-op (the default in-memory behaviour).
    disk: Mutex<Option<crate::disk::DiskStore>>,
    /// Live backends keyed by their advertised pid, used to route
    /// cancellation requests and asynchronous notifications.
    backends: Mutex<HashMap<i32, BackendHandle>>,
    /// `LISTEN` registrations: channel name → set of listening backend pids.
    listeners: Mutex<HashMap<String, Vec<i32>>>,
    /// Central lock manager coordinating table/row locks across connections.
    /// Paired with `lock_cv`, which blocked waiters park on; the manager mutex
    /// is never held across a wait.
    locks: Mutex<LockManager>,
    /// Condvar notified whenever locks are released, waking parked waiters.
    lock_cv: Condvar,
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
    let listener = TcpListener::bind(addr)?;
    println!("postgres-rs listening on {addr}");
    serve_on(listener, data_dir)
}

/// Serve connections on an already-bound `listener` until it stops yielding.
///
/// This is the startable entry point used both by [`run`] (which binds the
/// listener for you) and by integration tests, which bind an ephemeral
/// `127.0.0.1:0` listener and pass it in so they can discover the port and run
/// the server in a background thread without racing on a fixed address.
pub fn serve_on(listener: TcpListener, data_dir: Option<String>) -> io::Result<()> {
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

    // Opt-in page-based disk persistence (separate from the logical WAL above).
    // When `PGRS_DISK` names a directory, open the store and recover any
    // previously checkpointed tables into the database before serving.
    let disk = match std::env::var("PGRS_DISK").ok().filter(|d| !d.is_empty()) {
        Some(dir) => {
            let store = crate::disk::DiskStore::open(&dir)?;
            match store.recover() {
                Ok(recovered) => {
                    let n = recovered.table_names().len();
                    if n > 0 {
                        db = recovered;
                        println!("recovered {n} table(s) from disk store in {dir}");
                    }
                }
                Err(e) => eprintln!("warning: disk recovery failed: {e}"),
            }
            Some(store)
        }
        None => None,
    };

    let shared = Arc::new(Shared {
        db: Mutex::new(db),
        wal: Mutex::new(wal),
        disk: Mutex::new(disk),
        backends: Mutex::new(HashMap::new()),
        listeners: Mutex::new(HashMap::new()),
        locks: Mutex::new(LockManager::new()),
        lock_cv: Condvar::new(),
    });

    spawn_autovacuum_worker(&shared);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let shared = Arc::clone(&shared);
                thread::spawn(move || {
                    let peer = stream.peer_addr().ok();
                    let peer_ip = peer.map(|p| p.ip().to_string()).unwrap_or_default();
                    if let Err(e) = handle_connection(stream, shared, peer_ip) {
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

/// Spawn the autovacuum background worker, gated on `PGRS_AUTOVACUUM=on` (off by
/// default so behaviour/tests are unchanged). Every `PGRS_AUTOVACUUM_INTERVAL`
/// seconds (default 60) it locks the shared database and runs
/// [`Database::autovacuum_once`], which vacuums tables whose dead-tuple count
/// exceeds the threshold (see `should_autovacuum`). The deterministic decision
/// logic lives in storage.rs and is unit-tested directly; this thread is just
/// the periodic driver.
fn spawn_autovacuum_worker(shared: &Arc<Shared>) {
    let on = std::env::var("PGRS_AUTOVACUUM")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "on" | "1" | "true" | "yes"
            )
        })
        .unwrap_or(false);
    if !on {
        return;
    }
    let interval_secs = std::env::var("PGRS_AUTOVACUUM_INTERVAL")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(60);
    let shared = Arc::clone(shared);
    thread::spawn(move || {
        loop {
            thread::sleep(std::time::Duration::from_secs(interval_secs));
            if let Ok(mut db) = shared.db.lock() {
                let vacuumed = db.autovacuum_once();
                if !vacuumed.is_empty() {
                    eprintln!(
                        "autovacuum: vacuumed {} table(s): {}",
                        vacuumed.len(),
                        vacuumed.join(", ")
                    );
                }
            }
        }
    });
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
    /// The transaction's isolation level (resolved at BEGIN from the explicit
    /// clause, else the session default).
    isolation: IsolationLevel,
    /// READ ONLY mode (rejects mutations). Defaults to the session default.
    read_only: bool,
    /// The shared `commit_version` observed at BEGIN: the read snapshot used for
    /// optimistic write-write conflict detection.
    snapshot_version: u64,
    /// Names of tables this transaction has mutated (its write set), used to
    /// detect conflicting concurrent commits under REPEATABLE READ /
    /// SERIALIZABLE.
    write_set: HashSet<String>,
}

struct Savepoint {
    name: String,
    db: Database,
    buffered_len: usize,
    write_set: HashSet<String>,
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
    /// Global transaction ids prepared via `PREPARE TRANSACTION 'gid'` and not
    /// yet finished by `COMMIT PREPARED`/`ROLLBACK PREPARED`. The work itself is
    /// already committed at PREPARE time (simplest correct 2PC); these names
    /// just let the finishing commands validate the gid.
    prepared_gids: HashSet<String>,
    /// Wall-clock time the connection last finished processing a message, used
    /// to enforce `idle_in_transaction_session_timeout` at message boundaries.
    last_activity: Instant,
    /// Default isolation level for new transactions, settable via `SET SESSION
    /// CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL ...`. PostgreSQL's default
    /// is READ COMMITTED.
    default_isolation: IsolationLevel,
    /// Default read-only mode for new transactions.
    default_read_only: bool,
}

impl Session {
    fn new(
        pid: i32,
        cancel: Arc<AtomicBool>,
        notifications: Arc<Mutex<Vec<Notification>>>,
    ) -> Self {
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
            prepared_gids: HashSet::new(),
            last_activity: Instant::now(),
            default_isolation: IsolationLevel::ReadCommitted,
            default_read_only: false,
        }
    }

    /// Abort the in-progress transaction (used when the idle-in-transaction
    /// timeout fires). Discards the working copy and resets to idle.
    fn abort_idle_transaction(&mut self) {
        self.tx = None;
        self.tx_status = b'I';
    }
}

fn handle_connection(stream: TcpStream, shared: Arc<Shared>, peer_ip: String) -> io::Result<()> {
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
    // The `database` startup parameter defaults to the username if omitted.
    let database = params
        .iter()
        .find(|(k, _)| k == "database")
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| username.clone());

    match authenticate(&mut reader, &mut writer, &username, &database, &peer_ip)? {
        AuthOutcome::Ok => {}
        AuthOutcome::Failed => {
            send_error(&mut writer, "28P01", "password authentication failed")?;
            writer.flush()?;
            return Ok(());
        }
        AuthOutcome::Rejected => {
            send_error(
                &mut writer,
                "28000",
                &format!(
                    "pg_hba.conf rejects connection for host \"{peer_ip}\", user \"{username}\", database \"{database}\""
                ),
            )?;
            writer.flush()?;
            return Ok(());
        }
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

        // Idle-in-transaction timeout. The synchronous model can only check this
        // at message boundaries (we regain control when the next message
        // arrives, not while genuinely blocked on the socket): if a transaction
        // has been open and idle longer than the configured timeout, abort it
        // and report the error before processing the new message. See report:
        // this is a boundary-granularity approximation, not a mid-idle interrupt.
        if session.tx.is_some() && !matches!(msg, FrontendMessage::Terminate) {
            let idle_ms = current_guc_ms(&shared, &session, "idle_in_transaction_session_timeout");
            if idle_ms > 0 && session.last_activity.elapsed() >= Duration::from_millis(idle_ms) {
                session.abort_idle_transaction();
                release_locks(&shared, session.pid);
                send_error(
                    &mut writer,
                    "25P03",
                    "terminating connection due to idle-in-transaction timeout",
                )?;
                send_ready_for_query(&mut writer, session.tx_status)?;
                writer.flush()?;
                session.last_activity = Instant::now();
                continue;
            }
        }

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
        // Mark the boundary so the idle-in-transaction timeout measures the gap
        // until the next message.
        session.last_activity = Instant::now();
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
        // Release any locks this backend still holds so peers waiting on them
        // can proceed even if the connection died mid-transaction.
        release_locks(self.shared, self.pid);
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
    if std::env::var("PGRS_TRACE_QUERY").is_ok() {
        eprintln!("QUERY: {sql}");
    }
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
        // `COPY ... FROM/TO '<file>'` runs entirely server-side and goes through
        // the ordinary statement path below.
        if let Statement::Copy(copy) = &stmt {
            if matches!(copy.target, CopyTarget::Stdin | CopyTarget::Stdout) {
                if let Err(e) = run_copy(reader, w, shared, session, copy)? {
                    flush_notices(w, session)?;
                    send_error(w, sqlstate_for(&e), &e)?;
                    mark_error(session);
                    return ready_for_query(w, session);
                }
                continue;
            }
        }
        match run_statement_timed(shared, session, &stmt) {
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
    let delimiter = copy_delimiter(copy);
    let null_marker = copy_null_marker(copy);

    match copy.direction {
        CopyDirection::To => {
            copy_to_stdout(w, shared, session, copy, &columns, delimiter, &null_marker)
        }
        CopyDirection::From => copy_from_stdin(
            reader,
            w,
            shared,
            session,
            copy,
            &columns,
            delimiter,
            &null_marker,
        ),
    }
}

/// The effective field delimiter for a COPY (binary ignores it).
fn copy_delimiter(copy: &CopyStmt) -> char {
    copy.delimiter.unwrap_or(match copy.format {
        CopyFormat::Csv => ',',
        CopyFormat::Text | CopyFormat::Binary => '\t',
    })
}

/// The effective NULL marker for a COPY (binary ignores it).
fn copy_null_marker(copy: &CopyStmt) -> String {
    copy.null.clone().unwrap_or_else(|| match copy.format {
        CopyFormat::Csv => String::new(),
        CopyFormat::Text | CopyFormat::Binary => "\\N".to_string(),
    })
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
    // A query-source COPY (`COPY (SELECT ...) TO ...`) has no named table; the
    // output columns come from the query result instead.
    if table.is_empty() {
        return Ok(Vec::new());
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
    let (fields, rows) = match copy_to_rows(shared, session, copy, columns) {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let binary = copy.format == CopyFormat::Binary;

    // CopyOutResponse: overall format (0 text, 1 binary), one code per column.
    let mut hdr = MessageBuilder::new(b'H');
    hdr.put_u8(if binary { 1 } else { 0 });
    hdr.put_i16(fields.len() as i16);
    for _ in &fields {
        hdr.put_i16(if binary { 1 } else { 0 });
    }
    w.write_all(&hdr.finish())?;

    let payload = encode_copy_payload(&fields, &rows, copy, delimiter, null_marker);
    // Stream the formatted bytes as a single CopyData message.
    let mut d = MessageBuilder::new(b'd');
    d.put_bytes(&payload);
    w.write_all(&d.finish())?;

    send_simple(w, b'c')?; // CopyDone
    send_command_complete(w, &format!("COPY {}", rows.len()))?;
    Ok(Ok(()))
}

/// Run the source query of a `COPY ... TO` (a table scan or an explicit
/// `(SELECT ...)`), returning the result fields (with types, for binary
/// encoding) and rows. Reuses the SELECT path for exact transaction visibility.
fn copy_to_rows(
    shared: &Shared,
    session: &mut Session,
    copy: &CopyStmt,
    columns: &[String],
) -> Result<(Vec<FieldDescription>, Vec<Vec<Value>>), String> {
    let stmt = if let Some(query) = &copy.query {
        Statement::Select((**query).clone())
    } else {
        let col_list = columns
            .iter()
            .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("SELECT {col_list} FROM {}", copy.table);
        match Parser::parse_sql(&sql) {
            Ok(mut s) if !s.is_empty() => s.remove(0),
            Ok(_) => return Err("COPY TO produced no query".into()),
            Err(e) => return Err(e),
        }
    };
    match run_statement(shared, session, &stmt) {
        Ok(ExecResult::Rows { fields, rows, .. }) => Ok((fields, rows)),
        Ok(_) => Err("COPY TO source did not return rows".into()),
        Err(e) => Err(e),
    }
}

/// Format COPY output rows into the wire payload bytes for the chosen format.
fn encode_copy_payload(
    fields: &[FieldDescription],
    rows: &[Vec<Value>],
    copy: &CopyStmt,
    delimiter: char,
    null_marker: &str,
) -> Vec<u8> {
    if copy.format == CopyFormat::Binary {
        return encode_copy_binary(fields, rows);
    }
    let csv = copy.format == CopyFormat::Csv;
    let mut out = String::new();
    if copy.header && csv {
        let line = fields
            .iter()
            .map(|f| encode_copy_field(Some(&f.name), delimiter, null_marker, csv))
            .collect::<Vec<_>>()
            .join(&delimiter.to_string());
        out.push_str(&line);
        out.push('\n');
    }
    for row in rows {
        let line = row
            .iter()
            .map(|v| {
                let text = if v.is_null() { None } else { v.to_text() };
                encode_copy_field(text.as_deref(), delimiter, null_marker, csv)
            })
            .collect::<Vec<_>>()
            .join(&delimiter.to_string());
        out.push_str(&line);
        out.push('\n');
    }
    out.into_bytes()
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
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "COPY interrupted",
                ));
            }
            Some(_) => {
                return Ok(Err("unexpected message during COPY FROM STDIN".into()));
            }
        }
    }

    let count = match copy_from_bytes(shared, session, copy, columns, delimiter, null_marker, &buf)
    {
        Ok(n) => n,
        Err(e) => return Ok(Err(e)),
    };

    send_command_complete(w, &format!("COPY {count}"))?;
    Ok(Ok(()))
}

/// Parse a complete COPY-FROM payload (text/CSV/binary) and bulk-insert the
/// rows, returning the row count.
///
/// Bulk optimization: all rows are parsed into one tuple list and inserted via a
/// single multi-row `INSERT` statement. The executor's insert path reserves
/// capacity and appends in one loop (see `exec_insert`), so the whole batch is
/// planned once rather than re-parsing/re-planning per row, and it is WAL-logged
/// as one statement carrying the literal data (durable independent of the
/// source file).
fn copy_from_bytes(
    shared: &Shared,
    session: &mut Session,
    copy: &CopyStmt,
    columns: &[String],
    delimiter: char,
    null_marker: &str,
    buf: &[u8],
) -> Result<usize, String> {
    let rows: Vec<Vec<Expr>> = if copy.format == CopyFormat::Binary {
        let types = copy_column_types(shared, session, &copy.table, columns)?;
        decode_copy_binary(buf, &types)?
            .into_iter()
            .map(|row| row.into_iter().map(value_to_expr).collect())
            .collect()
    } else {
        let csv = copy.format == CopyFormat::Csv;
        let text = String::from_utf8_lossy(buf);
        let mut rows = Vec::new();
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
            rows.push(
                fields
                    .into_iter()
                    .map(|f| match f {
                        Some(s) => Expr::Str(s),
                        None => Expr::Null,
                    })
                    .collect(),
            );
        }
        rows
    };

    let count = rows.len();
    if count == 0 {
        return Ok(0);
    }
    let stmt = Statement::Insert(Insert {
        table: copy.table.clone(),
        columns: copy.columns.clone(),
        default_values: false,
        overriding_system_value: false,
        rows,
        select: None,
        on_conflict: None,
        returning: Vec::new(),
    });
    run_statement(shared, session, &stmt)?;
    Ok(count)
}

/// Turn a decoded [`Value`] into a literal [`Expr`] for the insert path.
fn value_to_expr(v: Value) -> Expr {
    match v {
        Value::Null => Expr::Null,
        Value::Int(i) => Expr::Int(i),
        Value::Float(f) => Expr::Float(f),
        Value::Numeric(n) => Expr::Cast {
            expr: Box::new(Expr::Str(n.to_canonical_string())),
            target: DataType::Numeric,
        },
        Value::Text(s) => Expr::Str(s),
        Value::Bool(b) => Expr::Bool(b),
    }
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

// --- COPY binary format -------------------------------------------------------

/// The 11-byte PostgreSQL COPY binary signature.
const COPY_BINARY_SIGNATURE: &[u8] = b"PGCOPY\n\xff\r\n\0";

/// Encode COPY rows into the PostgreSQL binary format. Per-field encoding
/// matches the wire DataRow binary encoding (`encode_value` with format=1).
fn encode_copy_binary(fields: &[FieldDescription], rows: &[Vec<Value>]) -> Vec<u8> {
    let mut out = Vec::with_capacity(19 + rows.len() * (2 + fields.len() * 8));
    out.extend_from_slice(COPY_BINARY_SIGNATURE);
    out.extend_from_slice(&0i32.to_be_bytes()); // flags
    out.extend_from_slice(&0i32.to_be_bytes()); // header extension length
    for row in rows {
        out.extend_from_slice(&(row.len() as i16).to_be_bytes());
        for (i, val) in row.iter().enumerate() {
            let dt = fields.get(i).map(|f| f.data_type).unwrap_or(DataType::Text);
            match encode_value(val, dt, 1) {
                Some(bytes) => {
                    out.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                    out.extend_from_slice(&bytes);
                }
                None => out.extend_from_slice(&(-1i32).to_be_bytes()), // NULL
            }
        }
    }
    out.extend_from_slice(&(-1i16).to_be_bytes()); // trailer
    out
}

/// Decode a PostgreSQL binary COPY stream into rows of [`Value`], interpreting
/// each field per the target column's [`DataType`] (the inverse of
/// [`encode_copy_binary`]; mirrors `bind::decode_binary`).
fn decode_copy_binary(buf: &[u8], types: &[DataType]) -> Result<Vec<Vec<Value>>, String> {
    let mut pos = 0usize;
    let need = |pos: usize, n: usize| -> Result<(), String> {
        if pos + n > buf.len() {
            Err("malformed binary COPY data: unexpected end of stream".into())
        } else {
            Ok(())
        }
    };
    need(pos, COPY_BINARY_SIGNATURE.len())?;
    if &buf[..COPY_BINARY_SIGNATURE.len()] != COPY_BINARY_SIGNATURE {
        return Err("invalid binary COPY signature".into());
    }
    pos += COPY_BINARY_SIGNATURE.len();
    need(pos, 8)?; // flags + header extension length
    let ext_len = i32::from_be_bytes(buf[pos + 4..pos + 8].try_into().unwrap());
    pos += 8;
    if ext_len < 0 {
        return Err("invalid binary COPY header extension length".into());
    }
    pos += ext_len as usize;

    let mut rows = Vec::new();
    loop {
        need(pos, 2)?;
        let field_count = i16::from_be_bytes(buf[pos..pos + 2].try_into().unwrap());
        pos += 2;
        if field_count == -1 {
            break; // trailer
        }
        let field_count = field_count as usize;
        let mut row = Vec::with_capacity(field_count);
        for i in 0..field_count {
            need(pos, 4)?;
            let len = i32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap());
            pos += 4;
            if len == -1 {
                row.push(Value::Null);
                continue;
            }
            let len = len as usize;
            need(pos, len)?;
            let bytes = &buf[pos..pos + len];
            pos += len;
            let dt = types.get(i).copied().unwrap_or(DataType::Text);
            row.push(decode_binary_value(bytes, dt)?);
        }
        rows.push(row);
    }
    Ok(rows)
}

/// Decode one COPY-binary field according to its column type (inverse of
/// `encode_value` in binary mode).
fn decode_binary_value(bytes: &[u8], dt: DataType) -> Result<Value, String> {
    let bad = |t: &str| format!("invalid binary length for {t}");
    Ok(match dt {
        DataType::Bool => Value::Bool(bytes.first().copied().unwrap_or(0) != 0),
        DataType::Int2 => {
            let b: [u8; 2] = bytes.try_into().map_err(|_| bad("int2"))?;
            Value::Int(i16::from_be_bytes(b) as i64)
        }
        DataType::Int4 => {
            let b: [u8; 4] = bytes.try_into().map_err(|_| bad("int4"))?;
            Value::Int(i32::from_be_bytes(b) as i64)
        }
        DataType::Int8 => {
            let b: [u8; 8] = bytes.try_into().map_err(|_| bad("int8"))?;
            Value::Int(i64::from_be_bytes(b))
        }
        DataType::Float4 => {
            let b: [u8; 4] = bytes.try_into().map_err(|_| bad("float4"))?;
            Value::Float(f32::from_be_bytes(b) as f64)
        }
        DataType::Float8 => {
            let b: [u8; 8] = bytes.try_into().map_err(|_| bad("float8"))?;
            Value::Float(f64::from_be_bytes(b))
        }
        // All other types are encoded as their UTF-8 text form.
        _ => Value::Text(String::from_utf8_lossy(bytes).into_owned()),
    })
}

/// Resolve the [`DataType`] of each COPY column (in COPY order) for binary
/// decoding. Honors the current transaction's working copy when present.
fn copy_column_types(
    shared: &Shared,
    session: &Session,
    table: &str,
    columns: &[String],
) -> Result<Vec<DataType>, String> {
    let lookup = |db: &Database| -> Result<Vec<DataType>, String> {
        let t = db
            .table(table)
            .ok_or_else(|| format!("relation \"{table}\" does not exist"))?;
        columns
            .iter()
            .map(|name| {
                t.columns
                    .iter()
                    .find(|c| &c.name == name)
                    .map(|c| c.data_type)
                    .ok_or_else(|| {
                        format!("column \"{name}\" of relation \"{table}\" does not exist")
                    })
            })
            .collect()
    };
    match &session.tx {
        Some(tx) => lookup(&tx.db),
        None => lookup(&shared.db.lock().expect("db mutex poisoned")),
    }
}

// --- COPY to/from a server-side file -----------------------------------------

/// Execute a `COPY ... FROM/TO '<path>'` entirely server-side via `std::fs`.
///
/// `COPY TO file` is read-only: it runs the source query and writes the
/// formatted bytes to the file. `COPY FROM file` reads + parses the file and
/// bulk-inserts the rows through the ordinary insert path (so constraints,
/// indexes, triggers, transaction visibility and WAL logging all apply — the
/// WAL records the resulting INSERT, not the file path, so replay is durable
/// even if the source file later changes or disappears).
fn run_copy_file(
    shared: &Shared,
    session: &mut Session,
    copy: &CopyStmt,
) -> Result<ExecResult, String> {
    let CopyTarget::File(path) = &copy.target else {
        return Err("COPY STDIN/STDOUT must use the COPY sub-protocol".into());
    };
    let columns = copy_columns(shared, session, &copy.table, copy.columns.as_ref())?;
    let delimiter = copy_delimiter(copy);
    let null_marker = copy_null_marker(copy);

    match copy.direction {
        CopyDirection::To => {
            let (fields, rows) = copy_to_rows(shared, session, copy, &columns)?;
            let payload = encode_copy_payload(&fields, &rows, copy, delimiter, &null_marker);
            std::fs::write(path, &payload)
                .map_err(|e| format!("could not write COPY destination \"{path}\": {e}"))?;
            Ok(ExecResult::Command(format!("COPY {}", rows.len())))
        }
        CopyDirection::From => {
            let buf = std::fs::read(path)
                .map_err(|e| format!("could not read COPY source \"{path}\": {e}"))?;
            let count = copy_from_bytes(
                shared,
                session,
                copy,
                &columns,
                delimiter,
                &null_marker,
                &buf,
            )?;
            Ok(ExecResult::Command(format!("COPY {count}")))
        }
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
        let result = run_statement_timed(shared, session, stmt);
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

/// Read an integer-valued GUC (e.g. `statement_timeout`) from the session's
/// effective database (the transaction working copy when in a transaction, else
/// the shared database). Returns `0` (disabled) when unset or unparsable.
fn current_guc_ms(shared: &Shared, session: &Session, name: &str) -> u64 {
    let value = match &session.tx {
        Some(tx) => tx.db.guc(name),
        None => shared.db.lock().expect("db mutex poisoned").guc(name),
    };
    value
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|ms| *ms > 0)
        .map(|ms| ms as u64)
        .unwrap_or(0)
}

/// Run a statement, enforcing `statement_timeout` (in ms) if set.
///
/// Because this engine executes statements synchronously while holding the data
/// lock, there is no way to interrupt arbitrary work mid-execution. We
/// approximate the timeout with a watchdog thread that, after the deadline, sets
/// the connection's cancel flag; the executor's cooperative `check_canceled`
/// points then abort the work. Statements that never poll cancellation (most of
/// ours run to completion quickly) are therefore checked at completion: if the
/// watchdog fired, the result is discarded and a `57014` timeout error is
/// returned. The watchdog is always torn down before this function returns, so
/// it can never affect a later statement.
fn run_statement_timed(
    shared: &Shared,
    session: &mut Session,
    stmt: &Statement,
) -> Result<ExecResult, String> {
    let timeout_ms = current_guc_ms(shared, session, "statement_timeout");
    if timeout_ms == 0 {
        return run_statement(shared, session, stmt);
    }

    // `done` lets us stop the watchdog promptly once the statement finishes.
    let done = Arc::new(AtomicBool::new(false));
    let fired = Arc::new(AtomicBool::new(false));
    let cancel = Arc::clone(&session.cancel);
    let watch_done = Arc::clone(&done);
    let watch_fired = Arc::clone(&fired);
    let watchdog = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        // Poll the done flag so we wake up soon after the statement finishes
        // instead of always sleeping the full timeout.
        while Instant::now() < deadline {
            if watch_done.load(Ordering::SeqCst) {
                return;
            }
            thread::sleep(Duration::from_millis(5).min(deadline - Instant::now()));
        }
        if !watch_done.load(Ordering::SeqCst) {
            watch_fired.store(true, Ordering::SeqCst);
            cancel.store(true, Ordering::SeqCst);
        }
    });

    let result = run_statement(shared, session, stmt);

    // Tear down the watchdog and clear any cancel flag it may have set so a
    // later statement is not spuriously canceled.
    done.store(true, Ordering::SeqCst);
    let _ = watchdog.join();
    if fired.load(Ordering::SeqCst) {
        session.cancel.store(false, Ordering::SeqCst);
        return Err("canceling statement due to statement timeout".to_string());
    }
    result
}

// --- lock manager integration -------------------------------------------------

/// Acquire `mode` on `obj` for this backend, blocking on the lock condvar until
/// the lock is free.
///
/// Returns `Ok(())` once granted. On a NOWAIT request that conflicts, returns
/// `55P03`. If granting the wait would deadlock, the requesting transaction is
/// aborted with `40P01` (its locks are released by the caller's COMMIT/ROLLBACK
/// path; here we just release this backend's locks so it cannot keep blocking
/// others). The cancel flag is honored so a canceled statement does not block
/// forever.
///
/// The lock-manager mutex is dropped while parked on the condvar, and the DB
/// mutex must NOT be held by the caller across this call.
fn acquire_lock(
    shared: &Shared,
    session: &Session,
    obj: &LockObject,
    mode: LockMode,
    nowait: bool,
) -> Result<(), String> {
    let mut guard = shared.locks.lock().expect("locks mutex poisoned");
    loop {
        match guard.try_acquire(session.pid, obj, mode) {
            TryAcquire::Granted => return Ok(()),
            TryAcquire::Deadlock => {
                // Release everything this backend holds/waits on so it stops
                // blocking peers, then surface the deadlock error.
                if guard.release_all(session.pid) {
                    shared.lock_cv.notify_all();
                }
                return Err("deadlock detected".to_string());
            }
            TryAcquire::Conflict(_) => {
                if nowait {
                    // Drop our tentative waiter entry before erroring.
                    guard.release_all(session.pid);
                    return Err("could not obtain lock on relation".to_string());
                }
                // Honor cancellation rather than blocking forever.
                if session.cancel.load(Ordering::SeqCst) {
                    guard.release_all(session.pid);
                    return Err("canceling statement due to user request".to_string());
                }
                // Park until woken by a release; re-check periodically so a
                // cancellation set while we sleep is still observed.
                let (g, _timeout) = shared
                    .lock_cv
                    .wait_timeout(guard, Duration::from_millis(50))
                    .expect("lock condvar poisoned");
                guard = g;
            }
        }
    }
}

/// Release every lock held or waited on by this backend and wake parked
/// waiters. Called at COMMIT/ROLLBACK and on disconnect.
fn release_locks(shared: &Shared, pid: i32) {
    let mut guard = shared.locks.lock().expect("locks mutex poisoned");
    if guard.release_all(pid) {
        drop(guard);
        shared.lock_cv.notify_all();
    }
}

/// Map a `LOCK TABLE` mode spelling to a [`LockMode`], defaulting to
/// ACCESS EXCLUSIVE (PostgreSQL's default).
fn table_lock_mode(spec: &Option<String>) -> Result<LockMode, String> {
    match spec {
        None => Ok(LockMode::AccessExclusive),
        Some(s) => LockMode::parse(s).ok_or_else(|| format!("unrecognized lock mode: {s}")),
    }
}

/// A stable opaque key for a row, derived from its full projected value tuple.
/// Coarser than PostgreSQL's physical-tuple locks (see `lock.rs` docs).
fn row_key(row: &[Value]) -> String {
    let mut key = String::new();
    for (i, v) in row.iter().enumerate() {
        if i > 0 {
            key.push('\u{1}');
        }
        match v.to_text() {
            Some(t) => key.push_str(&t),
            None => key.push_str("\u{0}NULL"),
        }
    }
    key
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
        Statement::Begin {
            isolation,
            read_only,
        } => {
            if session.tx.is_none() {
                // Snapshot the shared db AND its commit version atomically under
                // the same lock so the recorded snapshot version always matches
                // the cloned state (no commit can slip in between).
                let (snapshot, snapshot_version) = {
                    let db = shared.db.lock().expect("db mutex poisoned");
                    (db.clone(), db.commit_version())
                };
                let level = isolation.unwrap_or(session.default_isolation);
                let ro = read_only.unwrap_or(session.default_read_only);
                // Reflect the resolved level in the working copy's GUC so
                // `SHOW transaction_isolation` inside the tx is accurate.
                let mut snapshot = snapshot;
                snapshot
                    .set_system_setting("transaction_isolation".into(), level.guc_value().into());
                snapshot.set_system_setting(
                    "transaction_read_only".into(),
                    if ro { "on".into() } else { "off".into() },
                );
                session.tx = Some(Transaction {
                    db: snapshot,
                    buffered: Vec::new(),
                    savepoints: Vec::new(),
                    failed: false,
                    isolation: level,
                    read_only: ro,
                    snapshot_version,
                    write_set: HashSet::new(),
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
        Statement::SetTransaction {
            isolation,
            read_only,
            session: is_session,
        } => {
            if *is_session {
                // Change the session default for subsequent transactions.
                if let Some(level) = isolation {
                    session.default_isolation = *level;
                }
                if let Some(ro) = read_only {
                    session.default_read_only = *ro;
                }
            } else if let Some(tx) = session.tx.as_mut() {
                // `SET TRANSACTION` adjusts the current transaction.
                if let Some(level) = isolation {
                    tx.isolation = *level;
                    tx.db.set_system_setting(
                        "transaction_isolation".into(),
                        level.guc_value().into(),
                    );
                }
                if let Some(ro) = read_only {
                    tx.read_only = *ro;
                    tx.db.set_system_setting(
                        "transaction_read_only".into(),
                        if *ro { "on".into() } else { "off".into() },
                    );
                }
            } else {
                // `SET TRANSACTION` outside a transaction block is an error in
                // PostgreSQL, but be lenient: treat as setting the next tx.
                if let Some(level) = isolation {
                    session.default_isolation = *level;
                }
                if let Some(ro) = read_only {
                    session.default_read_only = *ro;
                }
            }
            Ok(ExecResult::Command("SET".into()))
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
                release_locks(shared, session.pid);
                session.tx_status = b'I';
                Ok(ExecResult::Command("ROLLBACK".into()))
            }
            Some(tx) => {
                let mut shared_db = shared.db.lock().expect("db mutex poisoned");
                // Optimistic write-write conflict detection. Under REPEATABLE
                // READ / SERIALIZABLE, abort if any table this transaction wrote
                // was modified by another committed transaction after our
                // snapshot. Under READ (UN)COMMITTED we keep last-write-wins.
                let serializing = matches!(
                    tx.isolation,
                    IsolationLevel::RepeatableRead | IsolationLevel::Serializable
                );
                if serializing
                    && shared_db.has_conflicting_commit(&tx.write_set, tx.snapshot_version)
                {
                    drop(shared_db);
                    release_locks(shared, session.pid);
                    session.tx_status = b'I';
                    return Err("could not serialize access due to concurrent update".to_string());
                }
                // Carry forward the new commit version + table stamps into the
                // working copy before it becomes the shared database (clone
                // dropped that bookkeeping field's prior shared state otherwise).
                let mut working = tx.db;
                working.adopt_commit_state(&shared_db);
                working.record_commit(&tx.write_set);
                *shared_db = working;
                drop(shared_db);
                if let Some(wal) = shared.wal.lock().expect("wal mutex poisoned").as_mut() {
                    for sql in &tx.buffered {
                        if let Err(e) = wal.append(sql) {
                            eprintln!("warning: WAL append failed: {e}");
                        }
                    }
                }
                release_locks(shared, session.pid);
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
            release_locks(shared, session.pid);
            session.tx_status = b'I';
            Ok(ExecResult::Command("ROLLBACK".into()))
        }
        Statement::PrepareTransaction { gid } => match session.tx.take() {
            Some(tx) if tx.failed => {
                release_locks(shared, session.pid);
                session.tx_status = b'I';
                Err("current transaction is aborted, commands ignored until end of transaction block".into())
            }
            Some(tx) => {
                // Simplest correct 2PC: commit the buffered work now (publish +
                // WAL), and remember the gid so COMMIT/ROLLBACK PREPARED accept it.
                let mut shared_db = shared.db.lock().expect("db mutex poisoned");
                let serializing = matches!(
                    tx.isolation,
                    IsolationLevel::RepeatableRead | IsolationLevel::Serializable
                );
                if serializing
                    && shared_db.has_conflicting_commit(&tx.write_set, tx.snapshot_version)
                {
                    drop(shared_db);
                    release_locks(shared, session.pid);
                    session.tx_status = b'I';
                    return Err("could not serialize access due to concurrent update".to_string());
                }
                let mut working = tx.db;
                working.adopt_commit_state(&shared_db);
                working.record_commit(&tx.write_set);
                *shared_db = working;
                drop(shared_db);
                if let Some(wal) = shared.wal.lock().expect("wal mutex poisoned").as_mut() {
                    for sql in &tx.buffered {
                        if let Err(e) = wal.append(sql) {
                            eprintln!("warning: WAL append failed: {e}");
                        }
                    }
                }
                session.prepared_gids.insert(gid.clone());
                release_locks(shared, session.pid);
                session.tx_status = b'I';
                Ok(ExecResult::Command("PREPARE TRANSACTION".into()))
            }
            None => Err("PREPARE TRANSACTION can only be used in transaction blocks".into()),
        },
        Statement::CommitPrepared { gid } => {
            if session.prepared_gids.remove(gid) {
                Ok(ExecResult::Command("COMMIT PREPARED".into()))
            } else {
                Err(format!(
                    "prepared transaction with identifier \"{gid}\" does not exist"
                ))
            }
        }
        Statement::RollbackPrepared { gid } => {
            if session.prepared_gids.remove(gid) {
                Ok(ExecResult::Command("ROLLBACK PREPARED".into()))
            } else {
                Err(format!(
                    "prepared transaction with identifier \"{gid}\" does not exist"
                ))
            }
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
                write_set: tx.write_set.clone(),
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
            tx.write_set = sp.write_set.clone();
            tx.savepoints.truncate(pos + 1);
            tx.failed = false;
            session.tx_status = b'T';
            Ok(ExecResult::Command("ROLLBACK".into()))
        }
        // Real table locking. LOCK TABLE requires a transaction block (the lock
        // is held until COMMIT/ROLLBACK), matching PostgreSQL.
        Statement::LockTable(lock) => {
            if session.tx.is_none() {
                return Err("LOCK TABLE can only be used in transaction blocks".into());
            }
            if session.tx.as_ref().is_some_and(|t| t.failed) {
                return Err(
                    "current transaction is aborted, commands ignored until end of transaction block".into(),
                );
            }
            // Validate the relations exist against the working copy.
            {
                let tx = session.tx.as_ref().unwrap();
                for table in &lock.tables {
                    if !tx.db.contains_table(table) {
                        return Err(format!("relation \"{table}\" does not exist"));
                    }
                }
            }
            let mode = table_lock_mode(&lock.mode)?;
            for table in &lock.tables {
                let obj = LockObject::Table(table.clone());
                if let Err(e) = acquire_lock(shared, session, &obj, mode, lock.nowait) {
                    // A lock failure aborts the transaction.
                    if let Some(tx) = session.tx.as_mut() {
                        tx.failed = true;
                    }
                    session.tx_status = b'E';
                    return Err(e);
                }
            }
            Ok(ExecResult::Command("LOCK TABLE".into()))
        }
        // `SELECT ... FOR UPDATE/SHARE` acquires row locks on the selected rows
        // (held until end of transaction). Plain SELECTs fall through.
        Statement::Select(select) if !select.locking.is_empty() => {
            run_select_for_locking(shared, session, stmt, select)
        }
        // Server-side file COPY (`FROM/TO '<path>'`). STDIN/STDOUT forms are
        // handled by the COPY sub-protocol before reaching here.
        Statement::Copy(copy) => run_copy_file(shared, session, copy),
        _ if session.tx.is_some() => {
            // Run against the transaction's working copy.
            let res = {
                let tx = session.tx.as_mut().unwrap();
                if tx.failed {
                    return Err(
                        "current transaction is aborted, commands ignored until end of transaction block".into(),
                    );
                }
                // A READ ONLY transaction rejects data-changing statements.
                if tx.read_only && is_data_mutation(stmt) {
                    tx.failed = true;
                    session.tx_status = b'E';
                    return Err("cannot execute statement in a read-only transaction".into());
                }
                let res = executor::execute(&mut tx.db, stmt.clone());
                match &res {
                    Ok(_) if is_mutation(stmt) => {
                        tx.buffered.push(serialize::statement_to_sql(stmt));
                        // Record the mutated table(s) into the write set for
                        // optimistic conflict detection at COMMIT.
                        for table in mutated_tables(stmt) {
                            tx.write_set.insert(table);
                        }
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

/// Execute a `SELECT ... FOR UPDATE/SHARE`, acquiring row locks on the selected
/// rows.
///
/// The SELECT runs first (against the transaction working copy if any, else the
/// shared db) to determine which rows are selected. Each result row gets a lock
/// object keyed by `(target table, row fingerprint)` — a coarser, logical-tuple
/// granularity than PostgreSQL's heap-TID locks (see `lock.rs`). `FOR UPDATE`
/// requests an exclusive row lock, `FOR SHARE` a share lock.
///
/// - default wait policy: conflicting rows block until free;
/// - `NOWAIT`: error `55P03` if any selected row is already locked;
/// - `SKIP LOCKED`: silently drop already-locked rows from the result.
fn run_select_for_locking(
    shared: &Shared,
    session: &mut Session,
    stmt: &Statement,
    select: &Select,
) -> Result<ExecResult, String> {
    // Resolve mode + wait policy from the (first) locking clause. Multiple
    // clauses collapse to the strongest mode.
    let strongest = select
        .locking
        .iter()
        .map(|c| c.mode)
        .max_by_key(|m| matches!(m, RowLockingMode::Update | RowLockingMode::NoKeyUpdate))
        .unwrap();
    let mode = match strongest {
        RowLockingMode::Update | RowLockingMode::NoKeyUpdate => LockMode::Exclusive,
        RowLockingMode::Share | RowLockingMode::KeyShare => LockMode::Share,
    };
    let wait_policy = select.locking.iter().find_map(|c| c.wait_policy);

    // The lock target table: the explicit `OF <table>` list (first entry) or the
    // base table of the FROM clause.
    let target = select
        .locking
        .iter()
        .flat_map(|c| c.tables.iter())
        .next()
        .cloned()
        .or_else(|| select.from.as_ref().map(|f| f.base.name.clone()));
    let Some(target) = target else {
        // No base relation (e.g. `SELECT 1 FOR UPDATE`): nothing to lock; run as
        // an ordinary statement.
        return run_plain(shared, session, stmt);
    };

    // Run the SELECT to obtain the selected rows.
    let res = run_plain(shared, session, stmt)?;
    let ExecResult::Rows { fields, rows, tag } = res else {
        return Ok(res);
    };

    // Acquire a row lock per selected row, applying the wait policy.
    let mut kept = Vec::with_capacity(rows.len());
    for row in rows {
        let obj = LockObject::Row {
            table: target.clone(),
            key: row_key(&row),
        };
        match wait_policy {
            Some(RowLockingWaitPolicy::SkipLocked) => {
                let locked = {
                    let mut guard = shared.locks.lock().expect("locks mutex poisoned");
                    if guard.is_locked_by_other(session.pid, &obj, mode) {
                        true
                    } else {
                        // Free for us: take it (no block) and keep the row.
                        guard.try_acquire(session.pid, &obj, mode);
                        false
                    }
                };
                if !locked {
                    kept.push(row);
                }
            }
            Some(RowLockingWaitPolicy::NoWait) => {
                if let Err(e) = acquire_lock(shared, session, &obj, mode, true) {
                    if let Some(tx) = session.tx.as_mut() {
                        tx.failed = true;
                    }
                    if session.tx.is_some() {
                        session.tx_status = b'E';
                    }
                    return Err(e);
                }
                kept.push(row);
            }
            None => {
                if let Err(e) = acquire_lock(shared, session, &obj, mode, false) {
                    if let Some(tx) = session.tx.as_mut() {
                        tx.failed = true;
                    }
                    if session.tx.is_some() {
                        session.tx_status = b'E';
                    }
                    return Err(e);
                }
                kept.push(row);
            }
        }
    }

    let n = kept.len();
    Ok(ExecResult::Rows {
        fields,
        rows: kept,
        // The command tag reports the (possibly reduced, under SKIP LOCKED) count.
        tag: if tag.starts_with("SELECT") {
            format!("SELECT {n}")
        } else {
            tag
        },
    })
}

/// Run a statement through the ordinary (non-locking) path: against the
/// transaction working copy when in a transaction, else the shared database.
/// This mirrors the fall-through arms of [`run_statement`] without re-entering
/// the LOCK TABLE / FOR UPDATE interception.
fn run_plain(
    shared: &Shared,
    session: &mut Session,
    stmt: &Statement,
) -> Result<ExecResult, String> {
    if session.tx.is_some() {
        let res = {
            let tx = session.tx.as_mut().unwrap();
            if tx.failed {
                return Err(
                    "current transaction is aborted, commands ignored until end of transaction block".into(),
                );
            }
            executor::execute(&mut tx.db, stmt.clone())
        };
        if res.is_err() {
            session.tx.as_mut().unwrap().failed = true;
            session.tx_status = b'E';
        }
        res
    } else {
        execute_autocommit(shared, stmt)
    }
}

/// Execute one statement against the shared database and, if it mutates state
/// and succeeds, durably append it to the WAL before releasing the lock.
/// Holding the db lock across the append keeps WAL order == execution order.
fn execute_autocommit(shared: &Shared, stmt: &Statement) -> Result<ExecResult, String> {
    let mut db = shared.db.lock().expect("db mutex poisoned");
    let result = executor::execute(&mut db, stmt.clone());

    // CHECKPOINT: when a page-based disk store is configured (PGRS_DISK), flush
    // the current database to it. A no-op otherwise (default behaviour).
    if result.is_ok() && matches!(stmt, Statement::Checkpoint) {
        if let Some(store) = shared.disk.lock().expect("disk mutex poisoned").as_mut() {
            if let Err(e) = store.checkpoint(&db) {
                eprintln!("warning: CHECKPOINT to disk store failed: {e}");
            }
        }
    }

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
    let statements = parse_wal_statements(contents);
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

fn parse_wal_statements(contents: &str) -> Vec<Statement> {
    match Parser::parse_sql(contents) {
        Ok(statements) => statements,
        Err(e) => {
            eprintln!("warning: failed to parse complete WAL, recovering prefix: {e}");
            let mut statements = Vec::new();
            for record in wal_records(contents) {
                match Parser::parse_sql(record) {
                    Ok(mut parsed) => statements.append(&mut parsed),
                    Err(e) => {
                        eprintln!("warning: stopped WAL prefix recovery at malformed record: {e}");
                        break;
                    }
                }
            }
            statements
        }
    }
}

fn wal_records(contents: &str) -> impl Iterator<Item = &str> {
    contents
        .split_inclusive(";\n")
        .filter(|record| record.ends_with(";\n"))
}

/// Whether a statement changes table *data* (DML), used both for write-set
/// tracking and to reject writes in a READ ONLY transaction. DDL is excluded
/// (PostgreSQL also rejects DDL in read-only transactions, but our optimistic
/// conflict model only meaningfully tracks data tables).
fn is_data_mutation(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_)
            | Statement::Merge(_)
            | Statement::Truncate(_)
    )
}

/// The table name(s) a mutating statement writes, for the transaction write set.
/// DDL that targets a specific table is included so e.g. a concurrent
/// `ALTER TABLE`/`DROP TABLE` of a written table is also caught.
fn mutated_tables(stmt: &Statement) -> Vec<String> {
    match stmt {
        Statement::Insert(i) => vec![i.table.clone()],
        Statement::Update(u) => vec![u.table.clone()],
        Statement::Delete(d) => vec![d.table.clone()],
        Statement::Merge(m) => vec![m.target.clone()],
        Statement::Truncate(t) => t.tables.clone(),
        Statement::AlterTable(a) => vec![a.table.clone()],
        Statement::CreatePolicy(c) => vec![c.table.clone()],
        Statement::AlterPolicy(a) => vec![a.table.clone()],
        Statement::DropPolicy(d) => vec![d.table.clone()],
        Statement::DropTable(d) => vec![d.name.clone()],
        Statement::Explain(e) if e.analyze => mutated_tables(&e.statement),
        _ => Vec::new(),
    }
}

/// Whether a statement changes persistent state and must be logged.
fn is_mutation(stmt: &Statement) -> bool {
    match stmt {
        Statement::CreateTable(_)
        | Statement::CreateExtension(_)
        | Statement::AlterExtension(_)
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
        | Statement::CreatePolicy(_)
        | Statement::AlterPolicy(_)
        | Statement::DropPolicy(_)
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
        | Statement::CreateCatalogObject(_)
        | Statement::DropCatalogObject(_)
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
        Value::Numeric(n) => n.to_f64() as i64,
        Value::Bool(b) => *b as i64,
        Value::Text(s) => s.parse().unwrap_or(0),
        Value::Null => 0,
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        Value::Numeric(n) => n.to_f64(),
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
/// The result of authenticating a connection.
enum AuthOutcome {
    /// The client is authenticated (or trusted).
    Ok,
    /// The client failed the password challenge.
    Failed,
    /// An HBA `reject` rule (or no matching rule) refused the connection.
    Rejected,
}

fn authenticate<R: io::Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    username: &str,
    database: &str,
    peer_ip: &str,
) -> io::Result<AuthOutcome> {
    let password = std::env::var("PGRS_PASSWORD").ok().unwrap_or_default();

    // pg_hba.conf-style rules take precedence when configured. The first rule
    // matching (database, user, address) selects the method.
    if let Some(config) = load_hba_config() {
        return match config.match_method(database, username, peer_ip) {
            Some(HbaMethod::Trust) => Ok(AuthOutcome::Ok),
            Some(HbaMethod::Reject) | None => Ok(AuthOutcome::Rejected),
            Some(HbaMethod::Password) => Ok(bool_outcome(cleartext_authenticate(
                reader, writer, &password,
            )?)),
            Some(HbaMethod::Md5) => Ok(bool_outcome(md5_authenticate(
                reader, writer, &password, username,
            )?)),
            Some(HbaMethod::ScramSha256) => {
                Ok(bool_outcome(scram_authenticate(reader, writer, &password)?))
            }
        };
    }

    let method = std::env::var("PGRS_AUTH_METHOD")
        .ok()
        .filter(|m| !m.is_empty());

    let ok = match method.as_deref() {
        // Legacy default: SCRAM when a password is configured, else trust.
        None => {
            if !password.is_empty() {
                scram_authenticate(reader, writer, &password)?
            } else {
                true
            }
        }
        Some("trust") => true,
        Some("password") => cleartext_authenticate(reader, writer, &password)?,
        Some("md5") => md5_authenticate(reader, writer, &password, username)?,
        Some("scram") => scram_authenticate(reader, writer, &password)?,
        // An unrecognized value is treated as trust rather than locking out.
        Some(_) => true,
    };
    Ok(bool_outcome(ok))
}

fn bool_outcome(ok: bool) -> AuthOutcome {
    if ok {
        AuthOutcome::Ok
    } else {
        AuthOutcome::Failed
    }
}

/// Load HBA rules from `PGRS_HBA` (a file path) or `PGRS_HBA_RULES` (inline
/// rules), or `None` when neither is set (keep the legacy auth behavior).
fn load_hba_config() -> Option<HbaConfig> {
    if let Some(path) = std::env::var("PGRS_HBA").ok().filter(|p| !p.is_empty()) {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        return Some(HbaConfig::parse(&text));
    }
    if let Some(rules) = std::env::var("PGRS_HBA_RULES")
        .ok()
        .filter(|r| !r.is_empty())
    {
        // Allow `;`-separated inline rules in addition to newlines.
        let normalized = rules.replace(';', "\n");
        return Some(HbaConfig::parse(&normalized));
    }
    None
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
    } else if msg.contains("could not serialize access") {
        "40001" // serialization_failure
    } else if msg.contains("deadlock detected") {
        "40P01" // deadlock_detected
    } else if msg.contains("could not obtain lock") {
        "55P03" // lock_not_available
    } else if msg.contains("read-only transaction") {
        "25006" // read_only_sql_transaction
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
            disk: Mutex::new(None),
            backends: Mutex::new(HashMap::new()),
            listeners: Mutex::new(HashMap::new()),
            locks: Mutex::new(LockManager::new()),
            lock_cv: Condvar::new(),
        }
    }

    /// CHECKPOINT is a no-op without a disk store, and flushes to the disk store
    /// (so a fresh recovery sees the rows) when one is configured.
    #[test]
    fn checkpoint_flushes_to_disk_store_when_configured() {
        use std::sync::atomic::{AtomicU64, Ordering};
        static C: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "pgrs_disk_srv_{}_{}",
            std::process::id(),
            C.fetch_add(1, Ordering::SeqCst)
        ));

        let s = shared();
        *s.disk.lock().unwrap() = Some(crate::disk::DiskStore::open(&dir).unwrap());

        let run = |sql: &str| {
            for stmt in Parser::parse_sql(sql).unwrap() {
                execute_autocommit(&s, &stmt).unwrap();
            }
        };
        run("CREATE TABLE kv (k bigint, v text)");
        run("INSERT INTO kv VALUES (1, 'one'), (2, 'two')");
        run("CHECKPOINT");

        // A fresh recovery from the same directory sees the checkpointed rows.
        let recovered = crate::disk::DiskStore::open(&dir)
            .unwrap()
            .recover()
            .unwrap();
        let kv = recovered.table("kv").expect("kv recovered");
        assert_eq!(kv.rows.len(), 2);
        assert_eq!(kv.rows[0], vec![Value::Int(1), Value::Text("one".into())]);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn logical_wal_replay_recovers_well_formed_prefix_from_torn_tail() {
        let mut db = Database::new();
        let log = "\
            CREATE TABLE t (id integer PRIMARY KEY, v integer);\n\
            INSERT INTO t VALUES (1, 10);\n\
            INSERT INTO t VALUES (2, 20);\n\
            INSERT INTO t VALUES (3, 30";

        assert_eq!(replay(&mut db, log), 3);
        let table = db.table("t").expect("table recovered");
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[0], vec![Value::Int(1), Value::Int(10)]);
        assert_eq!(table.rows[1], vec![Value::Int(2), Value::Int(20)]);
    }

    #[test]
    fn logical_wal_replay_stops_at_first_malformed_record() {
        let mut db = Database::new();
        let log = "\
            CREATE TABLE t (id integer PRIMARY KEY);\n\
            INSERT INTO t VALUES (1);\n\
            INSERT INTO missing VALUES (2;\n\
            INSERT INTO t VALUES (3);\n";

        assert_eq!(replay(&mut db, log), 2);
        let table = db.table("t").expect("table recovered");
        assert_eq!(table.rows, vec![vec![Value::Int(1)]]);
    }

    /// A bare session with dummy cancellation/notification handles.
    fn new_session() -> Session {
        Session::new(
            1,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
        )
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
    fn statement_timeout_guc_is_read_from_db() {
        let s = shared();
        let mut sess = new_session();
        // Unset -> disabled (0).
        assert_eq!(current_guc_ms(&s, &sess, "statement_timeout"), 0);
        // Set on the shared (autocommit) database.
        run(&s, &mut sess, "SET statement_timeout = '2500'").unwrap();
        assert_eq!(current_guc_ms(&s, &sess, "statement_timeout"), 2500);
        // RESET disables it again.
        run(&s, &mut sess, "RESET statement_timeout").unwrap();
        assert_eq!(current_guc_ms(&s, &sess, "statement_timeout"), 0);
    }

    #[test]
    fn statement_timeout_read_from_transaction_copy() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "BEGIN").unwrap();
        // Set inside the transaction's working copy.
        run(&s, &mut sess, "SET statement_timeout = '1000'").unwrap();
        assert_eq!(current_guc_ms(&s, &sess, "statement_timeout"), 1000);
        run(&s, &mut sess, "ROLLBACK").unwrap();
    }

    #[test]
    fn run_statement_timed_passthrough_when_disabled() {
        // With the timeout disabled, the timed wrapper behaves like run_statement.
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer)").unwrap();
        let stmt = Parser::parse_sql("INSERT INTO t VALUES (1)")
            .unwrap()
            .remove(0);
        assert!(run_statement_timed(&s, &mut sess, &stmt).is_ok());
        // The cancel flag must remain clear (no watchdog ran).
        assert!(!sess.cancel.load(Ordering::SeqCst));
    }

    #[test]
    fn statement_timeout_fires_and_reports_57014() {
        // Deterministic firing without sleeping inside the statement itself: set
        // a 1ms timeout *inside a transaction* (so the timeout is read from the
        // transaction's working copy, taking no shared lock), then time a COMMIT
        // while another thread holds the shared db lock past the deadline. COMMIT
        // blocks on that lock, the watchdog fires, and the timed wrapper returns
        // the timeout error mapped to 57014.
        let s = Arc::new(shared());
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer)").unwrap();
        run(&s, &mut sess, "BEGIN").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (1)").unwrap();
        // Stored in the transaction's working copy, readable without a lock.
        run(&s, &mut sess, "SET statement_timeout = '1'").unwrap();
        assert_eq!(current_guc_ms(&s, &sess, "statement_timeout"), 1);

        let guard_shared = Arc::clone(&s);
        let (tx, rx) = std::sync::mpsc::channel();
        let hold = thread::spawn(move || {
            let _db = guard_shared.db.lock().expect("db mutex");
            tx.send(()).unwrap(); // signal: lock acquired
            thread::sleep(Duration::from_millis(120));
        });
        // Wait until the holder actually owns the lock before timing the COMMIT.
        rx.recv().unwrap();

        let stmt = Parser::parse_sql("COMMIT").unwrap().remove(0);
        let res = run_statement_timed(&s, &mut sess, &stmt);
        hold.join().unwrap();

        let err = match res {
            Err(e) => e,
            Ok(_) => panic!("statement should time out"),
        };
        assert_eq!(sqlstate_for(&err), "57014");
        // The watchdog's cancel flag was consumed, not left set.
        assert!(!sess.cancel.load(Ordering::SeqCst));
    }

    #[test]
    fn idle_transaction_abort_resets_session() {
        // Plumbing for idle_in_transaction_session_timeout: opening a transaction
        // then aborting it (as the loop does when the timeout elapses) returns the
        // session to idle and discards the working copy.
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer)").unwrap();
        run(&s, &mut sess, "BEGIN").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (1)").unwrap();
        assert_eq!(sess.tx_status, b'T');
        sess.abort_idle_transaction();
        assert_eq!(sess.tx_status, b'I');
        assert!(sess.tx.is_none());
        // The uncommitted row never reached the shared database.
        assert_eq!(committed_rows(&s), 0);
    }

    #[test]
    fn idle_in_transaction_timeout_guc_is_read() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "BEGIN").unwrap();
        run(
            &s,
            &mut sess,
            "SET idle_in_transaction_session_timeout = '500'",
        )
        .unwrap();
        assert_eq!(
            current_guc_ms(&s, &sess, "idle_in_transaction_session_timeout"),
            500
        );
        run(&s, &mut sess, "ROLLBACK").unwrap();
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

    /// Run a SELECT against the working copy of `sess`'s transaction (or the
    /// shared db when idle) and return the integer rows of column 0.
    fn select_ints(shared: &Shared, sess: &Session, sql: &str) -> Vec<i64> {
        let stmt = Parser::parse_sql(sql).unwrap().remove(0);
        let rows = match &sess.tx {
            Some(tx) => {
                let mut db = tx.db.clone();
                executor::execute(&mut db, stmt).unwrap()
            }
            None => {
                let mut db = shared.db.lock().unwrap();
                executor::execute(&mut db, stmt).unwrap()
            }
        };
        match rows {
            ExecResult::Rows { rows, .. } => rows
                .into_iter()
                .map(|r| match r[0] {
                    Value::Int(i) => i,
                    _ => panic!("expected int"),
                })
                .collect(),
            _ => panic!("expected rows"),
        }
    }

    fn second_session() -> Session {
        Session::new(
            2,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
        )
    }

    #[test]
    fn repeatable_read_write_write_conflict_aborts_second_commit() {
        // Two sessions both BEGIN REPEATABLE READ off the same snapshot and
        // both update the same table. The first commit wins; the second commit
        // must fail with a serialization error (no last-commit-wins for RR).
        let s = shared();
        let mut setup = new_session();
        run(&s, &mut setup, "CREATE TABLE t (id integer)").unwrap();
        run(&s, &mut setup, "INSERT INTO t VALUES (1)").unwrap();

        let mut a = new_session();
        let mut b = second_session();
        run(&s, &mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
        run(&s, &mut b, "BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
        run(&s, &mut a, "UPDATE t SET id = 10").unwrap();
        run(&s, &mut b, "UPDATE t SET id = 20").unwrap();

        run(&s, &mut a, "COMMIT").unwrap();
        let commit_b = run_statement(&s, &mut b, &Parser::parse_sql("COMMIT").unwrap().remove(0));
        let err = match commit_b {
            Err(e) => e,
            Ok(_) => panic!("second RR commit must conflict"),
        };
        assert_eq!(sqlstate_for(&err), "40001");
        // b is rolled back; a's value stands.
        assert_eq!(select_ints(&s, &b, "SELECT id FROM t"), vec![10]);
    }

    #[test]
    fn read_committed_write_write_allows_second_commit() {
        // Same scenario under READ COMMITTED: last-commit-wins is acceptable, so
        // the second commit succeeds.
        let s = shared();
        let mut setup = new_session();
        run(&s, &mut setup, "CREATE TABLE t (id integer)").unwrap();
        run(&s, &mut setup, "INSERT INTO t VALUES (1)").unwrap();

        let mut a = new_session();
        let mut b = second_session();
        run(&s, &mut a, "BEGIN ISOLATION LEVEL READ COMMITTED").unwrap();
        run(&s, &mut b, "BEGIN").unwrap(); // default is READ COMMITTED
        run(&s, &mut a, "UPDATE t SET id = 10").unwrap();
        run(&s, &mut b, "UPDATE t SET id = 20").unwrap();
        run(&s, &mut a, "COMMIT").unwrap();
        run(&s, &mut b, "COMMIT").unwrap();
        // Last commit wins.
        assert_eq!(
            select_ints(&s, &new_session(), "SELECT id FROM t"),
            vec![20]
        );
    }

    #[test]
    fn repeatable_read_reader_does_not_see_concurrent_commit() {
        // A REPEATABLE READ transaction reads a stable snapshot: a concurrent
        // committed INSERT from another session is invisible inside it.
        let s = shared();
        let mut setup = new_session();
        run(&s, &mut setup, "CREATE TABLE t (id integer)").unwrap();
        run(&s, &mut setup, "INSERT INTO t VALUES (1)").unwrap();

        let mut reader = new_session();
        run(&s, &mut reader, "BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
        // Reader sees the one pre-existing row.
        assert_eq!(
            select_ints(&s, &reader, "SELECT id FROM t ORDER BY id"),
            vec![1]
        );

        // Another session commits an insert.
        let mut writer = second_session();
        run(&s, &mut writer, "INSERT INTO t VALUES (2)").unwrap();
        // It is visible in the shared db now...
        assert_eq!(
            select_ints(&s, &new_session(), "SELECT id FROM t ORDER BY id"),
            vec![1, 2]
        );
        // ...but NOT inside the already-open repeatable-read transaction.
        assert_eq!(
            select_ints(&s, &reader, "SELECT id FROM t ORDER BY id"),
            vec![1]
        );
        run(&s, &mut reader, "ROLLBACK").unwrap();
    }

    #[test]
    fn show_transaction_isolation_reflects_level() {
        let s = shared();
        let mut sess = new_session();
        // Inside a SERIALIZABLE transaction, SHOW reports it.
        run(&s, &mut sess, "BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
        let show = |sess: &Session| {
            let stmt = Parser::parse_sql("SHOW transaction_isolation")
                .unwrap()
                .remove(0);
            let db = sess.tx.as_ref().unwrap().db.clone();
            let mut db = db;
            match executor::execute(&mut db, stmt).unwrap() {
                ExecResult::Rows { rows, .. } => match &rows[0][0] {
                    Value::Text(t) => t.clone(),
                    _ => panic!("expected text"),
                },
                _ => panic!("expected rows"),
            }
        };
        assert_eq!(show(&sess), "serializable");
        run(&s, &mut sess, "ROLLBACK").unwrap();

        // READ UNCOMMITTED collapses to read committed.
        run(&s, &mut sess, "BEGIN ISOLATION LEVEL READ UNCOMMITTED").unwrap();
        assert_eq!(show(&sess), "read committed");
        run(&s, &mut sess, "ROLLBACK").unwrap();
    }

    #[test]
    fn read_only_transaction_rejects_writes() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer)").unwrap();
        run(&s, &mut sess, "BEGIN READ ONLY").unwrap();
        let stmt = Parser::parse_sql("INSERT INTO t VALUES (1)")
            .unwrap()
            .remove(0);
        let err = match run_statement(&s, &mut sess, &stmt) {
            Err(e) => e,
            Ok(_) => panic!("write must be rejected"),
        };
        assert_eq!(sqlstate_for(&err), "25006");
        run(&s, &mut sess, "ROLLBACK").unwrap();
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
        run(
            &s,
            &mut sess,
            "INSERT INTO t VALUES (1, 'Alice'), (2, 'Bob')",
        )
        .unwrap();

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

    /// A unique temp path under the OS temp dir for file-COPY tests.
    fn temp_copy_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::AtomicU64;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        std::env::temp_dir().join(format!("pgrs_copy_{tag}_{pid}_{n}.dat"))
    }

    /// Read back `t(id, name)` as (int, text) rows in id order.
    fn select_id_name(shared: &Shared) -> Vec<(i64, Option<String>)> {
        let mut db = shared.db.lock().unwrap();
        let stmt = Parser::parse_sql("SELECT id, name FROM t ORDER BY id")
            .unwrap()
            .remove(0);
        match executor::execute(&mut db, stmt).unwrap() {
            ExecResult::Rows { rows, .. } => rows
                .into_iter()
                .map(|r| {
                    let id = match r[0] {
                        Value::Int(i) => i,
                        _ => panic!("expected int id"),
                    };
                    let name = match &r[1] {
                        Value::Null => None,
                        Value::Text(s) => Some(s.clone()),
                        other => panic!("expected text name, got {other:?}"),
                    };
                    (id, name)
                })
                .collect(),
            _ => panic!("expected rows"),
        }
    }

    #[test]
    fn copy_to_file_then_from_file_round_trips() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer, name text)").unwrap();
        run(
            &s,
            &mut sess,
            "INSERT INTO t VALUES (1, 'Alice'), (2, 'Bob'), (3, NULL)",
        )
        .unwrap();

        let path = temp_copy_path("text");
        let p = path.to_str().unwrap();

        // COPY TO file (server-side, runs through the ordinary statement path).
        run(&s, &mut sess, &format!("COPY t TO '{p}'")).unwrap();
        assert!(path.exists(), "COPY TO did not create the file");

        // Reload into a fresh table from the file.
        run(&s, &mut sess, "DROP TABLE t").unwrap();
        run(&s, &mut sess, "CREATE TABLE t (id integer, name text)").unwrap();
        run(&s, &mut sess, &format!("COPY t FROM '{p}'")).unwrap();

        assert_eq!(
            select_id_name(&s),
            vec![
                (1, Some("Alice".into())),
                (2, Some("Bob".into())),
                (3, None),
            ]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn copy_query_to_file_csv() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer, name text)").unwrap();
        run(
            &s,
            &mut sess,
            "INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')",
        )
        .unwrap();

        let path = temp_copy_path("query");
        let p = path.to_str().unwrap();
        // COPY (SELECT ...) TO file.
        run(
            &s,
            &mut sess,
            &format!(
                "COPY (SELECT id, name FROM t WHERE id > 1 ORDER BY id) TO '{p}' WITH (FORMAT csv)"
            ),
        )
        .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "2,b\n3,c\n");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn copy_binary_file_round_trips() {
        let s = shared();
        let mut sess = new_session();
        run(
            &s,
            &mut sess,
            "CREATE TABLE t (id integer, name text, score double precision, ok boolean)",
        )
        .unwrap();
        run(
            &s,
            &mut sess,
            "INSERT INTO t VALUES (1, 'Alice', 9.5, true), (2, NULL, -1.25, false)",
        )
        .unwrap();
        let want = {
            let mut db = s.db.lock().unwrap();
            let stmt = Parser::parse_sql("SELECT id, name, score, ok FROM t ORDER BY id")
                .unwrap()
                .remove(0);
            match executor::execute(&mut db, stmt).unwrap() {
                ExecResult::Rows { rows, .. } => rows,
                _ => panic!("rows"),
            }
        };

        let path = temp_copy_path("bin");
        let p = path.to_str().unwrap();
        run(
            &s,
            &mut sess,
            &format!("COPY t TO '{p}' WITH (FORMAT binary)"),
        )
        .unwrap();

        run(&s, &mut sess, "DELETE FROM t").unwrap();
        run(
            &s,
            &mut sess,
            &format!("COPY t FROM '{p}' WITH (FORMAT binary)"),
        )
        .unwrap();

        let got = {
            let mut db = s.db.lock().unwrap();
            let stmt = Parser::parse_sql("SELECT id, name, score, ok FROM t ORDER BY id")
                .unwrap()
                .remove(0);
            match executor::execute(&mut db, stmt).unwrap() {
                ExecResult::Rows { rows, .. } => rows,
                _ => panic!("rows"),
            }
        };
        assert_eq!(got, want, "binary COPY did not round-trip");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn copy_binary_stdin_stdout_round_trips() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer, name text)").unwrap();
        run(&s, &mut sess, "INSERT INTO t VALUES (1, 'x'), (2, 'y')").unwrap();

        // COPY TO STDOUT (binary) — capture the CopyData payload.
        let mut reader = std::io::Cursor::new(Vec::new());
        let mut out = Vec::new();
        let copy = parse_copy("COPY t TO STDOUT WITH (FORMAT binary)");
        run_copy(&mut reader, &mut out, &s, &mut sess, &copy)
            .unwrap()
            .unwrap();
        // Extract the single CopyData ('d') message body.
        assert_eq!(out[0], b'H');
        let mut i = 0usize;
        let mut payload = Vec::new();
        while i < out.len() {
            let tag = out[i];
            let len = i32::from_be_bytes(out[i + 1..i + 5].try_into().unwrap()) as usize;
            // `len` counts itself (4 bytes) plus the body, but not the tag byte.
            let body = &out[i + 5..i + 1 + len];
            if tag == b'd' {
                payload = body.to_vec();
            }
            i += 1 + len;
        }
        assert_eq!(
            &payload[..COPY_BINARY_SIGNATURE.len()],
            COPY_BINARY_SIGNATURE
        );

        // Feed it back via COPY FROM STDIN (binary).
        run(&s, &mut sess, "DELETE FROM t").unwrap();
        let mut input = Vec::new();
        input.extend(frame(b'd', &payload));
        input.extend(frame(b'c', b""));
        let mut reader = std::io::Cursor::new(input);
        let mut out = Vec::new();
        let copy = parse_copy("COPY t FROM STDIN WITH (FORMAT binary)");
        run_copy(&mut reader, &mut out, &s, &mut sess, &copy)
            .unwrap()
            .unwrap();
        assert_eq!(
            select_id_name(&s),
            vec![(1, Some("x".into())), (2, Some("y".into()))]
        );
    }

    #[test]
    fn copy_from_stdin_bulk_loads_many_rows() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer, name text)").unwrap();

        // Build a few thousand text-format rows in one CopyData payload.
        const N: usize = 5000;
        let mut data = String::with_capacity(N * 12);
        for i in 0..N {
            data.push_str(&format!("{i}\trow{i}\n"));
        }
        let mut input = Vec::new();
        input.extend(frame(b'd', data.as_bytes()));
        input.extend(frame(b'c', b""));
        let mut reader = std::io::Cursor::new(input);
        let mut out = Vec::new();

        let copy = parse_copy("COPY t FROM STDIN");
        run_copy(&mut reader, &mut out, &s, &mut sess, &copy)
            .unwrap()
            .unwrap();

        assert_eq!(committed_rows(&s), N);
        assert!(window_contains(&out, format!("COPY {N}").as_bytes()));
        // Spot-check first and last rows survived the bulk path intact.
        assert_eq!(
            select_ints(&s, &sess, "SELECT id FROM t ORDER BY id LIMIT 1"),
            vec![0]
        );
        assert_eq!(
            select_ints(&s, &sess, "SELECT id FROM t ORDER BY id DESC LIMIT 1"),
            vec![(N - 1) as i64]
        );
    }

    #[test]
    fn copy_from_missing_file_errors_without_panic() {
        let s = shared();
        let mut sess = new_session();
        run(&s, &mut sess, "CREATE TABLE t (id integer)").unwrap();
        let missing = temp_copy_path("missing");
        let p = missing.to_str().unwrap();
        let err = run(&s, &mut sess, &format!("COPY t FROM '{p}'")).unwrap_err();
        assert!(err.contains("could not read COPY source"), "got: {err}");
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

    // --- locking integration tests ---------------------------------------

    /// Run a SELECT through the full `run_statement` path (so locking clauses
    /// take effect) and return the first column as ints.
    fn run_select_ints(s: &Shared, sess: &mut Session, sql: &str) -> Vec<i64> {
        let stmt = Parser::parse_sql(sql).unwrap().remove(0);
        match run_statement(s, sess, &stmt).unwrap() {
            ExecResult::Rows { rows, .. } => rows
                .into_iter()
                .map(|r| match r[0] {
                    Value::Int(i) => i,
                    _ => panic!("expected int"),
                })
                .collect(),
            _ => panic!("expected rows"),
        }
    }

    /// `run_statement` returning its `Err` (ExecResult isn't `Debug`, so
    /// `unwrap_err` is unavailable).
    fn run_stmt_err(s: &Shared, sess: &mut Session, sql: &str) -> String {
        let stmt = Parser::parse_sql(sql).unwrap().remove(0);
        match run_statement(s, sess, &stmt) {
            Err(e) => e,
            Ok(_) => panic!("expected error for {sql}"),
        }
    }

    /// A session with an explicit pid (so two share-the-Shared sessions are
    /// distinct to the lock manager).
    fn session_with_pid(pid: i32) -> Session {
        Session::new(
            pid,
            Arc::new(AtomicBool::new(false)),
            Arc::new(Mutex::new(Vec::new())),
        )
    }

    #[test]
    fn lock_table_requires_transaction_block() {
        let s = shared();
        let mut setup = new_session();
        run(&s, &mut setup, "CREATE TABLE t (id integer)").unwrap();
        let err = run_stmt_err(&s, &mut setup, "LOCK TABLE t");
        assert!(
            err.contains("can only be used in transaction blocks"),
            "{err}"
        );
    }

    #[test]
    fn table_lock_nowait_conflicts_then_succeeds_after_commit() {
        // Session A locks t EXCLUSIVE in a tx; B's NOWAIT attempt errors 55P03;
        // after A commits, B succeeds. Sequential calls, no real threads.
        let s = shared();
        let mut setup = new_session();
        run(&s, &mut setup, "CREATE TABLE t (id integer)").unwrap();

        let mut a = session_with_pid(10);
        let mut b = session_with_pid(20);
        run(&s, &mut a, "BEGIN").unwrap();
        run(&s, &mut a, "LOCK TABLE t IN EXCLUSIVE MODE").unwrap();

        run(&s, &mut b, "BEGIN").unwrap();
        let err = run_stmt_err(&s, &mut b, "LOCK TABLE t IN EXCLUSIVE MODE NOWAIT");
        assert_eq!(sqlstate_for(&err), "55P03", "{err}");
        // B's tx is now aborted; roll it back to clear.
        run(&s, &mut b, "ROLLBACK").unwrap();

        // A commits, releasing the lock; B can now lock t.
        run(&s, &mut a, "COMMIT").unwrap();
        let mut b2 = session_with_pid(20);
        run(&s, &mut b2, "BEGIN").unwrap();
        run(&s, &mut b2, "LOCK TABLE t IN EXCLUSIVE MODE NOWAIT").unwrap();
        run(&s, &mut b2, "COMMIT").unwrap();
    }

    #[test]
    fn access_share_does_not_conflict_with_access_share() {
        // Two readers can both hold ACCESS SHARE concurrently.
        let s = shared();
        let mut setup = new_session();
        run(&s, &mut setup, "CREATE TABLE t (id integer)").unwrap();

        let mut a = session_with_pid(10);
        let mut b = session_with_pid(20);
        run(&s, &mut a, "BEGIN").unwrap();
        run(&s, &mut a, "LOCK TABLE t IN ACCESS SHARE MODE").unwrap();
        run(&s, &mut b, "BEGIN").unwrap();
        run(&s, &mut b, "LOCK TABLE t IN ACCESS SHARE MODE NOWAIT").unwrap();
    }

    #[test]
    fn select_for_update_skip_locked_omits_locked_rows() {
        let s = shared();
        let mut setup = new_session();
        run(&s, &mut setup, "CREATE TABLE jobs (id integer)").unwrap();
        run(&s, &mut setup, "INSERT INTO jobs VALUES (1), (2), (3)").unwrap();

        // A locks all rows FOR UPDATE.
        let mut a = session_with_pid(10);
        run(&s, &mut a, "BEGIN").unwrap();
        let _ = run_select_ints(&s, &mut a, "SELECT id FROM jobs ORDER BY id FOR UPDATE");

        // B with SKIP LOCKED sees none of them (all locked by A).
        let mut b = session_with_pid(20);
        run(&s, &mut b, "BEGIN").unwrap();
        let got = run_select_ints(
            &s,
            &mut b,
            "SELECT id FROM jobs ORDER BY id FOR UPDATE SKIP LOCKED",
        );
        assert!(got.is_empty(), "expected all rows skipped, got {got:?}");

        run(&s, &mut a, "COMMIT").unwrap();
        // After A releases, B sees them all.
        let got = run_select_ints(
            &s,
            &mut b,
            "SELECT id FROM jobs ORDER BY id FOR UPDATE SKIP LOCKED",
        );
        assert_eq!(got, vec![1, 2, 3]);
    }

    #[test]
    fn select_for_update_nowait_errors_on_locked_row() {
        let s = shared();
        let mut setup = new_session();
        run(&s, &mut setup, "CREATE TABLE jobs (id integer)").unwrap();
        run(&s, &mut setup, "INSERT INTO jobs VALUES (1)").unwrap();

        let mut a = session_with_pid(10);
        run(&s, &mut a, "BEGIN").unwrap();
        let _ = run_select_ints(&s, &mut a, "SELECT id FROM jobs FOR UPDATE");

        let mut b = session_with_pid(20);
        run(&s, &mut b, "BEGIN").unwrap();
        let err = run_stmt_err(&s, &mut b, "SELECT id FROM jobs FOR UPDATE NOWAIT");
        assert_eq!(sqlstate_for(&err), "55P03", "{err}");
    }

    #[test]
    fn blocking_lock_unblocks_after_commit_real_threads() {
        // A real blocking test: B blocks on a table lock A holds, and proceeds
        // only once A commits. Synchronized with channels (no sleeps for
        // correctness; one short sleep only to bias scheduling, not relied on).
        use std::sync::mpsc;
        let s = Arc::new(shared());
        let mut setup = new_session();
        run(&s, &mut setup, "CREATE TABLE t (id integer)").unwrap();

        let mut a = session_with_pid(10);
        run(&s, &mut a, "BEGIN").unwrap();
        run(&s, &mut a, "LOCK TABLE t IN EXCLUSIVE MODE").unwrap();

        let (tx_started, rx_started) = mpsc::channel();
        let (tx_done, rx_done) = mpsc::channel();
        let s_b = Arc::clone(&s);
        let handle = thread::spawn(move || {
            let mut b = session_with_pid(20);
            run(&s_b, &mut b, "BEGIN").unwrap();
            tx_started.send(()).unwrap();
            // This blocks until A commits and releases the lock.
            run(&s_b, &mut b, "LOCK TABLE t IN EXCLUSIVE MODE").unwrap();
            tx_done.send(()).unwrap();
            run(&s_b, &mut b, "COMMIT").unwrap();
        });

        // Wait until B has started and is (about to be) blocked.
        rx_started.recv().unwrap();
        // B must NOT have acquired the lock yet (A still holds it).
        assert!(
            rx_done.recv_timeout(Duration::from_millis(200)).is_err(),
            "B acquired the lock while A still held it"
        );

        // Release by committing A; B should now proceed.
        run(&s, &mut a, "COMMIT").unwrap();
        rx_done
            .recv_timeout(Duration::from_secs(5))
            .expect("B did not unblock after A committed");
        handle.join().unwrap();
    }

    #[test]
    fn deadlock_is_detected_and_aborts_a_transaction() {
        // A holds table x and waits for y; B holds y and requests x, closing a
        // cycle. The lock manager must abort one with 40P01 rather than hang.
        use std::sync::mpsc;
        let s = Arc::new(shared());
        let mut setup = new_session();
        run(&s, &mut setup, "CREATE TABLE x (id integer)").unwrap();
        run(&s, &mut setup, "CREATE TABLE y (id integer)").unwrap();

        let mut a = session_with_pid(10);
        run(&s, &mut a, "BEGIN").unwrap();
        run(&s, &mut a, "LOCK TABLE x IN EXCLUSIVE MODE").unwrap();

        // B locks y, then signals it is ready, then asks for x (will block on A).
        let (tx_b_ready, rx_b_ready) = mpsc::channel();
        let (tx_b_result, rx_b_result) = mpsc::channel();
        let s_b = Arc::clone(&s);
        let handle = thread::spawn(move || {
            let mut b = session_with_pid(20);
            run(&s_b, &mut b, "BEGIN").unwrap();
            run(&s_b, &mut b, "LOCK TABLE y IN EXCLUSIVE MODE").unwrap();
            tx_b_ready.send(()).unwrap();
            let stmt = Parser::parse_sql("LOCK TABLE x IN EXCLUSIVE MODE")
                .unwrap()
                .remove(0);
            let r = run_statement(&s_b, &mut b, &stmt);
            tx_b_result.send(r.is_err()).unwrap();
            run(&s_b, &mut b, "ROLLBACK").unwrap();
        });

        rx_b_ready.recv().unwrap();
        // Give B a moment to actually park on x. We retry A's request which
        // closes the cycle; whichever side detects the cycle aborts with 40P01.
        // A now requests y (held by B): this would deadlock.
        let mut detected_on_a = false;
        // Spin until B is waiting on x (its wait-for edge is recorded), then
        // make A request y to form the cycle.
        loop {
            let waiting = {
                let lm = s.locks.lock().unwrap();
                !lm.wait_for_graph().is_empty()
            };
            if waiting {
                break;
            }
            thread::yield_now();
        }
        let stmt = Parser::parse_sql("LOCK TABLE y IN EXCLUSIVE MODE")
            .unwrap()
            .remove(0);
        match run_statement(&s, &mut a, &stmt) {
            Err(e) => {
                assert_eq!(sqlstate_for(&e), "40P01", "{e}");
                detected_on_a = true;
                run(&s, &mut a, "ROLLBACK").unwrap();
            }
            Ok(_) => {
                // A got y because B was the one aborted; release so B unblocks.
                run(&s, &mut a, "COMMIT").unwrap();
            }
        }

        let b_aborted = rx_b_result
            .recv_timeout(Duration::from_secs(5))
            .expect("B never completed: implementation deadlocked");
        // Exactly one side must have hit the deadlock abort.
        assert!(detected_on_a || b_aborted, "no side detected the deadlock");
        handle.join().unwrap();
    }
}
