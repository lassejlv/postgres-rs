use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use postgres_rs::executor::{self, ExecResult};
use postgres_rs::sql::Parser;
use postgres_rs::sql::serialize::statement_to_sql;
use postgres_rs::storage::Database;
use postgres_rs::types::Value;

fn run(db: &mut Database, sql: &str) -> ExecResult {
    let stmts = Parser::parse_sql(sql).expect("parse");
    let mut last = ExecResult::Empty;
    for stmt in stmts {
        last = executor::execute(db, stmt).expect("execute");
    }
    last
}

fn try_run(db: &mut Database, sql: &str) -> Result<ExecResult, String> {
    let stmts = Parser::parse_sql(sql).expect("parse");
    let mut last = ExecResult::Empty;
    for stmt in stmts {
        last = executor::execute(db, stmt)?;
    }
    Ok(last)
}

fn rows(res: ExecResult) -> Vec<Vec<Value>> {
    match res {
        ExecResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {}", tag_of(&other)),
    }
}

fn tag_of(res: &ExecResult) -> String {
    match res {
        ExecResult::Rows { .. } => "Rows".into(),
        ExecResult::Command(t) => format!("Command({t})"),
        ExecResult::Empty => "Empty".into(),
    }
}

#[test]
fn language_c_scalar_functions_load_and_execute() {
    let Some(lib_path) = build_native_test_library() else {
        eprintln!("skipping C extension ABI test: no usable cc compiler");
        return;
    };
    let lib = sql_literal(&lib_path.display().to_string());
    let mut db = Database::new();

    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_add(a integer, b integer) RETURNS integer \
             AS {lib}, 'pgrs_c_add' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_hello() RETURNS text \
             AS {lib}, 'pgrs_c_hello' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_not(flag boolean) RETURNS boolean \
             AS {lib}, 'pgrs_c_not' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_suffix(s text) RETURNS text \
             AS {lib}, 'pgrs_c_suffix' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_spi_answer() RETURNS integer \
             AS {lib}, 'pgrs_c_spi_answer' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_spi_sum() RETURNS integer \
             AS {lib}, 'pgrs_c_spi_sum' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_spi_mutate() RETURNS integer \
             AS {lib}, 'pgrs_c_spi_mutate' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_spi_plan_answer() RETURNS integer \
             AS {lib}, 'pgrs_c_spi_plan_answer' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_spi_binval() RETURNS integer \
             AS {lib}, 'pgrs_c_spi_binval' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_fmgr_cache(step integer) RETURNS integer \
             AS {lib}, 'pgrs_c_fmgr_cache' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_context_switch(step integer) RETURNS integer \
             AS {lib}, 'pgrs_c_context_switch' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_pg_init_seen() RETURNS integer \
             AS {lib}, 'pgrs_c_pg_init_seen' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_strict_probe(v integer) RETURNS integer \
             AS {lib}, 'pgrs_c_strict_probe' LANGUAGE c STRICT"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_notice(v integer) RETURNS integer \
             AS {lib}, 'pgrs_c_notice' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_elog_error() RETURNS integer \
             AS {lib}, 'pgrs_c_elog_error' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_ereport_error() RETURNS integer \
             AS {lib}, 'pgrs_c_ereport_error' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_get_guc(name text) RETURNS text \
             AS {lib}, 'pgrs_c_get_guc' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_custom_guc_defaults() RETURNS text \
             AS {lib}, 'pgrs_c_custom_guc_defaults' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_type_by_oid(oid integer) RETURNS text \
             AS {lib}, 'pgrs_c_type_by_oid' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_type_by_name(name text) RETURNS integer \
             AS {lib}, 'pgrs_c_type_by_name' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_type_by_name_wrong_ns(name text) RETURNS integer \
             AS {lib}, 'pgrs_c_type_by_name_wrong_ns' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_proc_metadata() RETURNS text \
             AS {lib}, 'pgrs_c_proc_metadata' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_catalog_helpers() RETURNS text \
             AS {lib}, 'pgrs_c_catalog_helpers' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        "CREATE TABLE spi_numbers(n integer); \
         INSERT INTO spi_numbers VALUES (10), (20), (12); \
         CREATE TABLE spi_log(n integer);",
    );

    assert_eq!(
        rows(run(
            &mut db,
            "SELECT c_add(2, 3), c_hello(), c_not(false), c_suffix('rust'), \
                    c_spi_answer(), c_spi_sum(), c_spi_mutate(), c_spi_plan_answer(), \
                    c_spi_binval()"
        )),
        vec![vec![
            Value::Int(5),
            Value::Text("hello from c".into()),
            Value::Bool(true),
            Value::Text("rust!".into()),
            Value::Int(42),
            Value::Int(42),
            Value::Int(11),
            Value::Int(42),
            Value::Int(1),
        ]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT n FROM spi_log ORDER BY n")),
        vec![vec![Value::Int(11)]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_fmgr_cache(1)")),
        vec![vec![Value::Int(101)]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_fmgr_cache(2)")),
        vec![vec![Value::Int(103)]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_context_switch(5)")),
        vec![vec![Value::Int(205)]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_context_switch(7)")),
        vec![vec![Value::Int(212)]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_pg_init_seen()")),
        vec![vec![Value::Int(1)]]
    );
    assert_eq!(
        rows(run(
            &mut db,
            "SELECT c_strict_probe(5), c_strict_probe(NULL)"
        )),
        vec![vec![Value::Int(6), Value::Null]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_notice(12)")),
        vec![vec![Value::Int(13)]]
    );
    let elog_err = match try_run(&mut db, "SELECT c_elog_error()") {
        Ok(_) => panic!("elog(ERROR) must abort the C function"),
        Err(err) => err,
    };
    assert!(elog_err.contains("native elog 9"));
    let ereport_err = match try_run(&mut db, "SELECT c_ereport_error()") {
        Ok(_) => panic!("ereport(ERROR) must abort the C function"),
        Err(err) => err,
    };
    assert!(ereport_err.contains("native ereport bad"));
    run(&mut db, "SET pgrs.c_abi_setting = 'from-sql'");
    assert_eq!(
        rows(run(
            &mut db,
            "SELECT c_get_guc('pgrs.c_abi_setting'), c_get_guc('missing.c_abi_setting')"
        )),
        vec![vec![Value::Text("from-sql".into()), Value::Null]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_custom_guc_defaults()")),
        vec![vec![Value::Text("boot:on:42".into())]]
    );
    assert_eq!(
        rows(run(
            &mut db,
            "SELECT c_type_by_oid(25), c_type_by_name('int4'), \
                    c_type_by_name('missing'), c_type_by_name_wrong_ns('int4')"
        )),
        vec![vec![
            Value::Text("text:-1:0:S".into()),
            Value::Int(23),
            Value::Null,
            Value::Null,
        ]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_proc_metadata()")),
        vec![vec![Value::Text("c_proc_metadata:0:0:25".into())]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_catalog_helpers()")),
        vec![vec![Value::Text(
            "text:-1:1:i:c_catalog_helpers:25:0".into()
        )]]
    );
    assert_eq!(
        rows(run(
            &mut db,
            "SELECT proisstrict FROM pg_proc WHERE proname = 'c_strict_probe'"
        )),
        vec![vec![Value::Bool(true)]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_add(NULL, 3), c_not(NULL)")),
        vec![vec![Value::Null, Value::Null]]
    );
}

#[test]
fn language_c_function_round_trips_link_symbol() {
    let sql = "CREATE FUNCTION c_add(a integer, b integer) RETURNS integer \
               AS '/tmp/libpgrs_test.dylib', 'pgrs_c_add' LANGUAGE c STRICT";
    let stmt = Parser::parse_sql(sql).expect("parse").remove(0);
    let rendered = statement_to_sql(&stmt);
    assert_eq!(
        rendered,
        "CREATE FUNCTION c_add(a integer, b integer) RETURNS integer \
         AS '/tmp/libpgrs_test.dylib', 'pgrs_c_add' LANGUAGE c STRICT"
    );
    Parser::parse_sql(&rendered).expect("reparse rendered c function");
}

#[test]
fn create_extension_loads_control_file_script_and_update_script() {
    let Some(lib_path) = build_native_test_library() else {
        eprintln!("skipping C extension ABI test: no usable cc compiler");
        return;
    };
    let dir = unique_temp_dir();
    fs::create_dir_all(&dir).expect("create extension temp dir");
    fs::write(
        dir.join("pgrs_file.control"),
        format!(
            "default_version = '1.0'\nmodule_path = '{}'\n",
            lib_path.display()
        ),
    )
    .expect("write extension control");
    fs::write(
        dir.join("pgrs_file--1.0.sql"),
        "CREATE FUNCTION ext_answer() RETURNS integer \
         AS 'MODULE_PATHNAME', 'pgrs_ext_answer_v1' LANGUAGE c;",
    )
    .expect("write extension install script");
    fs::write(
        dir.join("pgrs_file--1.0--2.0.sql"),
        "CREATE OR REPLACE FUNCTION ext_answer() RETURNS integer \
         AS 'MODULE_PATHNAME', 'pgrs_ext_answer_v2' LANGUAGE c;",
    )
    .expect("write extension update script");

    let _guard = extension_env_lock().lock().expect("lock extension env");
    unsafe {
        std::env::set_var("PGRS_EXTENSION_DIR", &dir);
    }
    let mut db = Database::new();
    run(&mut db, "CREATE EXTENSION pgrs_file");
    assert_eq!(
        rows(run(&mut db, "SELECT ext_answer()")),
        vec![vec![Value::Int(7)]]
    );
    assert_eq!(
        rows(run(
            &mut db,
            "SELECT p.proname, d.deptype \
             FROM pg_depend d \
             JOIN pg_extension e ON d.refobjid = e.oid \
             JOIN pg_proc p ON d.objid = p.oid \
             WHERE e.extname = 'pgrs_file' \
             ORDER BY p.proname"
        )),
        vec![vec![
            Value::Text("ext_answer".into()),
            Value::Text("e".into()),
        ]]
    );
    assert_eq!(
        rows(run(
            &mut db,
            "SELECT extversion FROM pg_extension WHERE extname = 'pgrs_file'"
        )),
        vec![vec![Value::Text("1.0".into())]]
    );

    run(&mut db, "ALTER EXTENSION pgrs_file UPDATE TO '2.0'");
    assert_eq!(
        rows(run(&mut db, "SELECT ext_answer()")),
        vec![vec![Value::Int(8)]]
    );
    assert_eq!(
        rows(run(
            &mut db,
            "SELECT extversion FROM pg_extension WHERE extname = 'pgrs_file'"
        )),
        vec![vec![Value::Text("2.0".into())]]
    );

    run(&mut db, "DROP EXTENSION pgrs_file CASCADE");
    assert!(
        try_run(&mut db, "SELECT ext_answer()").is_err(),
        "dropping an extension must remove its owned C functions"
    );
    assert_eq!(
        rows(run(
            &mut db,
            "SELECT extname FROM pg_extension WHERE extname = 'pgrs_file'"
        )),
        Vec::<Vec<Value>>::new()
    );
}

fn extension_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn build_native_test_library() -> Option<PathBuf> {
    if Command::new("cc").arg("--version").output().is_err() {
        return None;
    }

    let dir = unique_temp_dir();
    fs::create_dir_all(&dir).expect("create native test temp dir");
    let c_path = dir.join("pgrs_native_test.c");
    let lib_path = dir.join(dynamic_library_name(&dir));
    fs::write(&c_path, native_test_source()).expect("write native test source");

    let mut cmd = Command::new("cc");
    if cfg!(target_os = "macos") {
        cmd.arg("-dynamiclib");
        cmd.arg(format!("-Wl,-install_name,{}", lib_path.display()));
    } else {
        cmd.args(["-shared", "-fPIC"]);
    }
    let status = cmd
        .arg("-o")
        .arg(&lib_path)
        .arg("-I")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("include"))
        .arg(&c_path)
        .status()
        .expect("run cc");
    if status.success() {
        Some(lib_path)
    } else {
        panic!("cc failed to build native C extension test library");
    }
}

fn unique_temp_dir() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_nanos();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("postgres_rs_c_abi_{nanos}_{counter}"))
}

fn dynamic_library_name(dir: &std::path::Path) -> String {
    let unique = dir
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("native_test");
    if cfg!(target_os = "macos") {
        format!("libpgrs_native_test_{unique}.dylib")
    } else {
        format!("libpgrs_native_test_{unique}.so")
    }
}

fn sql_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn native_test_source() -> &'static str {
    r#"
#include "postgres_rs_fmgr.h"

PG_MODULE_MAGIC;
PG_FUNCTION_INFO_V1(pgrs_c_add);
PG_FUNCTION_INFO_V1(pgrs_c_hello);
PG_FUNCTION_INFO_V1(pgrs_c_not);
PG_FUNCTION_INFO_V1(pgrs_c_suffix);
PG_FUNCTION_INFO_V1(pgrs_c_spi_answer);
PG_FUNCTION_INFO_V1(pgrs_c_spi_sum);
PG_FUNCTION_INFO_V1(pgrs_c_spi_mutate);
PG_FUNCTION_INFO_V1(pgrs_c_spi_plan_answer);
PG_FUNCTION_INFO_V1(pgrs_c_spi_binval);
PG_FUNCTION_INFO_V1(pgrs_c_fmgr_cache);
PG_FUNCTION_INFO_V1(pgrs_c_context_switch);
PG_FUNCTION_INFO_V1(pgrs_c_pg_init_seen);
PG_FUNCTION_INFO_V1(pgrs_c_strict_probe);
PG_FUNCTION_INFO_V1(pgrs_c_notice);
PG_FUNCTION_INFO_V1(pgrs_c_elog_error);
PG_FUNCTION_INFO_V1(pgrs_c_ereport_error);
PG_FUNCTION_INFO_V1(pgrs_c_get_guc);
PG_FUNCTION_INFO_V1(pgrs_c_custom_guc_defaults);
PG_FUNCTION_INFO_V1(pgrs_c_type_by_oid);
PG_FUNCTION_INFO_V1(pgrs_c_type_by_name);
PG_FUNCTION_INFO_V1(pgrs_c_type_by_name_wrong_ns);
PG_FUNCTION_INFO_V1(pgrs_c_proc_metadata);
PG_FUNCTION_INFO_V1(pgrs_c_catalog_helpers);
PG_FUNCTION_INFO_V1(pgrs_ext_answer_v1);
PG_FUNCTION_INFO_V1(pgrs_ext_answer_v2);

static int pgrs_pg_init_count = 0;
static char *pgrs_custom_text = NULL;
static bool pgrs_custom_bool = false;
static int pgrs_custom_int = 0;

void _PG_init(void) {
    pgrs_pg_init_count++;
    DefineCustomStringVariable(
        "pgrs.custom_text",
        "postgres-rs c abi text setting",
        NULL,
        &pgrs_custom_text,
        "boot",
        PGC_USERSET,
        GUC_NOT_IN_SAMPLE,
        NULL,
        NULL,
        NULL
    );
    DefineCustomBoolVariable(
        "pgrs.custom_bool",
        "postgres-rs c abi bool setting",
        NULL,
        &pgrs_custom_bool,
        true,
        PGC_USERSET,
        GUC_NOT_IN_SAMPLE,
        NULL,
        NULL,
        NULL
    );
    DefineCustomIntVariable(
        "pgrs.custom_int",
        "postgres-rs c abi int setting",
        NULL,
        &pgrs_custom_int,
        42,
        0,
        100,
        PGC_USERSET,
        GUC_NOT_IN_SAMPLE,
        NULL,
        NULL,
        NULL
    );
}

Datum pgrs_c_add(PG_FUNCTION_ARGS) {
    if (PG_ARGISNULL(0) || PG_ARGISNULL(1)) {
        PG_RETURN_NULL();
    }
    PG_RETURN_INT32(PG_GETARG_INT32(0) + PG_GETARG_INT32(1));
}

Datum pgrs_c_hello(PG_FUNCTION_ARGS) {
    (void)fcinfo;
    static struct {
        uint32_t len;
        char data[12];
    } msg = {
        sizeof(uint32_t) + 12,
        {'h','e','l','l','o',' ','f','r','o','m',' ','c'}
    };
    PG_RETURN_TEXT_P((varlena *)&msg);
}

Datum pgrs_c_not(PG_FUNCTION_ARGS) {
    if (PG_ARGISNULL(0)) {
        PG_RETURN_NULL();
    }
    PG_RETURN_BOOL(!PG_GETARG_BOOL(0));
}

Datum pgrs_c_suffix(PG_FUNCTION_ARGS) {
    if (PG_ARGISNULL(0)) {
        PG_RETURN_NULL();
    }
    char *input = text_to_cstring(PG_GETARG_TEXT_P(0));
    size_t len = strlen(input);
    char *out = (char *)palloc(len + 2);
    memcpy(out, input, len);
    out[len] = '!';
    out[len + 1] = '\0';
    PG_RETURN_TEXT_P(cstring_to_text(out));
}

Datum pgrs_c_spi_answer(PG_FUNCTION_ARGS) {
    if (SPI_connect() != SPI_OK_CONNECT) {
        PG_RETURN_NULL();
    }
    int rc = SPI_execute("SELECT 40 + 2", true, 1);
    if (rc != SPI_OK_SELECT || SPI_processed < 1 || SPI_tuptable == NULL) {
        PG_RETURN_NULL();
    }
    char *value = SPI_getvalue(SPI_tuptable->vals[0], SPI_tuptable->tupdesc, 1);
    if (value == NULL) {
        PG_RETURN_NULL();
    }
    int answer = atoi(value);
    SPI_finish();
    PG_RETURN_INT32(answer);
}

Datum pgrs_c_spi_sum(PG_FUNCTION_ARGS) {
    if (SPI_connect() != SPI_OK_CONNECT) {
        PG_RETURN_NULL();
    }
    int rc = SPI_execute("SELECT n FROM spi_numbers ORDER BY n", true, 0);
    if (rc != SPI_OK_SELECT || SPI_processed < 1 || SPI_tuptable == NULL) {
        PG_RETURN_NULL();
    }
    int total = 0;
    for (size_t i = 0; i < SPI_processed; i++) {
        char *value = SPI_getvalue(SPI_tuptable->vals[i], SPI_tuptable->tupdesc, 1);
        if (value == NULL) {
            PG_RETURN_NULL();
        }
        total += atoi(value);
    }
    SPI_finish();
    PG_RETURN_INT32(total);
}

Datum pgrs_c_spi_mutate(PG_FUNCTION_ARGS) {
    if (SPI_connect() != SPI_OK_CONNECT) {
        PG_RETURN_NULL();
    }
    int rc = SPI_execute("INSERT INTO spi_log VALUES (1), (2)", false, 0);
    if (rc != SPI_OK_INSERT || SPI_result != rc || SPI_processed != 2) {
        PG_RETURN_NULL();
    }
    rc = SPI_execute("UPDATE spi_log SET n = n + 10 WHERE n = 1", false, 0);
    if (rc != SPI_OK_UPDATE || SPI_result != rc || SPI_processed != 1) {
        PG_RETURN_NULL();
    }
    rc = SPI_execute("DELETE FROM spi_log WHERE n = 2", false, 0);
    if (rc != SPI_OK_DELETE || SPI_result != rc || SPI_processed != 1) {
        PG_RETURN_NULL();
    }
    rc = SPI_execute("SELECT n FROM spi_log ORDER BY n", true, 0);
    if (rc != SPI_OK_SELECT || SPI_tuptable == NULL || SPI_tuptable->numvals != 1) {
        PG_RETURN_NULL();
    }
    char *value = SPI_getvalue(SPI_tuptable->vals[0], SPI_tuptable->tupdesc, 1);
    if (value == NULL) {
        PG_RETURN_NULL();
    }
    int remaining = atoi(value);
    SPI_finish();
    PG_RETURN_INT32(remaining);
}

Datum pgrs_c_spi_plan_answer(PG_FUNCTION_ARGS) {
    if (SPI_connect() != SPI_OK_CONNECT) {
        PG_RETURN_NULL();
    }
    SPIPlanPtr plan = SPI_prepare("SELECT 21 + 21", 0, NULL);
    if (plan == NULL) {
        PG_RETURN_NULL();
    }
    int rc = SPI_execute_plan(plan, NULL, NULL, true, 1);
    SPI_freeplan(plan);
    if (rc != SPI_OK_SELECT || SPI_processed != 1 || SPI_tuptable == NULL) {
        PG_RETURN_NULL();
    }
    char *value = SPI_getvalue(SPI_tuptable->vals[0], SPI_tuptable->tupdesc, 1);
    if (value == NULL) {
        PG_RETURN_NULL();
    }
    int answer = atoi(value);
    SPI_finish();
    PG_RETURN_INT32(answer);
}

Datum pgrs_c_spi_binval(PG_FUNCTION_ARGS) {
    if (SPI_connect() != SPI_OK_CONNECT) {
        PG_RETURN_NULL();
    }
    int rc = SPI_execute("SELECT 7, 'bin', NULL", true, 1);
    if (rc != SPI_OK_SELECT || SPI_processed != 1 || SPI_tuptable == NULL) {
        PG_RETURN_NULL();
    }
    bool isnull = false;
    Datum int_datum = SPI_getbinval(SPI_tuptable->vals[0], SPI_tuptable->tupdesc, 1, &isnull);
    if (isnull || DatumGetInt32(int_datum) != 7) {
        PG_RETURN_NULL();
    }
    Datum text_datum = SPI_getbinval(SPI_tuptable->vals[0], SPI_tuptable->tupdesc, 2, &isnull);
    if (isnull) {
        PG_RETURN_NULL();
    }
    char *text_value = text_to_cstring((text *)DatumGetPointer(text_datum));
    if (strcmp(text_value, "bin") != 0) {
        PG_RETURN_NULL();
    }
    (void)SPI_getbinval(SPI_tuptable->vals[0], SPI_tuptable->tupdesc, 3, &isnull);
    if (!isnull) {
        PG_RETURN_NULL();
    }
    SPI_finish();
    PG_RETURN_INT32(1);
}

Datum pgrs_c_fmgr_cache(PG_FUNCTION_ARGS) {
    if (fcinfo->flinfo == NULL || fcinfo->flinfo->fn_nargs != 1 || fcinfo->flinfo->fn_oid == 0) {
        PG_RETURN_NULL();
    }
    int *cached = (int *)fcinfo->flinfo->fn_extra;
    if (cached == NULL) {
        cached = (int *)MemoryContextAllocZero(fcinfo->flinfo->fn_mcxt, sizeof(int));
        *cached = 100;
        fcinfo->flinfo->fn_extra = cached;
    }
    *cached += PG_GETARG_INT32(0);
    PG_RETURN_INT32(*cached);
}

Datum pgrs_c_context_switch(PG_FUNCTION_ARGS) {
    if (fcinfo->flinfo == NULL || fcinfo->flinfo->fn_mcxt == NULL || CurrentMemoryContext == NULL) {
        PG_RETURN_NULL();
    }
    int *cached = (int *)fcinfo->flinfo->fn_extra;
    if (cached == NULL) {
        MemoryContext old = MemoryContextSwitchTo(fcinfo->flinfo->fn_mcxt);
        cached = (int *)palloc0(sizeof(int));
        MemoryContextSwitchTo(old);
        *cached = 200;
        fcinfo->flinfo->fn_extra = cached;
    }
    *cached += PG_GETARG_INT32(0);
    PG_RETURN_INT32(*cached);
}

Datum pgrs_c_pg_init_seen(PG_FUNCTION_ARGS) {
    (void)fcinfo;
    PG_RETURN_INT32(pgrs_pg_init_count);
}

Datum pgrs_c_strict_probe(PG_FUNCTION_ARGS) {
    if (fcinfo->flinfo == NULL || !fcinfo->flinfo->fn_strict) {
        PG_RETURN_NULL();
    }
    if (PG_ARGISNULL(0)) {
        PG_RETURN_INT32(777);
    }
    PG_RETURN_INT32(PG_GETARG_INT32(0) + 1);
}

Datum pgrs_c_notice(PG_FUNCTION_ARGS) {
    elog(NOTICE, "native notice %d", PG_GETARG_INT32(0));
    PG_RETURN_INT32(PG_GETARG_INT32(0) + 1);
}

Datum pgrs_c_elog_error(PG_FUNCTION_ARGS) {
    elog(ERROR, "native elog %d", 9);
    PG_RETURN_INT32(0);
}

Datum pgrs_c_ereport_error(PG_FUNCTION_ARGS) {
    ereport(ERROR, (errcode(ERRCODE_INVALID_PARAMETER_VALUE), errmsg("native ereport %s", "bad")));
    PG_RETURN_INT32(0);
}

Datum pgrs_c_get_guc(PG_FUNCTION_ARGS) {
    if (PG_ARGISNULL(0)) {
        PG_RETURN_NULL();
    }
    char *name = text_to_cstring(PG_GETARG_TEXT_P(0));
    const char *varname = NULL;
    const char *value = GetConfigOptionByName(name, &varname, true);
    if (value == NULL || varname == NULL || strcmp(varname, name) != 0) {
        PG_RETURN_NULL();
    }
    PG_RETURN_TEXT_P(cstring_to_text(value));
}

Datum pgrs_c_custom_guc_defaults(PG_FUNCTION_ARGS) {
    (void)fcinfo;
    char buf[64];
    snprintf(
        buf,
        sizeof(buf),
        "%s:%s:%d",
        pgrs_custom_text == NULL ? "null" : pgrs_custom_text,
        pgrs_custom_bool ? "on" : "off",
        pgrs_custom_int
    );
    PG_RETURN_TEXT_P(cstring_to_text(buf));
}

Datum pgrs_c_type_by_oid(PG_FUNCTION_ARGS) {
    if (PG_ARGISNULL(0)) {
        PG_RETURN_NULL();
    }
    HeapTuple tuple = SearchSysCache1(TYPEOID, ObjectIdGetDatum((Oid)PG_GETARG_INT32(0)));
    if (!HeapTupleIsValid(tuple)) {
        PG_RETURN_NULL();
    }
    Form_pg_type form = (Form_pg_type)GETSTRUCT(tuple);
    char buf[128];
    snprintf(
        buf,
        sizeof(buf),
        "%s:%d:%d:%c",
        form->typname,
        (int)form->typlen,
        form->typbyval ? 1 : 0,
        form->typcategory
    );
    ReleaseSysCache(tuple);
    PG_RETURN_TEXT_P(cstring_to_text(buf));
}

Datum pgrs_c_type_by_name(PG_FUNCTION_ARGS) {
    if (PG_ARGISNULL(0)) {
        PG_RETURN_NULL();
    }
    char *name = text_to_cstring(PG_GETARG_TEXT_P(0));
    HeapTuple tuple = SearchSysCache2(
        TYPENAMENSP,
        CStringGetDatum(name),
        ObjectIdGetDatum(PG_CATALOG_NAMESPACE_OID)
    );
    if (!HeapTupleIsValid(tuple)) {
        PG_RETURN_NULL();
    }
    Form_pg_type form = (Form_pg_type)GETSTRUCT(tuple);
    Oid oid = form->oid;
    ReleaseSysCache(tuple);
    PG_RETURN_INT32((int32_t)oid);
}

Datum pgrs_c_type_by_name_wrong_ns(PG_FUNCTION_ARGS) {
    if (PG_ARGISNULL(0)) {
        PG_RETURN_NULL();
    }
    char *name = text_to_cstring(PG_GETARG_TEXT_P(0));
    HeapTuple tuple = SearchSysCache2(
        TYPENAMENSP,
        CStringGetDatum(name),
        ObjectIdGetDatum((Oid)999999)
    );
    if (!HeapTupleIsValid(tuple)) {
        PG_RETURN_NULL();
    }
    Form_pg_type form = (Form_pg_type)GETSTRUCT(tuple);
    Oid oid = form->oid;
    ReleaseSysCache(tuple);
    PG_RETURN_INT32((int32_t)oid);
}

Datum pgrs_c_proc_metadata(PG_FUNCTION_ARGS) {
    if (fcinfo->flinfo == NULL || fcinfo->flinfo->fn_oid == InvalidOid) {
        PG_RETURN_NULL();
    }
    HeapTuple tuple = SearchSysCache1(PROCOID, ObjectIdGetDatum(fcinfo->flinfo->fn_oid));
    if (!HeapTupleIsValid(tuple)) {
        PG_RETURN_NULL();
    }
    Form_pg_proc form = (Form_pg_proc)GETSTRUCT(tuple);
    char buf[128];
    snprintf(
        buf,
        sizeof(buf),
        "%s:%d:%d:%u",
        form->proname,
        form->proisstrict ? 1 : 0,
        (int)form->pronargs,
        (unsigned)form->prorettype
    );
    ReleaseSysCache(tuple);
    PG_RETURN_TEXT_P(cstring_to_text(buf));
}

Datum pgrs_c_catalog_helpers(PG_FUNCTION_ARGS) {
    if (fcinfo->flinfo == NULL) {
        PG_RETURN_NULL();
    }
    int16_t text_len = 0;
    bool text_byval = true;
    char text_align = '?';
    get_typlenbyvalalign(TEXTOID, &text_len, &text_byval, &text_align);
    char *type_name = format_type_be(TEXTOID);
    char *func_name = get_func_name(fcinfo->flinfo->fn_oid);
    Oid rettype = get_func_rettype(fcinfo->flinfo->fn_oid);
    int nargs = get_func_nargs(fcinfo->flinfo->fn_oid);
    if (type_name == NULL || func_name == NULL || rettype == InvalidOid || nargs < 0) {
        PG_RETURN_NULL();
    }
    char buf[160];
    snprintf(
        buf,
        sizeof(buf),
        "%s:%d:%d:%c:%s:%u:%d",
        type_name,
        (int)text_len,
        get_typbyval(INT4OID) ? 1 : 0,
        get_typalign(INT4OID),
        func_name,
        (unsigned)rettype,
        nargs
    );
    PG_RETURN_TEXT_P(cstring_to_text(buf));
}

Datum pgrs_ext_answer_v1(PG_FUNCTION_ARGS) {
    (void)fcinfo;
    PG_RETURN_INT32(7);
}

Datum pgrs_ext_answer_v2(PG_FUNCTION_ARGS) {
    (void)fcinfo;
    PG_RETURN_INT32(8);
}
"#
}
