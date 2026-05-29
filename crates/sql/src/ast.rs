//! Abstract syntax tree for the supported SQL subset.

use crate::types::DataType;

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable(CreateTable),
    CreateExtension(CreateExtension),
    CreateRole(CreateRole),
    CreateSequence(CreateSequence),
    CreateSchema(CreateSchema),
    CreateDatabase(CreateDatabase),
    CreateTablespace(CreateTablespace),
    CreateCollation(CreateCollation),
    CreateType(CreateType),
    CreateDomain(CreateDomain),
    CreateView(CreateView),
    CreateMaterializedView(CreateMaterializedView),
    CreateFunction(CreateFunction),
    CreateTrigger(CreateTrigger),
    CreateRule(CreateRule),
    CreateAggregate(CreateAggregate),
    DropTable(DropTable),
    DropExtension(DropExtension),
    DropRole(DropRole),
    DropSequence(DropSequence),
    DropSchema(DropSchema),
    DropDatabase(DropDatabase),
    DropTablespace(DropTablespace),
    DropCollation(DropCollation),
    DropType(DropType),
    DropDomain(DropDomain),
    DropView(DropView),
    DropMaterializedView(DropMaterializedView),
    DropFunction(DropFunction),
    DropTrigger(DropTrigger),
    DropRule(DropRule),
    DropAggregate(DropAggregate),
    AlterTable(AlterTable),
    CreatePolicy(CreatePolicy),
    AlterPolicy(AlterPolicy),
    DropPolicy(DropPolicy),
    AlterRole(AlterRole),
    AlterSequence(AlterSequence),
    CreateIndex(CreateIndex),
    DropIndex(DropIndex),
    Insert(Insert),
    Copy(Copy),
    Truncate(Truncate),
    DeclareCursor(DeclareCursor),
    Fetch(Fetch),
    Select(Select),
    AlterDatabase(AlterDatabase),
    Update(Update),
    Delete(Delete),
    Merge(Merge),
    Explain(Explain),
    Analyze(Analyze),
    Comment(Comment),
    SecurityLabel(SecurityLabel),
    Grant(Grant),
    Revoke(Revoke),
    AlterSystem(AlterSystem),
    Vacuum(Vacuum),
    Reindex(Reindex),
    Cluster(Cluster),
    Checkpoint,
    Discard(Discard),
    Listen {
        channel: String,
    },
    Notify {
        channel: String,
        payload: Option<String>,
    },
    Unlisten {
        channel: Option<String>,
    },
    LockTable(LockTable),
    RefreshMaterializedView(RefreshMaterializedView),
    /// Transaction control. `BEGIN`/`START TRANSACTION`, optionally carrying an
    /// isolation level and/or read-only mode (`BEGIN [TRANSACTION] [ISOLATION
    /// LEVEL <lvl>] [READ ONLY|READ WRITE]`). The session layer (server.rs)
    /// honors these; executed directly it is a no-op acknowledgement.
    Begin {
        isolation: Option<IsolationLevel>,
        read_only: Option<bool>,
    },
    Commit,
    Rollback,
    Savepoint {
        name: String,
    },
    ReleaseSavepoint {
        name: String,
    },
    RollbackToSavepoint {
        name: String,
    },
    /// `SET [SESSION|LOCAL] name {=|TO} value` — stores a runtime configuration
    /// parameter (GUC). `local` reflects `SET LOCAL` (transaction-scoped in real
    /// PostgreSQL; see the executor for the simplification taken here).
    Set {
        name: String,
        value: String,
        local: bool,
    },
    /// `RESET name` / `RESET ALL` / `SET name TO DEFAULT` — restore a GUC (or all
    /// GUCs when `name` is `None`) to its built-in default.
    ResetConfig {
        name: Option<String>,
    },
    /// `SHOW name` / `SHOW ALL` — returns configuration parameter rows.
    Show {
        name: String,
    },
    /// `CREATE` of an extended catalog object that is accepted and stored by
    /// name but not otherwise interpreted (operator classes/families, operators,
    /// event triggers, FDWs, servers, user mappings, publications,
    /// subscriptions). See [`CatalogObjectKind`].
    CreateCatalogObject(CatalogObject),
    /// `DROP` of an extended catalog object (see [`CreateCatalogObject`]).
    DropCatalogObject(DropCatalogObject),
    /// `SET CONSTRAINTS ... { DEFERRED | IMMEDIATE }` — accepted no-op.
    SetConstraints,
    /// `SET TRANSACTION ...` / `SET SESSION CHARACTERISTICS AS TRANSACTION ...`:
    /// set the isolation level and/or read-only mode of the current transaction
    /// (or the session default when `session` is true).
    SetTransaction {
        isolation: Option<IsolationLevel>,
        read_only: Option<bool>,
        /// `true` for `SET SESSION CHARACTERISTICS AS TRANSACTION ...`, which
        /// changes the default for subsequent transactions.
        session: bool,
    },
    /// `PREPARE TRANSACTION 'gid'` — two-phase commit prepare.
    PrepareTransaction {
        gid: String,
    },
    /// `COMMIT PREPARED 'gid'`.
    CommitPrepared {
        gid: String,
    },
    /// `ROLLBACK PREPARED 'gid'`.
    RollbackPrepared {
        gid: String,
    },
    /// An empty statement (e.g. a lone `;`).
    Empty,
}

