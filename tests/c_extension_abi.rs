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
            "CREATE FUNCTION c_spi_with_args() RETURNS text \
             AS {lib}, 'pgrs_c_spi_with_args' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_spi_plan_args() RETURNS text \
             AS {lib}, 'pgrs_c_spi_plan_args' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_spi_tupledesc() RETURNS text \
             AS {lib}, 'pgrs_c_spi_tupledesc' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_heap_getattr() RETURNS text \
             AS {lib}, 'pgrs_c_heap_getattr' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_heap_form_tuple() RETURNS text \
             AS {lib}, 'pgrs_c_heap_form_tuple' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_tupledesc_build() RETURNS text \
             AS {lib}, 'pgrs_c_tupledesc_build' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_stringinfo_utils() RETURNS text \
             AS {lib}, 'pgrs_c_stringinfo_utils' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_name_utils() RETURNS text \
             AS {lib}, 'pgrs_c_name_utils' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_bytea_echo(v bytea) RETURNS bytea \
             AS {lib}, 'pgrs_c_bytea_echo' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_varlena_aliases(v jsonb, u uuid) RETURNS text \
             AS {lib}, 'pgrs_c_varlena_aliases' LANGUAGE c"
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
            "CREATE FUNCTION c_extended_alloc() RETURNS text \
             AS {lib}, 'pgrs_c_extended_alloc' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_fmgr_invocation() RETURNS integer \
             AS {lib}, 'pgrs_c_fmgr_invocation' LANGUAGE c"
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
        &format!(
            "CREATE FUNCTION c_namespace_helpers() RETURNS text \
             AS {lib}, 'pgrs_c_namespace_helpers' LANGUAGE c"
        ),
    );
    run(
        &mut db,
        &format!(
            "CREATE FUNCTION c_syscache_helpers() RETURNS text \
             AS {lib}, 'pgrs_c_syscache_helpers' LANGUAGE c"
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
        rows(run(&mut db, "SELECT c_spi_with_args()")),
        vec![vec![Value::Text("12:arg:1".into())]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_spi_plan_args()")),
        vec![vec![Value::Text("13:plan:1".into())]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_spi_tupledesc()")),
        vec![vec![Value::Text("1:n:23:int4:4:1:i:10".into())]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_heap_getattr()")),
        vec![vec![Value::Text("32:heap:1:1".into())]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_heap_form_tuple()")),
        vec![vec![Value::Text("44:formed:1:44:formed:1".into())]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_tupledesc_build()")),
        vec![vec![Value::Text("3:num:23:label:25:77:templated:1".into())]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_stringinfo_utils()")),
        vec![vec![Value::Text(
            "hello rust!  42:done:segment:made".into()
        )]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_name_utils()")),
        vec![vec![Value::Text("alpha:beta:0:1:63".into())]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_bytea_echo('\\x616263'::bytea)")),
        vec![vec![Value::Text("\\x616263:ok".into())]]
    );
    assert_eq!(
        rows(run(
            &mut db,
            "SELECT c_varlena_aliases('{\"k\":1}'::jsonb, '11111111-2222-3333-4444-555555555555'::uuid)"
        )),
        vec![vec![Value::Text(
            "3802:2950:{\"k\": 1}:11111111-2222-3333-4444-555555555555:{\"k\": 1".into()
        )]]
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
        rows(run(&mut db, "SELECT c_extended_alloc()")),
        vec![vec![Value::Text("abc:ctx:1:1:1".into())]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_fmgr_invocation()")),
        vec![vec![Value::Int(3056)]]
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
        rows(run(&mut db, "SELECT c_namespace_helpers()")),
        vec![vec![Value::Text("pg_catalog:11:10:1:0".into())]]
    );
    assert_eq!(
        rows(run(&mut db, "SELECT c_syscache_helpers()")),
        vec![vec![Value::Text("1:0:25:23:11:11".into())]]
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
PG_FUNCTION_INFO_V1(pgrs_c_spi_with_args);
PG_FUNCTION_INFO_V1(pgrs_c_spi_plan_args);
PG_FUNCTION_INFO_V1(pgrs_c_spi_tupledesc);
PG_FUNCTION_INFO_V1(pgrs_c_heap_getattr);
PG_FUNCTION_INFO_V1(pgrs_c_heap_form_tuple);
PG_FUNCTION_INFO_V1(pgrs_c_tupledesc_build);
PG_FUNCTION_INFO_V1(pgrs_c_stringinfo_utils);
PG_FUNCTION_INFO_V1(pgrs_c_name_utils);
PG_FUNCTION_INFO_V1(pgrs_c_bytea_echo);
PG_FUNCTION_INFO_V1(pgrs_c_varlena_aliases);
PG_FUNCTION_INFO_V1(pgrs_c_fmgr_cache);
PG_FUNCTION_INFO_V1(pgrs_c_extended_alloc);
PG_FUNCTION_INFO_V1(pgrs_c_fmgr_invocation);
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
PG_FUNCTION_INFO_V1(pgrs_c_namespace_helpers);
PG_FUNCTION_INFO_V1(pgrs_c_syscache_helpers);
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

static Datum pgrs_c_collation_probe(PG_FUNCTION_ARGS) {
    if (PG_ARGISNULL(0)) {
        PG_RETURN_NULL();
    }
    PG_RETURN_INT32((int32_t)PG_GET_COLLATION() + PG_GETARG_INT32(0));
}

static Datum pgrs_c_scalar_helper_probe(PG_FUNCTION_ARGS) {
    if (PG_ARGISNULL(0) || PG_ARGISNULL(1) || PG_ARGISNULL(2)) {
        PG_RETURN_NULL();
    }
    CString label = PG_GETARG_CSTRING(0);
    char marker = PG_GETARG_CHAR(1);
    uint32_t count = PG_GETARG_UINT32(2);
    uint64_t doubled = DatumGetUInt64(UInt64GetDatum((uint64_t)count * 2));
    char *out = psprintf("%s:%c:%u:%llu", label, marker, count, (unsigned long long)doubled);
    PG_RETURN_CSTRING(out);
}

Datum pgrs_c_fmgr_invocation(PG_FUNCTION_ARGS) {
    (void)fcinfo;
    int direct = DatumGetInt32(DirectFunctionCall2(
        pgrs_c_add,
        Int32GetDatum(4),
        Int32GetDatum(6)
    ));

    FmgrInfo add_info;
    add_info.fn_addr = pgrs_c_add;
    add_info.fn_oid = InvalidOid;
    add_info.fn_nargs = 2;
    add_info.fn_strict = false;
    add_info.fn_retset = false;
    add_info.fn_stats = 0;
    add_info.fn_extra = NULL;
    add_info.fn_mcxt = NULL;
    add_info.fn_expr = NULL;

    LOCAL_FCINFO(manual_fcinfo, 2);
    InitFunctionCallInfoData(
        *manual_fcinfo,
        &add_info,
        2,
        InvalidOid,
        NULL,
        NULL
    );
    manual_fcinfo->args[0].value = Int32GetDatum(3);
    manual_fcinfo->args[0].isnull = false;
    manual_fcinfo->args[1].value = Int32GetDatum(5);
    manual_fcinfo->args[1].isnull = false;

    int invoked = DatumGetInt32(FunctionCallInvoke(manual_fcinfo));
    if (manual_fcinfo->isnull) {
        PG_RETURN_NULL();
    }

    int flinfo_call = DatumGetInt32(FunctionCall2(
        &add_info,
        Int32GetDatum(6),
        Int32GetDatum(8)
    ));

    FmgrInfo collation_info = add_info;
    collation_info.fn_addr = pgrs_c_collation_probe;
    collation_info.fn_nargs = 1;
    int flinfo_collation = DatumGetInt32(FunctionCall1Coll(
        &collation_info,
        (Oid)1000,
        Int32GetDatum(11)
    ));
    int direct_collation = DatumGetInt32(DirectFunctionCall1Coll(
        pgrs_c_collation_probe,
        (Oid)2000,
        Int32GetDatum(12)
    ));
    CString scalar_helpers = DatumGetCString(DirectFunctionCall3(
        pgrs_c_scalar_helper_probe,
        CStringGetDatum("fmgr"),
        CharGetDatum('Z'),
        UInt32GetDatum(17)
    ));
    int scalar_ok = scalar_helpers != NULL && strcmp(scalar_helpers, "fmgr:Z:17:34") == 0 ? 1 : 0;

    PG_RETURN_INT32(direct + invoked + flinfo_call + flinfo_collation + direct_collation + scalar_ok);
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

Datum pgrs_c_spi_with_args(PG_FUNCTION_ARGS) {
    if (SPI_connect() != SPI_OK_CONNECT) {
        PG_RETURN_NULL();
    }
    Oid argtypes[3] = {INT4OID, TEXTOID, BOOLOID};
    Datum values[3] = {
        Int32GetDatum(7),
        PointerGetDatum(cstring_to_text("arg")),
        BoolGetDatum(false)
    };
    const char nulls[3] = {' ', ' ', 'n'};
    int rc = SPI_execute_with_args(
        "SELECT $1 + 5, $2, $3",
        3,
        argtypes,
        values,
        nulls,
        true,
        1
    );
    if (rc != SPI_OK_SELECT || SPI_processed != 1 || SPI_tuptable == NULL) {
        PG_RETURN_NULL();
    }
    bool isnull = false;
    Datum int_datum = SPI_getbinval(SPI_tuptable->vals[0], SPI_tuptable->tupdesc, 1, &isnull);
    if (isnull) {
        PG_RETURN_NULL();
    }
    char *value = SPI_getvalue(SPI_tuptable->vals[0], SPI_tuptable->tupdesc, 2);
    if (value == NULL || strcmp(value, "arg") != 0) {
        PG_RETURN_NULL();
    }
    (void)SPI_getbinval(SPI_tuptable->vals[0], SPI_tuptable->tupdesc, 3, &isnull);
    if (!isnull) {
        PG_RETURN_NULL();
    }
    char buf[64];
    snprintf(buf, sizeof(buf), "%d:%s:%d", DatumGetInt32(int_datum), value, isnull ? 1 : 0);
    SPI_finish();
    PG_RETURN_TEXT_P(cstring_to_text(buf));
}

Datum pgrs_c_spi_plan_args(PG_FUNCTION_ARGS) {
    if (SPI_connect() != SPI_OK_CONNECT) {
        PG_RETURN_NULL();
    }
    Oid argtypes[3] = {INT4OID, TEXTOID, BOOLOID};
    SPIPlanPtr plan = SPI_prepare("SELECT $1 + 6, $2, $3", 3, argtypes);
    if (plan == NULL) {
        PG_RETURN_NULL();
    }
    Datum values[3] = {
        Int32GetDatum(7),
        PointerGetDatum(cstring_to_text("plan")),
        BoolGetDatum(false)
    };
    const char nulls[3] = {' ', ' ', 'n'};
    int rc = SPI_execute_plan(plan, values, nulls, true, 1);
    SPI_freeplan(plan);
    if (rc != SPI_OK_SELECT || SPI_processed != 1 || SPI_tuptable == NULL) {
        PG_RETURN_NULL();
    }
    bool isnull = false;
    Datum int_datum = SPI_getbinval(SPI_tuptable->vals[0], SPI_tuptable->tupdesc, 1, &isnull);
    if (isnull) {
        PG_RETURN_NULL();
    }
    char *value = SPI_getvalue(SPI_tuptable->vals[0], SPI_tuptable->tupdesc, 2);
    if (value == NULL || strcmp(value, "plan") != 0) {
        PG_RETURN_NULL();
    }
    (void)SPI_getbinval(SPI_tuptable->vals[0], SPI_tuptable->tupdesc, 3, &isnull);
    if (!isnull) {
        PG_RETURN_NULL();
    }
    char buf[64];
    snprintf(buf, sizeof(buf), "%d:%s:%d", DatumGetInt32(int_datum), value, isnull ? 1 : 0);
    SPI_finish();
    PG_RETURN_TEXT_P(cstring_to_text(buf));
}

Datum pgrs_c_spi_tupledesc(PG_FUNCTION_ARGS) {
    if (SPI_connect() != SPI_OK_CONNECT) {
        PG_RETURN_NULL();
    }
    int rc = SPI_execute("SELECT n FROM spi_numbers ORDER BY n", true, 1);
    if (rc != SPI_OK_SELECT || SPI_processed != 1 || SPI_tuptable == NULL || SPI_tuptable->tupdesc == NULL) {
        PG_RETURN_NULL();
    }
    TupleDesc tupdesc = SPI_tuptable->tupdesc;
    Form_pg_attribute attr = TupleDescAttr(tupdesc, 0);
    if (tupdesc->natts != 1 || attr == NULL) {
        PG_RETURN_NULL();
    }
    Oid typid = SPI_gettypeid(tupdesc, 1);
    char *type_name = SPI_gettype(tupdesc, 1);
    char *value = SPI_getvalue(SPI_tuptable->vals[0], tupdesc, 1);
    if (typid == InvalidOid || type_name == NULL || value == NULL) {
        PG_RETURN_NULL();
    }
    char buf[160];
    snprintf(
        buf,
        sizeof(buf),
        "%d:%s:%u:%s:%d:%d:%c:%s",
        tupdesc->natts,
        attr->attname,
        (unsigned)typid,
        type_name,
        (int)attr->attlen,
        attr->attbyval ? 1 : 0,
        attr->attalign,
        value
    );
    SPI_finish();
    PG_RETURN_TEXT_P(cstring_to_text(buf));
}

Datum pgrs_c_heap_getattr(PG_FUNCTION_ARGS) {
    if (SPI_connect() != SPI_OK_CONNECT) {
        PG_RETURN_NULL();
    }
    int rc = SPI_execute("SELECT 32, 'heap', NULL", true, 1);
    if (rc != SPI_OK_SELECT || SPI_processed != 1 || SPI_tuptable == NULL) {
        PG_RETURN_NULL();
    }
    TupleDesc tupdesc = SPI_tuptable->tupdesc;
    HeapTuple tuple = SPI_tuptable->vals[0];
    bool isnull = false;
    Datum int_datum = heap_getattr(tuple, 1, tupdesc, &isnull);
    if (isnull || DatumGetInt32(int_datum) != 32) {
        PG_RETURN_NULL();
    }
    Datum text_datum = heap_getattr(tuple, 2, tupdesc, &isnull);
    if (isnull) {
        PG_RETURN_NULL();
    }
    char *value = text_to_cstring((text *)DatumGetPointer(text_datum));
    bool null_attr = heap_attisnull(tuple, 3, tupdesc);
    bool missing_attr = heap_attisnull(tuple, 99, tupdesc);
    char buf[96];
    snprintf(
        buf,
        sizeof(buf),
        "%d:%s:%d:%d",
        DatumGetInt32(int_datum),
        value,
        null_attr ? 1 : 0,
        missing_attr ? 1 : 0
    );
    SPI_finish();
    PG_RETURN_TEXT_P(cstring_to_text(buf));
}

Datum pgrs_c_heap_form_tuple(PG_FUNCTION_ARGS) {
    if (SPI_connect() != SPI_OK_CONNECT) {
        PG_RETURN_NULL();
    }
    int rc = SPI_execute("SELECT 0 AS a, '' AS b, NULL AS c", true, 1);
    if (rc != SPI_OK_SELECT || SPI_processed != 1 || SPI_tuptable == NULL || SPI_tuptable->tupdesc == NULL) {
        PG_RETURN_NULL();
    }
    TupleDesc tupdesc = SPI_tuptable->tupdesc;
    Datum values[3] = {
        Int32GetDatum(44),
        PointerGetDatum(cstring_to_text("formed")),
        (Datum)0
    };
    bool nulls[3] = {false, false, true};
    HeapTuple tuple = heap_form_tuple(tupdesc, values, nulls);
    if (!HeapTupleIsValid(tuple)) {
        PG_RETURN_NULL();
    }
    bool isnull = false;
    Datum int_datum = heap_getattr(tuple, 1, tupdesc, &isnull);
    if (isnull || DatumGetInt32(int_datum) != 44) {
        PG_RETURN_NULL();
    }
    Datum text_datum = heap_getattr(tuple, 2, tupdesc, &isnull);
    if (isnull) {
        PG_RETURN_NULL();
    }
    char *value = text_to_cstring((text *)DatumGetPointer(text_datum));
    bool null_attr = heap_attisnull(tuple, 3, tupdesc);
    Datum out_values[3] = {0, 0, 0};
    bool out_nulls[3] = {false, false, false};
    heap_deform_tuple(tuple, tupdesc, out_values, out_nulls);
    char *deformed = text_to_cstring((text *)DatumGetPointer(out_values[1]));
    char buf[160];
    snprintf(
        buf,
        sizeof(buf),
        "%d:%s:%d:%d:%s:%d",
        DatumGetInt32(int_datum),
        value,
        null_attr ? 1 : 0,
        DatumGetInt32(out_values[0]),
        deformed,
        out_nulls[2] ? 1 : 0
    );
    heap_freetuple(tuple);
    SPI_finish();
    PG_RETURN_TEXT_P(cstring_to_text(buf));
}

Datum pgrs_c_tupledesc_build(PG_FUNCTION_ARGS) {
    TupleDesc tupdesc = CreateTemplateTupleDesc(3);
    if (tupdesc == NULL) {
        PG_RETURN_NULL();
    }
    TupleDescInitEntry(tupdesc, 1, "num", INT4OID, -1, 0);
    TupleDescInitEntry(tupdesc, 2, "label", TEXTOID, -1, 0);
    TupleDescInitEntry(tupdesc, 3, "flag", BOOLOID, -1, 0);
    TupleDesc blessed = BlessTupleDesc(tupdesc);
    TupleDesc copy = TupleDescCopy(blessed);
    if (copy == NULL || copy->natts != 3) {
        PG_RETURN_NULL();
    }
    Form_pg_attribute first = TupleDescAttr(copy, 0);
    Form_pg_attribute second = TupleDescAttr(copy, 1);
    if (first == NULL || second == NULL || strcmp(first->attname, "num") != 0 || strcmp(second->attname, "label") != 0) {
        PG_RETURN_NULL();
    }
    Datum values[3] = {
        Int32GetDatum(77),
        PointerGetDatum(cstring_to_text("templated")),
        (Datum)0
    };
    bool nulls[3] = {false, false, true};
    HeapTuple tuple = heap_form_tuple(copy, values, nulls);
    if (!HeapTupleIsValid(tuple)) {
        PG_RETURN_NULL();
    }
    Datum out_values[3] = {0, 0, 0};
    bool out_nulls[3] = {false, false, false};
    heap_deform_tuple(tuple, copy, out_values, out_nulls);
    char *label = text_to_cstring((text *)DatumGetPointer(out_values[1]));
    char buf[192];
    snprintf(
        buf,
        sizeof(buf),
        "%d:%s:%u:%s:%u:%d:%s:%d",
        copy->natts,
        first->attname,
        (unsigned)first->atttypid,
        second->attname,
        (unsigned)second->atttypid,
        DatumGetInt32(out_values[0]),
        label,
        out_nulls[2] ? 1 : 0
    );
    heap_freetuple(tuple);
    FreeTupleDesc(copy);
    FreeTupleDesc(tupdesc);
    PG_RETURN_TEXT_P(cstring_to_text(buf));
}

Datum pgrs_c_stringinfo_utils(PG_FUNCTION_ARGS) {
    StringInfoData buf;
    initStringInfo(&buf);
    appendStringInfoString(&buf, "hello");
    appendStringInfoChar(&buf, ' ');
    char *copy = pstrdup("rust");
    appendStringInfo(&buf, "%s!", copy);
    appendStringInfoSpaces(&buf, 2);
    appendStringInfo(&buf, "%d", 42);
    char *formatted = psprintf(":%s", "done");
    appendStringInfoString(&buf, formatted);
    char *segment = pnstrdup("segment-tail", 7);
    appendStringInfoChar(&buf, ':');
    appendBinaryStringInfo(&buf, segment, (int)strlen(segment));

    StringInfo made = makeStringInfo();
    if (made == NULL) {
        PG_RETURN_NULL();
    }
    appendStringInfoString(made, "reset");
    resetStringInfo(made);
    appendStringInfoString(made, "made");
    appendStringInfoChar(&buf, ':');
    appendStringInfoString(&buf, made->data);

    if (buf.len != (int)strlen(buf.data) || made->len != 4) {
        PG_RETURN_NULL();
    }
    PG_RETURN_TEXT_P(cstring_to_text(buf.data));
}

Datum pgrs_c_name_utils(PG_FUNCTION_ARGS) {
    NameData first;
    NameData second;
    NameData longname;
    namestrcpy(&first, "alpha");
    namestrncpy(&second, "beta-tail", 4);
    namestrcpy(
        &longname,
        "abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz"
    );
    int equal = namestrcmp(&first, "alpha");
    int less = namestrcmp(&first, NameStr(second)) < 0 ? 1 : 0;
    char buf[160];
    snprintf(
        buf,
        sizeof(buf),
        "%s:%s:%d:%d:%zu",
        NameStr(first),
        NameStr(second),
        equal,
        less,
        strlen(NameStr(longname))
    );
    PG_RETURN_TEXT_P(cstring_to_text(buf));
}

Datum pgrs_c_bytea_echo(PG_FUNCTION_ARGS) {
    if (PG_ARGISNULL(0)) {
        PG_RETURN_NULL();
    }
    bytea *input = PG_GETARG_BYTEA_P(0);
    size_t input_len = VARSIZE_ANY_EXHDR(input);
    size_t suffix_len = 3;
    bytea *out = (bytea *)palloc(VARHDRSZ + input_len + suffix_len);
    if (out == NULL) {
        PG_RETURN_NULL();
    }
    SET_VARSIZE(out, VARHDRSZ + input_len + suffix_len);
    memcpy(VARDATA(out), VARDATA_ANY(input), input_len);
    memcpy(VARDATA(out) + input_len, ":ok", suffix_len);

    Datum datum = ByteaPGetDatum(out);
    bytea *roundtrip = DatumGetByteaP(datum);
    if (VARSIZE_ANY_EXHDR(roundtrip) != input_len + suffix_len) {
        PG_RETURN_NULL();
    }
    PG_RETURN_BYTEA_P(roundtrip);
}

Datum pgrs_c_varlena_aliases(PG_FUNCTION_ARGS) {
    if (PG_ARGISNULL(0) || PG_ARGISNULL(1)) {
        PG_RETURN_NULL();
    }
    Jsonb *json = PG_GETARG_JSONB_P(0);
    pg_uuid_t *uuid = PG_GETARG_UUID_P(1);
    varlena *copy = PG_DETOAST_DATUM_COPY(PG_GETARG_DATUM(0));
    if (json == NULL || uuid == NULL || copy == NULL) {
        PG_RETURN_NULL();
    }

    varlena *roundtrip = DatumGetVarLenaP(VarLenaPGetDatum(copy));
    if (VARSIZE_ANY(roundtrip) != VARSIZE_ANY(json)) {
        PG_RETURN_NULL();
    }

    char json_buf[64];
    char uuid_buf[64];
    text_to_cstring_buffer((text *)json, json_buf, sizeof(json_buf));
    text_to_cstring_buffer((text *)uuid, uuid_buf, sizeof(uuid_buf));
    text *prefix = cstring_to_text_with_len(json_buf, 7);
    char *prefix_cstr = text_to_cstring(prefix);

    char buf[220];
    snprintf(
        buf,
        sizeof(buf),
        "%u:%u:%s:%s:%s",
        (unsigned)JSONBOID,
        (unsigned)UUIDOID,
        json_buf,
        uuid_buf,
        prefix_cstr
    );
    PG_FREE_IF_COPY(copy, 0);
    PG_RETURN_TEXT_P(cstring_to_text(buf));
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

Datum pgrs_c_extended_alloc(PG_FUNCTION_ARGS) {
    char *buf = (char *)palloc_extended(5, MCXT_ALLOC_ZERO | MCXT_ALLOC_NO_OOM);
    if (buf == NULL || buf[0] != '\0' || buf[4] != '\0') {
        PG_RETURN_NULL();
    }
    memcpy(buf, "abc", 4);
    buf = (char *)repalloc0(buf, 5, 9);
    if (buf == NULL || strcmp(buf, "abc") != 0 || buf[5] != '\0' || buf[8] != '\0') {
        PG_RETURN_NULL();
    }

    char *ctx = (char *)MemoryContextAllocExtended(CurrentMemoryContext, 4, MCXT_ALLOC_ZERO);
    if (ctx == NULL || ctx[0] != '\0' || ctx[3] != '\0') {
        PG_RETURN_NULL();
    }
    memcpy(ctx, "ctx", 4);

    char *huge = (char *)MemoryContextAllocExtended(CurrentMemoryContext, 2, MCXT_ALLOC_HUGE | MCXT_ALLOC_ZERO);
    if (huge == NULL || huge[0] != '\0' || huge[1] != '\0') {
        PG_RETURN_NULL();
    }
    huge[0] = 'x';

    char *zero_aligned = (char *)MemoryContextAllocZeroAligned(CurrentMemoryContext, 2);
    int aligned_zeroed = zero_aligned != NULL && zero_aligned[0] == '\0' && zero_aligned[1] == '\0';

    char *dup = MemoryContextStrdup(CurrentMemoryContext, "dup");
    int dup_ok = dup != NULL && strcmp(dup, "dup") == 0;

    char out[80];
    snprintf(
        out,
        sizeof(out),
        "%s:%s:%d:%d:%d",
        buf,
        ctx,
        huge[0] == 'x' ? 1 : 0,
        aligned_zeroed,
        dup_ok
    );
    pfree(buf);
    PG_RETURN_TEXT_P(cstring_to_text(out));
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

Datum pgrs_c_namespace_helpers(PG_FUNCTION_ARGS) {
    char *name = get_namespace_name(PG_CATALOG_NAMESPACE_OID);
    Oid nspid = get_namespace_oid("pg_catalog", false);
    Oid missing = get_namespace_oid("missing_schema", true);
    HeapTuple tuple = SearchSysCache1(NAMESPACENAME, CStringGetDatum("pg_catalog"));
    if (name == NULL || nspid == InvalidOid || !HeapTupleIsValid(tuple)) {
        PG_RETURN_NULL();
    }
    Form_pg_namespace form = (Form_pg_namespace)GETSTRUCT(tuple);
    int owner = (int)form->nspowner;
    ReleaseSysCache(tuple);
    HeapTuple by_oid = SearchSysCache1(NAMESPACEOID, ObjectIdGetDatum(PG_CATALOG_NAMESPACE_OID));
    int oid_lookup = HeapTupleIsValid(by_oid) ? 1 : 0;
    if (HeapTupleIsValid(by_oid)) {
        ReleaseSysCache(by_oid);
    }
    char buf[160];
    snprintf(
        buf,
        sizeof(buf),
        "%s:%u:%d:%d:%u",
        name,
        (unsigned)nspid,
        owner,
        oid_lookup,
        (unsigned)missing
    );
    PG_RETURN_TEXT_P(cstring_to_text(buf));
}

Datum pgrs_c_syscache_helpers(PG_FUNCTION_ARGS) {
    bool has_text = SearchSysCacheExists1(TYPEOID, ObjectIdGetDatum(TEXTOID));
    bool has_missing = SearchSysCacheExists1(TYPEOID, ObjectIdGetDatum((Oid)999999));
    Oid text_oid = GetSysCacheOid2(
        TYPENAMENSP,
        Anum_pg_type_oid,
        CStringGetDatum("text"),
        ObjectIdGetDatum(PG_CATALOG_NAMESPACE_OID)
    );
    Oid int_oid = GetSysCacheOid1(TYPEOID, Anum_pg_type_oid, ObjectIdGetDatum(INT4OID));
    Oid namespace_oid = GetSysCacheOid1(
        NAMESPACENAME,
        Anum_pg_namespace_oid,
        CStringGetDatum("pg_catalog")
    );
    HeapTuple tuple = SearchSysCache1(NAMESPACEOID, ObjectIdGetDatum(PG_CATALOG_NAMESPACE_OID));
    Oid tuple_oid = HeapTupleGetOid(tuple);
    if (HeapTupleIsValid(tuple)) {
        ReleaseSysCache(tuple);
    }
    char buf[128];
    snprintf(
        buf,
        sizeof(buf),
        "%d:%d:%u:%u:%u:%u",
        has_text ? 1 : 0,
        has_missing ? 1 : 0,
        (unsigned)text_oid,
        (unsigned)int_oid,
        (unsigned)namespace_oid,
        (unsigned)tuple_oid
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
