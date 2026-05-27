//! Abstract syntax tree for the supported SQL subset.

use crate::types::DataType;

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    CreateTable(CreateTable),
    DropTable(DropTable),
    AlterTable(AlterTable),
    CreateIndex(CreateIndex),
    DropIndex(DropIndex),
    Insert(Insert),
    Select(Select),
    Update(Update),
    Delete(Delete),
    /// Transaction control. Currently executed as a no-op acknowledgement
    /// (everything is auto-committed) but parsed so clients don't error.
    Begin,
    Commit,
    Rollback,
    /// `SET name = value` — accepted and ignored.
    Set { name: String, value: String },
    /// `SHOW name` — returns a single-row, single-column result.
    Show { name: String },
    /// An empty statement (e.g. a lone `;`).
    Empty,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub data_type: DataType,
    pub not_null: bool,
    pub primary_key: bool,
    /// `DEFAULT <expr>` applied when the column is omitted from an INSERT.
    pub default: Option<Expr>,
    /// `serial`/`bigserial`/`smallserial`: auto-incrementing from a sequence.
    pub serial: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub if_not_exists: bool,
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
    AddColumn { column: ColumnDef, if_not_exists: bool },
    DropColumn { name: String, if_exists: bool },
    RenameColumn { from: String, to: String },
    RenameTable { to: String },
}

/// `CREATE [UNIQUE] INDEX [IF NOT EXISTS] [name] ON table (column)`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateIndex {
    /// Explicit index name, or `None` to auto-generate one.
    pub name: Option<String>,
    pub table: String,
    pub column: String,
    pub unique: bool,
    pub if_not_exists: bool,
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
    /// One inner `Vec` per `VALUES (...)` tuple.
    pub rows: Vec<Vec<Expr>>,
    /// `RETURNING` projection (empty when absent).
    pub returning: Vec<SelectItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    /// `SELECT DISTINCT` deduplicates the projected rows.
    pub distinct: bool,
    pub projection: Vec<SelectItem>,
    /// `None` for `SELECT <exprs>` with no `FROM`.
    pub from: Option<FromClause>,
    pub filter: Option<Expr>,
    /// `GROUP BY` expressions (empty when absent).
    pub group_by: Vec<Expr>,
    /// `HAVING` predicate, applied per group after aggregation.
    pub having: Option<Expr>,
    pub order_by: Vec<OrderByItem>,
    pub limit: Option<Expr>,
    pub offset: Option<Expr>,
}

/// A table reference with an optional alias, e.g. `users u`.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    /// Schema qualifier if written (`information_schema.tables` → `Some(...)`).
    pub schema: Option<String>,
    pub name: String,
    pub alias: Option<String>,
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

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub table: String,
    pub assignments: Vec<(String, Expr)>,
    pub filter: Option<Expr>,
    pub returning: Vec<SelectItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub table: String,
    pub filter: Option<Expr>,
    pub returning: Vec<SelectItem>,
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
    Binary { op: BinaryOp, left: Box<Expr>, right: Box<Expr> },
    /// `expr IS [NOT] NULL`.
    IsNull { expr: Box<Expr>, negated: bool },
    /// `expr [NOT] LIKE/ILIKE pattern`.
    Like { expr: Box<Expr>, pattern: Box<Expr>, negated: bool, case_insensitive: bool },
    /// `expr [NOT] IN (list)`.
    InList { expr: Box<Expr>, list: Vec<Expr>, negated: bool },
    /// `expr [NOT] BETWEEN low AND high`.
    Between { expr: Box<Expr>, low: Box<Expr>, high: Box<Expr>, negated: bool },
    /// `CASE [operand] WHEN cond THEN result ... [ELSE result] END`.
    Case { operand: Option<Box<Expr>>, whens: Vec<(Expr, Expr)>, else_expr: Option<Box<Expr>> },
    /// `CAST(expr AS type)` or `expr::type`.
    Cast { expr: Box<Expr>, target: DataType },
    /// A scalar subquery `(SELECT ...)` yielding one value.
    ScalarSubquery(Box<Select>),
    /// `EXISTS (SELECT ...)`.
    Exists(Box<Select>),
    /// `expr [NOT] IN (SELECT ...)`.
    InSubquery { expr: Box<Expr>, subquery: Box<Select>, negated: bool },
    /// A function call, e.g. `count(*)`, `upper(name)`, or `count(DISTINCT x)`.
    Function { name: String, args: Vec<Expr>, star: bool, distinct: bool },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
    /// POSIX regex match `~` / `~*` (case-insensitive).
    RegexMatch { ci: bool },
    /// Negated regex match `!~` / `!~*`.
    RegexNotMatch { ci: bool },
}