/// Transaction isolation level. `READ UNCOMMITTED` is accepted but treated as
/// `READ COMMITTED` (as PostgreSQL does — it has no dirty reads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl IsolationLevel {
    /// The canonical lowercase name PostgreSQL reports for
    /// `transaction_isolation` (READ UNCOMMITTED collapses to read committed).
    pub fn guc_value(self) -> &'static str {
        match self {
            IsolationLevel::ReadUncommitted | IsolationLevel::ReadCommitted => "read committed",
            IsolationLevel::RepeatableRead => "repeatable read",
            IsolationLevel::Serializable => "serializable",
        }
    }

    /// Parse from a GUC string value (case-insensitive). Returns `None` for an
    /// unrecognized level.
    pub fn from_guc_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "read uncommitted" => Some(IsolationLevel::ReadUncommitted),
            "read committed" => Some(IsolationLevel::ReadCommitted),
            "repeatable read" => Some(IsolationLevel::RepeatableRead),
            "serializable" => Some(IsolationLevel::Serializable),
            _ => None,
        }
    }
}

/// The kind of an extended catalog object that is accepted and stored by name
/// (no enforcement / behavior). Used by [`Statement::CreateCatalogObject`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CatalogObjectKind {
    OperatorClass,
    OperatorFamily,
    Operator,
    EventTrigger,
    ForeignDataWrapper,
    Server,
    UserMapping,
    Publication,
    Subscription,
}

impl CatalogObjectKind {
    /// The SQL keyword phrase used in command tags and serialization.
    pub fn keyword(self) -> &'static str {
        match self {
            CatalogObjectKind::OperatorClass => "OPERATOR CLASS",
            CatalogObjectKind::OperatorFamily => "OPERATOR FAMILY",
            CatalogObjectKind::Operator => "OPERATOR",
            CatalogObjectKind::EventTrigger => "EVENT TRIGGER",
            CatalogObjectKind::ForeignDataWrapper => "FOREIGN DATA WRAPPER",
            CatalogObjectKind::Server => "SERVER",
            CatalogObjectKind::UserMapping => "USER MAPPING",
            CatalogObjectKind::Publication => "PUBLICATION",
            CatalogObjectKind::Subscription => "SUBSCRIPTION",
        }
    }
}

/// A `CREATE` of an extended catalog object. The `definition` is the verbatim
/// remainder of the statement (after the object name), kept so the statement
/// round-trips through the WAL unchanged.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogObject {
    pub kind: CatalogObjectKind,
    pub name: String,
    /// The verbatim tail of the statement following the name, reproduced as-is
    /// for WAL replay. Empty when nothing followed.
    pub definition: String,
}

/// A `DROP` of an extended catalog object.
#[derive(Debug, Clone, PartialEq)]
pub struct DropCatalogObject {
    pub kind: CatalogObjectKind,
    pub name: String,
    pub if_exists: bool,
    /// The verbatim tail following the name (e.g. operator signature), kept for
    /// round-tripping. Empty when nothing followed.
    pub definition: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Explain {
    pub analyze: bool,
    pub statement: Box<Statement>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Analyze {
    pub table: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Vacuum {
    pub table: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reindex {
    pub target: ReindexTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cluster {
    pub table: Option<String>,
    pub index: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReindexTarget {
    Table(String),
    Index(String),
    Database(String),
    System(Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Discard {
    pub target: DiscardTarget,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DiscardTarget {
    All,
    Plans,
    Sequences,
    Temp,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LockTable {
    pub tables: Vec<String>,
    /// The requested mode spelling (e.g. "ACCESS EXCLUSIVE"); `None` means the
    /// PostgreSQL default of ACCESS EXCLUSIVE. Kept as text so the AST stays
    /// independent of the lock manager's `LockMode` type.
    pub mode: Option<String>,
    /// `LOCK TABLE ... NOWAIT`: fail instead of blocking on a conflict.
    pub nowait: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub object: CommentObject,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SecurityLabel {
    pub provider: String,
    pub object: CommentObject,
    pub label: Option<String>,
}

/// A privilege that may be granted/revoked on a table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Privilege {
    Select,
    Insert,
    Update,
    Delete,
    Truncate,
    References,
    Trigger,
}

impl Privilege {
    pub fn as_str(self) -> &'static str {
        match self {
            Privilege::Select => "SELECT",
            Privilege::Insert => "INSERT",
            Privilege::Update => "UPDATE",
            Privilege::Delete => "DELETE",
            Privilege::Truncate => "TRUNCATE",
            Privilege::References => "REFERENCES",
            Privilege::Trigger => "TRIGGER",
        }
    }
}

/// The set of privileges named by a GRANT/REVOKE on a table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Privileges {
    /// `ALL [PRIVILEGES]`.
    All,
    /// An explicit list.
    List(Vec<Privilege>),
}

/// A grantee: a named role or `PUBLIC`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grantee {
    Role(String),
    Public,
}

/// What a GRANT/REVOKE applies to.
#[derive(Debug, Clone, PartialEq)]
pub enum GrantObject {
    /// Privileges on a table.
    Table {
        privileges: Privileges,
        table: String,
    },
    /// Role membership: grant `roles` (membership in them) to grantees.
    Roles { roles: Vec<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Grant {
    pub object: GrantObject,
    pub grantees: Vec<Grantee>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Revoke {
    pub object: GrantObject,
    pub grantees: Vec<Grantee>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlterSystem {
    pub action: AlterSystemAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlterSystemAction {
    Set { name: String, value: String },
    Reset { name: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CommentObject {
    Relation { name: String },
    Column { table: String, column: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateExtension {
    pub name: String,
    pub if_not_exists: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropExtension {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateRole {
    pub name: String,
    pub login: bool,
    pub options: RoleOptions,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlterRole {
    pub name: String,
    pub options: RoleOptions,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropRole {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateSequence {
    pub name: String,
    pub if_not_exists: bool,
    pub start: i64,
    pub increment: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlterSequence {
    pub name: String,
    pub restart: Option<i64>,
    pub increment: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropSequence {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoleOptions {
    pub superuser: Option<bool>,
    pub inherit: Option<bool>,
    pub create_role: Option<bool>,
    pub create_db: Option<bool>,
    pub login: Option<bool>,
    pub replication: Option<bool>,
    pub bypass_rls: Option<bool>,
    pub connection_limit: Option<i64>,
    pub password: Option<Option<String>>,
    pub valid_until: Option<Option<String>>,
    /// `IN ROLE name[,...]`: this role becomes a member of the named roles.
    pub in_role: Vec<String>,
    /// `ROLE name[,...]`: the named roles become members of this role.
    pub role_members: Vec<String>,
    /// `ADMIN name[,...]`: like `ROLE` but with admin option (stored as membership).
    pub admin_members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateSchema {
    pub name: String,
    pub if_not_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropSchema {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateDatabase {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropDatabase {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTablespace {
    pub name: String,
    pub location: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropTablespace {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateCollation {
    pub name: String,
    pub if_not_exists: bool,
    pub locale: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropCollation {
    pub name: String,
    pub if_exists: bool,
}

/// `CREATE TYPE name AS ENUM (...) | AS (...) | AS RANGE (...)`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateType {
    pub name: String,
    pub kind: CreateTypeKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CreateTypeKind {
    /// `AS ENUM ('a', 'b', ...)`: an ordered list of text labels.
    Enum { labels: Vec<String> },
    /// `AS (attr type, ...)`: a composite/row type.
    Composite { attributes: Vec<(String, DataType)> },
    /// `AS RANGE (subtype = type, ...)`: a range type over a subtype.
    Range { subtype: DataType },
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropType {
    pub name: String,
    pub if_exists: bool,
}

/// `CREATE DOMAIN name [AS] base [NOT NULL] [CHECK (...)]`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateDomain {
    pub name: String,
    pub base: DataType,
    pub not_null: bool,
    /// `CHECK (VALUE ...)` predicate; `VALUE` refers to the inserted value.
    pub check: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropDomain {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlterDatabase {
    pub name: String,
    pub action: AlterDatabaseAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlterDatabaseAction {
    Rename { to: String },
    SetConnectionLimit { limit: i64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateView {
    pub name: String,
    pub or_replace: bool,
    pub select: Box<Select>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropView {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateMaterializedView {
    pub name: String,
    pub if_not_exists: bool,
    pub select: Box<Select>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropMaterializedView {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RefreshMaterializedView {
    pub name: String,
}

/// A formal argument of a `CREATE FUNCTION`/`CREATE AGGREGATE`.
#[derive(Debug, Clone, PartialEq)]
pub struct FunctionArg {
    /// Argument name, if written (`amount integer`). `None` for unnamed args.
    pub name: Option<String>,
    pub data_type: DataType,
    /// The raw lowercased type name as written, used to round-trip and to
    /// identify a function by its argument-type signature.
    pub type_name: String,
}

/// `CREATE [OR REPLACE] FUNCTION name(args) RETURNS rettype AS $$ body $$
/// LANGUAGE sql`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateFunction {
    pub name: String,
    pub or_replace: bool,
    pub args: Vec<FunctionArg>,
    /// The declared return type name as written (lowercased), or `None` when a
    /// `RETURNS` clause was omitted (e.g. trigger functions returning void).
    pub return_type: Option<DataType>,
    pub return_type_name: Option<String>,
    /// The function body extracted from the dollar-quoted (or string) literal.
    pub body: String,
    /// The language given by `LANGUAGE <lang>` (lowercased); defaults to `sql`.
    pub language: String,
    /// `SECURITY DEFINER` was specified (`prosecdef`). `SECURITY INVOKER` (the
    /// default) leaves this false. Behaviorally moot single-user, but recorded
    /// and exposed via `pg_proc.prosecdef`.
    pub security_definer: bool,
}

/// `DROP FUNCTION [IF EXISTS] name [(argtypes)]`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropFunction {
    pub name: String,
    pub if_exists: bool,
    /// Argument type names, when an explicit signature was written. `None`
    /// means "drop by name" (only valid when the name is unambiguous).
    pub arg_types: Option<Vec<String>>,
}

/// The DML events a trigger fires on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEvent {
    Insert,
    Update,
    Delete,
}

/// Whether a trigger runs before or after the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerTiming {
    Before,
    After,
}

/// `CREATE TRIGGER name {BEFORE|AFTER} {INSERT|UPDATE|DELETE [OR ...]} ON table
/// FOR EACH ROW EXECUTE {FUNCTION|PROCEDURE} fname()`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTrigger {
    pub name: String,
    pub timing: TriggerTiming,
    pub events: Vec<TriggerEvent>,
    pub table: String,
    /// `true` for `FOR EACH ROW`, `false` for `FOR EACH STATEMENT`.
    pub for_each_row: bool,
    /// The trigger function name (the `()` argument list is accepted but empty).
    pub function: String,
}

/// `DROP TRIGGER [IF EXISTS] name ON table`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropTrigger {
    pub name: String,
    pub table: String,
    pub if_exists: bool,
}

/// `CREATE [OR REPLACE] RULE name AS ON event TO table [WHERE ...]
/// DO [ALSO|INSTEAD] (...)`. Accepted and catalogued; not applied.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateRule {
    pub name: String,
    pub or_replace: bool,
    pub event: TriggerEvent,
    pub table: String,
    /// The verbatim definition text following `AS`, kept for round-tripping.
    pub definition: String,
}

/// `DROP RULE [IF EXISTS] name ON table`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropRule {
    pub name: String,
    pub table: String,
    pub if_exists: bool,
}

/// `CREATE [OR REPLACE] AGGREGATE name(argtype) (SFUNC=..., STYPE=..., ...)`.
/// Accepted and catalogued; not applied.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateAggregate {
    pub name: String,
    pub or_replace: bool,
    /// Input argument type names (lowercased), or `["*"]` for `(*)`.
    pub arg_types: Vec<String>,
    /// `(key = value, ...)` options, in written order.
    pub options: Vec<(String, String)>,
}

/// `DROP AGGREGATE [IF EXISTS] name(argtypes)`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropAggregate {
    pub name: String,
    pub if_exists: bool,
    pub arg_types: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Truncate {
    pub tables: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeclareCursor {
    pub name: String,
    pub select: Box<Select>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fetch {
    pub cursor: String,
    pub count: FetchCount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchCount {
    Next,
    All,
    Count(i64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    /// The declared type name when it is a user-defined type (enum/domain/
    /// composite/range) rather than a built-in. `None` for built-in types.
    /// Lets the executor look the column up in the type/domain catalogs to
    /// enforce enum-label and domain constraints. Stored lowercased.
    pub type_name: Option<String>,
    pub not_null: bool,
    pub primary_key: bool,
    /// `DEFAULT <expr>` applied when the column is omitted from an INSERT.
    pub default: Option<Expr>,
    /// `serial`/`bigserial`/`smallserial`: auto-incrementing from a sequence.
    pub serial: bool,
    /// `GENERATED ... AS IDENTITY`: sequence-backed integer values.
    pub identity: bool,
    /// Identity mode: `GENERATED ALWAYS` rejects explicit values unless overridden.
    pub identity_always: bool,
    /// `GENERATED ALWAYS AS (<expr>) STORED`: computed on insert/update.
    pub generated: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<TableConstraint>,
    pub if_not_exists: bool,
    pub persistence: TablePersistence,
    /// `INHERITS (parent1, parent2, ...)`: parent tables whose columns are
    /// prepended to this table's own, and whose `SELECT` scans include this
    /// table's rows.
    pub inherits: Vec<String>,
    /// `PARTITION BY {RANGE|LIST|HASH} (col)`: marks this as a partitioned
    /// parent that routes inserted rows to its partitions.
    pub partition_by: Option<PartitionBy>,
    /// `PARTITION OF parent FOR VALUES ...`: declares this table a partition of
    /// `parent`, inheriting its columns and bound.
    pub partition_of: Option<PartitionOf>,
}

/// The partitioning strategy of a partitioned parent table.
#[derive(Debug, Clone, PartialEq)]
pub struct PartitionBy {
    pub strategy: PartitionStrategy,
    /// The partition key column name.
    pub column: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartitionStrategy {
    Range,
    List,
    Hash,
}

/// `PARTITION OF parent FOR VALUES ...`.
#[derive(Debug, Clone, PartialEq)]
pub struct PartitionOf {
    pub parent: String,
    pub bound: PartitionBound,
}

/// The bound of a partition, mirroring `FOR VALUES`.
#[derive(Debug, Clone, PartialEq)]
pub enum PartitionBound {
    /// `FROM (lo) TO (hi)`: range `lo <= v < hi`.
    Range { from: Expr, to: Expr },
    /// `IN (v, ...)`: membership.
    List(Vec<Expr>),
    /// `WITH (MODULUS m, REMAINDER r)`: `hash(v) % m == r`.
    Hash { modulus: i64, remainder: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TablePersistence {
    Permanent,
    Unlogged,
    Temporary,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DropTable {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AlterTable {
    pub table: String,
    pub action: AlterAction,
}

#[derive(Debug, Clone, PartialEq)]
pub enum AlterAction {
    AddColumn {
        column: ColumnDef,
        if_not_exists: bool,
    },
    AddConstraint {
        constraint: TableConstraint,
    },
    DropColumn {
        name: String,
        if_exists: bool,
    },
    DropConstraint {
        name: String,
        if_exists: bool,
    },
    RenameColumn {
        from: String,
        to: String,
    },
    RenameTable {
        to: String,
    },
    /// `OWNER TO role`.
    OwnerTo {
        owner: String,
    },
    /// `{ENABLE|DISABLE|FORCE|NO FORCE} ROW LEVEL SECURITY`.
    RowSecurity {
        action: RowSecurityAction,
    },
    /// `ALTER COLUMN col SET DEFAULT expr`.
    SetColumnDefault {
        column: String,
        default: Expr,
    },
    /// `ALTER COLUMN col DROP DEFAULT`.
    DropColumnDefault {
        column: String,
    },
    /// `ALTER COLUMN col {SET|DROP} NOT NULL`.
    SetColumnNotNull {
        column: String,
        not_null: bool,
    },
    /// Per-column attribute tweaks that this engine does not model
    /// (`SET STORAGE`, `SET STATISTICS`, `SET (...)`, `RESET (...)`,
    /// `SET COMPRESSION`). Accepted as a no-op so `pg_dump` output replays.
    AlterColumnNoop,
    /// Other table-level clauses accepted but not modeled (`SET (...)`,
    /// `CLUSTER ON`, `SET WITHOUT CLUSTER`, `INHERIT`, etc.).
    Noop,
}

/// The row-level-security toggle in `ALTER TABLE ... ROW LEVEL SECURITY`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowSecurityAction {
    Enable,
    Disable,
    Force,
    NoForce,
}

/// `CREATE POLICY name ON table [AS PERMISSIVE|RESTRICTIVE] [FOR cmd]
/// [TO role[,...]] [USING (expr)] [WITH CHECK (expr)]`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreatePolicy {
    pub name: String,
    pub table: String,
    /// `true` = PERMISSIVE (default), `false` = RESTRICTIVE.
    pub permissive: bool,
    /// The command: `"all"` (default), `"select"`, `"insert"`, `"update"`, `"delete"`.
    pub command: String,
    /// Roles in `TO`; empty means `PUBLIC`.
    pub roles: Vec<String>,
    pub using: Option<Expr>,
    pub with_check: Option<Expr>,
}

/// `ALTER POLICY name ON table [TO role[,...]] [USING (expr)] [WITH CHECK (expr)]`.
#[derive(Debug, Clone, PartialEq)]
pub struct AlterPolicy {
    pub name: String,
    pub table: String,
    pub roles: Option<Vec<String>>,
    pub using: Option<Expr>,
    pub with_check: Option<Expr>,
}

/// `DROP POLICY [IF EXISTS] name ON table`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropPolicy {
    pub name: String,
    pub table: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableConstraint {
    Unique {
        name: String,
        columns: Vec<String>,
        primary_key: bool,
    },
    Check {
        name: String,
        expr: Expr,
        validated: bool,
    },
    ForeignKey {
        name: String,
        column: String,
        ref_table: String,
        ref_column: String,
        validated: bool,
    },
    /// `EXCLUDE [USING method] (element WITH operator, ...)`. Accepted and
    /// stored as a named constraint; never enforced. The `definition` is the
    /// verbatim text following the constraint name (from `EXCLUDE` onward),
    /// kept so the constraint round-trips through the WAL.
    Exclude { name: String, definition: String },
}

/// `CREATE [UNIQUE] INDEX [IF NOT EXISTS] [name] ON table [USING method]
/// (key, ...) [INCLUDE (cols)] [WHERE predicate]`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndex {
    /// Explicit index name, or `None` to auto-generate one.
    pub name: Option<String>,
    pub table: String,
    /// The indexed keys, in order. Each key is either a plain column or an
    /// arbitrary expression (`((lower(name)))`).
    pub keys: Vec<IndexKeyExpr>,
    pub unique: bool,
    pub if_not_exists: bool,
    /// Access method: `btree` (default) or `hash`.
    pub method: IndexMethod,
    /// `INCLUDE (col, ...)` covering columns.
    pub include: Vec<String>,
    /// `WHERE <predicate>` partial-index condition.
    pub predicate: Option<Expr>,
}

/// One key of an index: a bare column or a parenthesised expression.
#[derive(Debug, Clone, PartialEq)]
pub enum IndexKeyExpr {
    /// A plain column name.
    Column(String),
    /// An expression, e.g. `lower(name)`.
    Expr(Expr),
}

/// The access method backing an index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexMethod {
    Btree,
    Hash,
    Gist,
    SpGist,
    Brin,
    Gin,
}

/// `DROP INDEX [IF EXISTS] name`.
#[derive(Debug, Clone, PartialEq)]
pub struct DropIndex {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub table: String,
    /// Explicit column list, or `None` for "all columns in table order".
    pub columns: Option<Vec<String>>,
    /// `INSERT INTO t DEFAULT VALUES`.
    pub default_values: bool,
    /// `OVERRIDING SYSTEM VALUE` allows explicit values for GENERATED ALWAYS identity columns.
    pub overriding_system_value: bool,
    /// One inner `Vec` per `VALUES (...)` tuple.
    pub rows: Vec<Vec<Expr>>,
    /// `INSERT INTO t SELECT ...`.
    pub select: Option<Box<Select>>,
    pub on_conflict: Option<OnConflict>,
    /// `RETURNING` projection (empty when absent).
    pub returning: Vec<SelectItem>,
}

/// `COPY table [(cols)] FROM <src> | TO <dst> [WITH (...)]`, and
/// `COPY (SELECT ...) TO <dst> [WITH (...)]`.
///
/// The endpoint is given by [`Copy::target`]: `STDIN`/`STDOUT` use the COPY
/// sub-protocol (driven by the server with the connection socket), while a
/// single-quoted file path runs entirely server-side via `std::fs`.
#[derive(Debug, Clone, PartialEq)]
pub struct Copy {
    /// Target table for a `COPY table ...`. For `COPY (query) TO ...` this is
    /// the empty string and [`Copy::query`] holds the source query instead.
    pub table: String,
    /// `COPY (SELECT ...) TO <dst>`: the source query. Mutually exclusive with a
    /// named `table`; only valid with [`CopyDirection::To`].
    pub query: Option<Box<Select>>,
    /// Explicit column list, or `None` for all columns in table order.
    pub columns: Option<Vec<String>>,
    pub direction: CopyDirection,
    /// Where the data comes from / goes to (STDIN/STDOUT or a file path).
    pub target: CopyTarget,
    pub format: CopyFormat,
    /// Field delimiter; defaults to tab (text) or comma (CSV) when `None`.
    pub delimiter: Option<char>,
    /// `HEADER` option (CSV): skip/emit a header row.
    pub header: bool,
    /// String that represents SQL NULL; defaults to `\N` (text) or empty (CSV).
    pub null: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyDirection {
    /// `COPY ... FROM ...` — data flows into the table.
    From,
    /// `COPY ... TO ...` — data flows out of the table/query.
    To,
}

/// The endpoint of a COPY: the client stream (STDIN/STDOUT) or a server file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyTarget {
    /// `FROM STDIN` — client streams rows to the server (COPY sub-protocol).
    Stdin,
    /// `TO STDOUT` — server streams rows to the client (COPY sub-protocol).
    Stdout,
    /// `FROM '<path>'` / `TO '<path>'` — a server-side file, read/written with
    /// `std::fs`. No COPY sub-protocol messages are exchanged.
    File(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyFormat {
    Text,
    Csv,
    /// PostgreSQL COPY binary format (`WITH (FORMAT binary)`).
    Binary,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OnConflict {
    DoNothing {
        target: Vec<String>,
    },
    DoUpdate {
        target: Vec<String>,
        assignments: Vec<(String, Expr)>,
        filter: Option<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Select {
    /// Non-recursive `WITH name AS (<select>)` bindings visible to this SELECT.
    pub ctes: Vec<Cte>,
    /// `SELECT DISTINCT` deduplicates the projected rows.
    pub distinct: bool,
    /// `SELECT DISTINCT ON (...)` keeps the first projected row per key.
    pub distinct_on: Vec<Expr>,
    pub projection: Vec<SelectItem>,
    /// `None` for `SELECT <exprs>` with no `FROM`.
    pub from: Option<FromClause>,
    pub filter: Option<Expr>,
    /// `GROUP BY` expressions (empty when absent). Used for the ordinary
    /// single-grouping-set case (when `grouping_sets` is empty).
    pub group_by: Vec<Expr>,
    /// Expanded grouping sets from `GROUP BY GROUPING SETS (...)`, `ROLLUP(...)`
    /// or `CUBE(...)`. Each inner `Vec` is one grouping set (an empty inner
    /// `Vec` is the grand total). Empty outer `Vec` means no grouping sets were
    /// used and the ordinary `group_by` path applies.
    pub grouping_sets: Vec<Vec<Expr>>,
    /// `HAVING` predicate, applied per group after aggregation.
    pub having: Option<Expr>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
    pub locking: Vec<RowLockingClause>,
    pub set_ops: Vec<SetOperation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cte {
    pub name: String,
    pub columns: Vec<String>,
    pub select: Box<Select>,
    /// A data-modifying CTE body (`INSERT`/`UPDATE`/`DELETE ... RETURNING`).
    /// When present, the CTE relation is materialised from the statement's
    /// `RETURNING` rows and `select` is an unused placeholder. `None` for an
    /// ordinary read-only CTE.
    pub dml: Option<Box<Statement>>,
    /// `true` when declared under `WITH RECURSIVE`; the CTE's body may
    /// reference itself and is evaluated iteratively to a fixpoint.
    pub recursive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowLockingClause {
    pub mode: RowLockingMode,
    pub tables: Vec<String>,
    pub wait_policy: Option<RowLockingWaitPolicy>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLockingMode {
    Update,
    NoKeyUpdate,
    Share,
    KeyShare,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowLockingWaitPolicy {
    NoWait,
    SkipLocked,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SetOperation {
    pub op: SetOperator,
    pub all: bool,
    pub select: Box<Select>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOperator {
    Union,
    Intersect,
    Except,
}

/// A table reference with an optional alias, e.g. `users u`.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    /// Schema qualifier if written (`information_schema.tables` → `Some(...)`).
    pub schema: Option<String>,
    pub name: String,
    /// Function arguments when this is a set-returning function in FROM.
    pub args: Vec<Expr>,
    pub alias: Option<String>,
    /// A parenthesised subquery in FROM (derived table), e.g. `(SELECT ...) s`.
    /// When present, `name` is the alias (also used as the qualifier).
    pub subquery: Option<Box<Select>>,
    /// `true` when prefixed with `LATERAL`: the subquery or set-returning
    /// function may reference columns from preceding FROM items.
    pub lateral: bool,
    /// `true` when prefixed with `ONLY`: restrict the scan to this table's own
    /// rows, excluding inheritance children / partitions.
    pub only: bool,
    /// Explicit output column-alias list, e.g. `AS perm(acl, row_n)`. Empty when
    /// none was written. For set-returning functions these rename the produced
    /// columns; combined with `with_ordinality` the trailing alias names the
    /// ordinality column.
    pub column_aliases: Vec<String>,
    /// `true` when `WITH ORDINALITY` was written on a set-returning function in
    /// FROM, appending a `bigint` ordinality column (1-based) to the output.
    pub with_ordinality: bool,
}

impl TableRef {
    /// The name used to qualify this table's columns (`alias` if present).
    pub fn qualifier(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub table: TableRef,
    /// The `ON` predicate; `None` for `CROSS JOIN`.
    pub on: Option<Expr>,
}

/// A `FROM` clause: a base table plus zero or more joins.
#[derive(Debug, Clone, PartialEq)]
pub struct FromClause {
    pub base: TableRef,
    pub joins: Vec<Join>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    /// `*`
    Wildcard,
    /// An expression with an optional `AS alias`.
    Expr { expr: Expr, alias: Option<String> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderByItem {
    pub expr: Expr,
    pub asc: bool,
}

/// An `OVER (...)` window specification. Frame clauses are parsed but the
/// executor uses the SQL default frame, so they are not retained here.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderByItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub table: String,
    pub assignments: Vec<(String, Expr)>,
    pub from: Option<FromClause>,
    pub filter: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub table: String,
    pub using: Option<FromClause>,
    pub filter: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

/// `MERGE INTO target [AS alias] USING source [AS alias] ON cond WHEN ...`.
#[derive(Debug, Clone, PartialEq)]
pub struct Merge {
    pub target: String,
    /// Alias for the target table; defaults to the target name when absent.
    pub target_alias: Option<String>,
    pub source: MergeSource,
    /// The `ON` join condition, visible to both target and source columns.
    pub on: Expr,
    pub clauses: Vec<MergeWhen>,
}

impl Merge {
    /// The qualifier used for the target's columns (`target_alias` else name).
    pub fn target_qualifier(&self) -> &str {
        self.target_alias.as_deref().unwrap_or(&self.target)
    }
}

/// The data source of a MERGE: a table, a parenthesized subquery, or a
/// `(VALUES ...)` construct. All forms carry the alias used to qualify the
/// produced columns.
#[derive(Debug, Clone, PartialEq)]
pub enum MergeSource {
    /// A named table (`USING src s`).
    Table { name: String, alias: Option<String> },
    /// A parenthesized subquery (`USING (SELECT ...) AS s`).
    Subquery {
        select: Box<Select>,
        alias: String,
    },
    /// A `(VALUES (...), ...) AS s(col, ...)` construct.
    Values {
        rows: Vec<Vec<Expr>>,
        alias: String,
        columns: Vec<String>,
    },
}

impl MergeSource {
    /// The qualifier used for this source's columns.
    pub fn qualifier(&self) -> &str {
        match self {
            MergeSource::Table { name, alias } => alias.as_deref().unwrap_or(name),
            MergeSource::Subquery { alias, .. } | MergeSource::Values { alias, .. } => alias,
        }
    }
}

/// One `WHEN [NOT] MATCHED [AND cond] THEN action` clause of a MERGE.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeWhen {
    /// `true` for `WHEN MATCHED`, `false` for `WHEN NOT MATCHED`.
    pub matched: bool,
    /// Optional `AND <cond>` extra predicate gating this clause.
    pub condition: Option<Expr>,
    pub action: MergeAction,
}

/// The action a MERGE clause performs once it is selected.
#[derive(Debug, Clone, PartialEq)]
pub enum MergeAction {
    /// `UPDATE SET col = expr [, ...]` (only valid for `WHEN MATCHED`).
    Update { assignments: Vec<(String, Expr)> },
    /// `DELETE` (only valid for `WHEN MATCHED`).
    Delete,
    /// `INSERT [(cols)] VALUES (...)` or `INSERT DEFAULT VALUES`
    /// (only valid for `WHEN NOT MATCHED`).
    Insert {
        columns: Option<Vec<String>>,
        values: Vec<Expr>,
        default_values: bool,
    },
    /// `DO NOTHING`.
    DoNothing,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Integer literal.
    Int(i64),
    /// Floating-point literal.
    Float(f64),
    /// String literal.
    Str(String),
    /// Boolean literal.
    Bool(bool),
    /// `NULL`.
    Null,
    /// A positional parameter placeholder `$N` (1-based), filled in at Bind.
    Param(u32),
    /// An unqualified column reference (`col`).
    Column(String),
    /// A qualified column reference (`table.col` or `alias.col`).
    QualifiedColumn { qualifier: String, name: String },
    /// Unary operator applied to an operand.
    Unary { op: UnaryOp, expr: Box<Expr> },
    /// Binary operator.
    Binary {
        op: BinaryOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// `left <op> ANY/SOME/ALL (value, ...)`.
    QuantifiedCompare {
        left: Box<Expr>,
        op: BinaryOp,
        quantifier: Quantifier,
        list: Vec<Expr>,
    },
    /// `ROW(...)` or tuple-style `(a, b)` row constructor.
    Row(Vec<Expr>),
    /// `ARRAY[...]` array constructor.
    Array(Vec<Expr>),
    /// `expr IS [NOT] NULL`.
    IsNull { expr: Box<Expr>, negated: bool },
    /// `left IS [NOT] DISTINCT FROM right`.
    IsDistinctFrom {
        left: Box<Expr>,
        right: Box<Expr>,
        negated: bool,
    },
    /// `expr [NOT] LIKE/ILIKE pattern`.
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        negated: bool,
        case_insensitive: bool,
    },
    /// `expr [NOT] IN (list)`.
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `expr [NOT] BETWEEN low AND high`.
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    /// `CASE [operand] WHEN cond THEN result ... [ELSE result] END`.
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        else_expr: Option<Box<Expr>>,
    },
    /// `CAST(expr AS type)` or `expr::type`.
    Cast { expr: Box<Expr>, target: DataType },
    /// A scalar subquery `(SELECT ...)` yielding one value.
    ScalarSubquery(Box<Select>),
    /// `EXISTS (SELECT ...)`.
    Exists(Box<Select>),
    /// `expr [NOT] IN (SELECT ...)`.
    InSubquery {
        expr: Box<Expr>,
        subquery: Box<Select>,
        negated: bool,
    },
    /// A function call, e.g. `count(*)`, `upper(name)`, or `count(DISTINCT x)`.
    Function {
        name: String,
        args: Vec<Expr>,
        star: bool,
        distinct: bool,
        filter: Option<Box<Expr>>,
        /// `Some` when the call carries an `OVER (...)` window specification,
        /// turning it into a window function.
        over: Option<Box<WindowSpec>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantifier {
    Any,
    Some,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
    /// JSON extraction `left -> key_or_index`.
    JsonGet,
    /// JSON text extraction `left ->> key_or_index`.
    JsonGetText,
    /// Array contains `left @> right`.
    ArrayContains,
    /// Array contained-by `left <@ right`.
    ArrayContainedBy,
    /// Array overlap `left && right`.
    ArrayOverlap,
    /// Network containment operators.
    NetworkContainedBy,
    NetworkContainedByEq,
    NetworkContains,
    NetworkContainsEq,
    /// Full-text search match `tsvector @@ tsquery`.
    TextSearchMatch,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    /// POSIX regex match `~` / `~*` (case-insensitive).
    RegexMatch {
        ci: bool,
    },
    /// Negated regex match `!~` / `!~*`.
    RegexNotMatch {
        ci: bool,
    },
}
