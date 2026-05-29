//! Minimal PostgreSQL-style native extension ABI for `LANGUAGE c` scalar
//! functions.
//!
//! The call boundary follows the shape of PostgreSQL's fmgr API: loaded symbols
//! are called as `Datum func(FunctionCallInfo fcinfo)`, with nullable `Datum`
//! arguments and `fcinfo->isnull` for NULL returns. This is not the whole
//! PostgreSQL server ABI, but it lets simple `PG_FUNCTION_ARGS`-style scalar
//! functions compile without external Rust dependencies.

use std::alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc};
use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::ptr::NonNull;

use crate::executor::eval_expr;
use crate::sql::Parser;
use crate::sql::ast::{Expr, Select, SelectItem, Statement, TableRef};
use crate::sql::serialize::statement_to_sql;
use crate::types::{DataType, Value};

pub type PgDatum = usize;
pub type NativeSpiHandler = unsafe fn(
    ctx: *mut c_void,
    query: &str,
    read_only: bool,
    count: i64,
) -> Result<NativeSpiResult, String>;

const RTLD_NOW: c_int = 0x2;
const RTLD_LOCAL: c_int = 0x0;
const MAX_FMGR_ARGS: usize = 32;
const VARLENA_HEADER_LEN: usize = 4;
const PG_MAGIC_VERSION: c_int = 160000;
pub const SPI_OK_UTILITY: c_int = 4;
pub const SPI_OK_SELECT: c_int = 5;
pub const SPI_OK_INSERT: c_int = 7;
pub const SPI_OK_DELETE: c_int = 8;
pub const SPI_OK_UPDATE: c_int = 9;
pub const SPI_OK_INSERT_RETURNING: c_int = 11;
pub const SPI_OK_DELETE_RETURNING: c_int = 12;
pub const SPI_OK_UPDATE_RETURNING: c_int = 13;
pub const SPI_OK_MERGE: c_int = 18;
const SPI_ERROR_UNSUPPORTED: c_int = -100;
const SYSCACHE_TYPEOID: c_int = 1;
const SYSCACHE_TYPENAMENSP: c_int = 2;
const SYSCACHE_PROCOID: c_int = 3;
const SYSCACHE_NAMESPACEOID: c_int = 4;
const SYSCACHE_NAMESPACENAME: c_int = 5;
const HEAP_TUPLE_KIND_SPI: c_int = 0;
const HEAP_TUPLE_KIND_PG_TYPE: c_int = 1;
const HEAP_TUPLE_KIND_PG_PROC: c_int = 2;
const HEAP_TUPLE_KIND_PG_NAMESPACE: c_int = 3;
const PG_CATALOG_NAMESPACE_OID: u32 = 11;
const DEFAULT_COLLATION_OID: u32 = 100;
const C_LANGUAGE_OID: u32 = 13;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PgNullableDatum {
    pub value: PgDatum,
    pub isnull: bool,
}

#[repr(C)]
pub struct PgFunctionCallInfoData {
    pub flinfo: *mut PgFmgrInfo,
    pub context: *mut c_void,
    pub resultinfo: *mut c_void,
    pub fncollation: u32,
    pub isnull: bool,
    pub nargs: u16,
    pub args: [PgNullableDatum; MAX_FMGR_ARGS],
}

pub type PgFmgrFn = unsafe extern "C" fn(fcinfo: *mut PgFunctionCallInfoData) -> PgDatum;
type PgModuleMagicFn = unsafe extern "C" fn() -> *const PgMagicStruct;
type PgFinfoFn = unsafe extern "C" fn() -> *const PgFinfoRecord;
type PgInitFn = unsafe extern "C" fn();

#[repr(C)]
struct PgMagicStruct {
    len: c_int,
    version: c_int,
}

#[repr(C)]
struct PgFinfoRecord {
    api_version: c_int,
}

#[repr(C)]
pub struct PgFmgrInfo {
    fn_addr: *mut c_void,
    fn_oid: u32,
    fn_nargs: i16,
    fn_strict: bool,
    fn_retset: bool,
    fn_stats: u8,
    fn_extra: *mut c_void,
    fn_mcxt: *mut c_void,
    fn_expr: *mut c_void,
}

#[repr(C)]
pub struct PgHeapTupleData {
    row_index: usize,
    data: *mut c_void,
    kind: c_int,
    natts: c_int,
    values: *mut PgDatum,
    isnull: *mut bool,
}

#[repr(C)]
pub struct PgTupleDescData {
    natts: c_int,
    attrs: *const PgAttributeFormData,
}

#[repr(C)]
pub struct PgSpiTupleTable {
    tupdesc: *const PgTupleDescData,
    vals: *const *const PgHeapTupleData,
    numvals: u64,
}

#[repr(C)]
struct PgTypeFormData {
    oid: u32,
    typname: [c_char; 64],
    typnamespace: u32,
    typlen: i16,
    typbyval: bool,
    typtype: c_char,
    typcategory: c_char,
    typcollation: u32,
    typelem: u32,
    typrelid: u32,
    typalign: c_char,
}

#[repr(C)]
#[derive(Clone)]
struct PgProcFormData {
    oid: u32,
    proname: [c_char; 64],
    pronamespace: u32,
    proowner: u32,
    prolang: u32,
    prokind: c_char,
    proisstrict: bool,
    proretset: bool,
    prorettype: u32,
    pronargs: i16,
}

#[repr(C)]
struct PgNamespaceFormData {
    oid: u32,
    nspname: [c_char; 64],
    nspowner: u32,
    nspacl: *mut c_void,
}

#[repr(C)]
#[derive(Clone)]
struct PgAttributeFormData {
    attrelid: u32,
    attname: [c_char; 64],
    atttypid: u32,
    attlen: i16,
    attnum: i16,
    attbyval: bool,
    attalign: c_char,
    attnotnull: bool,
}

type PgSpiExecuteFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    query: *const c_char,
    read_only: bool,
    count: i64,
) -> c_int;
type PgSpiExecuteWithArgsFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    query: *const c_char,
    nargs: c_int,
    argtypes: *const u32,
    values: *const PgDatum,
    nulls: *const c_char,
    read_only: bool,
    count: i64,
) -> c_int;
type PgSpiGetValueFn =
    unsafe extern "C" fn(ctx: *mut c_void, row_index: usize, column_index: c_int) -> *const c_char;
type PgSpiGetBinValFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    row_index: usize,
    column_index: c_int,
    isnull: *mut bool,
) -> PgDatum;
type PgAllocFn = unsafe extern "C" fn(ctx: *mut c_void, size: usize, zero: bool) -> *mut c_void;
type PgReallocFn =
    unsafe extern "C" fn(ctx: *mut c_void, ptr: *mut c_void, size: usize) -> *mut c_void;
type PgFreeFn = unsafe extern "C" fn(ctx: *mut c_void, ptr: *mut c_void);
type PgReportErrorFn =
    unsafe extern "C" fn(ctx: *mut c_void, elevel: c_int, message: *const c_char);
type PgGetConfigOptionFn =
    unsafe extern "C" fn(ctx: *mut c_void, name: *const c_char, missing_ok: bool) -> *const c_char;
type PgSearchSysCache1Fn =
    unsafe extern "C" fn(ctx: *mut c_void, cache_id: c_int, key: PgDatum) -> *mut PgHeapTupleData;
type PgSearchSysCache2Fn = unsafe extern "C" fn(
    ctx: *mut c_void,
    cache_id: c_int,
    key1: PgDatum,
    key2: PgDatum,
) -> *mut PgHeapTupleData;
type PgReleaseSysCacheFn = unsafe extern "C" fn(ctx: *mut c_void, tuple: *mut PgHeapTupleData);
type PgMemoryContextAllocFn =
    unsafe extern "C" fn(context: *mut PgMemoryContextData, size: usize, zero: bool) -> *mut c_void;
type PgMemoryContextReallocFn = unsafe extern "C" fn(
    context: *mut PgMemoryContextData,
    ptr: *mut c_void,
    size: usize,
) -> *mut c_void;
type PgMemoryContextFreeFn =
    unsafe extern "C" fn(context: *mut PgMemoryContextData, ptr: *mut c_void);

#[repr(C)]
pub struct PgMemoryContextData {
    state: *mut c_void,
    alloc: Option<PgMemoryContextAllocFn>,
    realloc: Option<PgMemoryContextReallocFn>,
    free: Option<PgMemoryContextFreeFn>,
}

#[repr(C)]
pub struct PgExtensionContext {
    spi_state: *mut c_void,
    memory_state: *mut c_void,
    current_memory_context: *mut PgMemoryContextData,
    spi_execute: Option<PgSpiExecuteFn>,
    spi_execute_with_args: Option<PgSpiExecuteWithArgsFn>,
    spi_getvalue: Option<PgSpiGetValueFn>,
    spi_getbinval: Option<PgSpiGetBinValFn>,
    memory_alloc: Option<PgAllocFn>,
    memory_realloc: Option<PgReallocFn>,
    memory_free: Option<PgFreeFn>,
    report_error: Option<PgReportErrorFn>,
    get_config_option: Option<PgGetConfigOptionFn>,
    search_syscache1: Option<PgSearchSysCache1Fn>,
    search_syscache2: Option<PgSearchSysCache2Fn>,
    release_syscache: Option<PgReleaseSysCacheFn>,
    spi_processed: usize,
    spi_tuptable: *mut PgSpiTupleTable,
    spi_result: c_int,
}

#[derive(Clone)]
pub struct NativeUdf {
    pub name: String,
    pub arg_types: Vec<DataType>,
    pub return_type: Option<DataType>,
    pub library_path: String,
    pub symbol: String,
    pub oid: u32,
    pub strict: bool,
}

pub struct NativeSpiResult {
    pub code: c_int,
    pub processed: usize,
    pub fields: Vec<NativeSpiField>,
    pub rows: Vec<Vec<Value>>,
    pub has_tuptable: bool,
}

pub struct NativeSpiField {
    pub name: String,
    pub data_type: DataType,
}

impl NativeSpiResult {
    pub fn select(rows: Vec<Vec<Value>>) -> Self {
        Self {
            code: SPI_OK_SELECT,
            processed: rows.len(),
            fields: Vec::new(),
            rows,
            has_tuptable: true,
        }
    }

    pub fn command(code: c_int, processed: usize) -> Self {
        Self {
            code,
            processed,
            fields: Vec::new(),
            rows: Vec::new(),
            has_tuptable: false,
        }
    }

    pub fn returning(code: c_int, fields: Vec<NativeSpiField>, rows: Vec<Vec<Value>>) -> Self {
        Self {
            code,
            processed: rows.len(),
            fields,
            rows,
            has_tuptable: true,
        }
    }
}

thread_local! {
    static LIBRARIES: RefCell<HashMap<String, NativeLibrary>> = RefCell::new(HashMap::new());
    static FMGR_INFOS: RefCell<HashMap<NativeFmgrKey, Box<NativeFmgrState>>> = RefCell::new(HashMap::new());
    static SPI_HANDLER: RefCell<Option<NativeSpiHandlerSlot>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct NativeSpiState {
    rows: Vec<Vec<Option<CString>>>,
    datum_rows: Vec<Vec<PgNullableDatum>>,
    datum_storage: Vec<Vec<u8>>,
    heap_rows: Vec<Box<PgHeapTupleData>>,
    row_ptrs: Vec<*const PgHeapTupleData>,
    tuple_attrs: Vec<PgAttributeFormData>,
    tuple_desc: Option<Box<PgTupleDescData>>,
    tuple_table: Option<Box<PgSpiTupleTable>>,
    config_values: Vec<CString>,
    syscache_tuples: Vec<NonNull<PgHeapTupleData>>,
    current_proc: Option<PgProcFormData>,
    error: Option<String>,
}

#[derive(Clone, Copy)]
struct NativeSpiHandlerSlot {
    ctx: *mut c_void,
    handler: NativeSpiHandler,
}

#[derive(Hash, PartialEq, Eq)]
struct NativeFmgrKey {
    library_path: String,
    symbol: String,
    oid: u32,
    nargs: usize,
}

#[derive(Clone, Copy)]
struct NativeLibrary {
    handle: *mut c_void,
}

struct NativeFmgrState {
    info: PgFmgrInfo,
    _memory_state: Box<NativeMemoryState>,
    _memory_context: Box<PgMemoryContextData>,
}

#[derive(Default)]
struct NativeMemoryState {
    allocations: HashMap<NonNull<c_void>, Layout>,
}

#[cfg_attr(any(target_os = "linux", target_os = "android"), link(name = "dl"))]
unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *const c_char;
}

pub fn with_spi_handler<T>(
    ctx: *mut c_void,
    handler: NativeSpiHandler,
    f: impl FnOnce() -> T,
) -> T {
    SPI_HANDLER.with(|cell| {
        let previous = cell.replace(Some(NativeSpiHandlerSlot { ctx, handler }));
        let result = f();
        cell.replace(previous);
        result
    })
}

pub fn eval_native_udf(udf: &NativeUdf, vals: &[Value]) -> Result<Value, String> {
    if vals.len() != udf.arg_types.len() {
        return Err(format!(
            "native function {} expected {} arguments, got {}",
            udf.symbol,
            udf.arg_types.len(),
            vals.len()
        ));
    }
    if vals.len() > MAX_FMGR_ARGS {
        return Err(format!(
            "native function {} has {} arguments; max supported is {}",
            udf.symbol,
            vals.len(),
            MAX_FMGR_ARGS
        ));
    }

    let symbol = load_symbol(&udf.library_path, &udf.symbol)?;
    let mut text_storage: Vec<Vec<u8>> = Vec::new();
    let mut spi_state = NativeSpiState::default();
    spi_state.current_proc = Some(pg_proc_form_for_udf(udf));
    let mut memory_state = NativeMemoryState::default();
    let mut current_memory_context = PgMemoryContextData {
        state: &mut memory_state as *mut NativeMemoryState as *mut c_void,
        alloc: Some(memory_context_alloc_callback),
        realloc: Some(memory_context_realloc_callback),
        free: Some(memory_context_free_callback),
    };
    let mut ext_context = PgExtensionContext {
        spi_state: &mut spi_state as *mut NativeSpiState as *mut c_void,
        memory_state: &mut memory_state as *mut NativeMemoryState as *mut c_void,
        current_memory_context: &mut current_memory_context as *mut PgMemoryContextData,
        spi_execute: Some(spi_execute_callback),
        spi_execute_with_args: Some(spi_execute_with_args_callback),
        spi_getvalue: Some(spi_getvalue_callback),
        spi_getbinval: Some(spi_getbinval_callback),
        memory_alloc: Some(memory_alloc_callback),
        memory_realloc: Some(memory_realloc_callback),
        memory_free: Some(memory_free_callback),
        report_error: Some(report_error_callback),
        get_config_option: Some(get_config_option_callback),
        search_syscache1: Some(search_syscache1_callback),
        search_syscache2: Some(search_syscache2_callback),
        release_syscache: Some(release_syscache_callback),
        spi_processed: 0,
        spi_tuptable: std::ptr::null_mut(),
        spi_result: 0,
    };
    let flinfo = fmgr_info_for_udf(udf, symbol);
    let mut fcinfo = PgFunctionCallInfoData {
        flinfo,
        context: &mut ext_context as *mut PgExtensionContext as *mut c_void,
        resultinfo: std::ptr::null_mut(),
        fncollation: 0,
        isnull: false,
        nargs: vals.len() as u16,
        args: [PgNullableDatum::default(); MAX_FMGR_ARGS],
    };

    for (idx, value) in vals.iter().enumerate() {
        fcinfo.args[idx] = value_to_datum(value, &mut text_storage);
    }

    let datum = unsafe { symbol(&mut fcinfo) };
    if let Some(error) = spi_state.error.take() {
        memory_state.free_all_except_fmgr_extra();
        return Err(error);
    }
    if fcinfo.isnull {
        memory_state.free_all_except_fmgr_extra();
        return Ok(Value::Null);
    }
    let value = datum_to_value(datum, udf.return_type);
    memory_state.free_all_except_fmgr_extra();
    value
}

fn fmgr_info_for_udf(udf: &NativeUdf, symbol: PgFmgrFn) -> *mut PgFmgrInfo {
    let key = NativeFmgrKey {
        library_path: udf.library_path.clone(),
        symbol: udf.symbol.clone(),
        oid: udf.oid,
        nargs: udf.arg_types.len(),
    };
    FMGR_INFOS.with(|cell| {
        let mut infos = cell.borrow_mut();
        let state = infos.entry(key).or_insert_with(|| {
            let mut memory_state = Box::new(NativeMemoryState::default());
            let mut memory_context = Box::new(PgMemoryContextData {
                state: memory_state.as_mut() as *mut NativeMemoryState as *mut c_void,
                alloc: Some(memory_context_alloc_callback),
                realloc: Some(memory_context_realloc_callback),
                free: Some(memory_context_free_callback),
            });
            let info = PgFmgrInfo {
                fn_addr: symbol as *mut c_void,
                fn_oid: udf.oid,
                fn_nargs: udf.arg_types.len() as i16,
                fn_strict: udf.strict,
                fn_retset: false,
                fn_stats: 0,
                fn_extra: std::ptr::null_mut(),
                fn_mcxt: memory_context.as_mut() as *mut PgMemoryContextData as *mut c_void,
                fn_expr: std::ptr::null_mut(),
            };
            Box::new(NativeFmgrState {
                info,
                _memory_state: memory_state,
                _memory_context: memory_context,
            })
        });
        &mut state.info as *mut PgFmgrInfo
    })
}

unsafe extern "C" fn spi_execute_callback(
    ctx: *mut c_void,
    query: *const c_char,
    _read_only: bool,
    count: i64,
) -> c_int {
    let Some((context, state)) = spi_context_and_state(ctx) else {
        return SPI_ERROR_UNSUPPORTED;
    };
    if query.is_null() {
        state.error = Some("SPI_execute received a null query".into());
        return SPI_ERROR_UNSUPPORTED;
    }
    let query = unsafe { CStr::from_ptr(query) }
        .to_string_lossy()
        .into_owned();
    match execute_spi_query(&query, _read_only, count) {
        Ok(result) => apply_spi_result(context, state, result),
        Err(e) => {
            state.error = Some(e);
            SPI_ERROR_UNSUPPORTED
        }
    }
}

unsafe extern "C" fn spi_execute_with_args_callback(
    ctx: *mut c_void,
    query: *const c_char,
    nargs: c_int,
    argtypes: *const u32,
    values: *const PgDatum,
    nulls: *const c_char,
    read_only: bool,
    count: i64,
) -> c_int {
    let Some((context, state)) = spi_context_and_state(ctx) else {
        return SPI_ERROR_UNSUPPORTED;
    };
    if query.is_null() {
        state.error = Some("SPI_execute_with_args received a null query".into());
        return SPI_ERROR_UNSUPPORTED;
    }
    let query = unsafe { CStr::from_ptr(query) }
        .to_string_lossy()
        .into_owned();
    let params = match unsafe { spi_params_from_datums(nargs, argtypes, values, nulls) } {
        Ok(params) => params,
        Err(e) => {
            state.error = Some(e);
            return SPI_ERROR_UNSUPPORTED;
        }
    };
    match execute_spi_query_with_params(&query, &params, read_only, count) {
        Ok(result) => apply_spi_result(context, state, result),
        Err(e) => {
            state.error = Some(e);
            SPI_ERROR_UNSUPPORTED
        }
    }
}

fn apply_spi_result(
    context: &mut PgExtensionContext,
    state: &mut NativeSpiState,
    result: NativeSpiResult,
) -> c_int {
    context.spi_result = result.code;
    context.spi_processed = result.processed;
    if result.has_tuptable {
        state.set_rows(result.rows, result.fields);
        context.spi_tuptable = state
            .tuple_table
            .as_deref_mut()
            .map(|table| table as *mut PgSpiTupleTable)
            .unwrap_or(std::ptr::null_mut());
    } else {
        state.clear_rows();
        context.spi_tuptable = std::ptr::null_mut();
    }
    result.code
}

unsafe extern "C" fn memory_alloc_callback(
    ctx: *mut c_void,
    size: usize,
    zero: bool,
) -> *mut c_void {
    let Some(state) = native_memory_state(ctx) else {
        return std::ptr::null_mut();
    };
    state.alloc(size, zero)
}

unsafe extern "C" fn memory_realloc_callback(
    ctx: *mut c_void,
    ptr: *mut c_void,
    size: usize,
) -> *mut c_void {
    let Some(state) = native_memory_state(ctx) else {
        return std::ptr::null_mut();
    };
    state.realloc(ptr, size)
}

unsafe extern "C" fn memory_free_callback(ctx: *mut c_void, ptr: *mut c_void) {
    let Some(state) = native_memory_state(ctx) else {
        return;
    };
    state.free(ptr);
}

unsafe extern "C" fn report_error_callback(
    ctx: *mut c_void,
    elevel: c_int,
    message: *const c_char,
) {
    const ERROR: c_int = 20;
    if elevel < ERROR {
        return;
    }
    let Some((_, state)) = spi_context_and_state(ctx) else {
        return;
    };
    let message = if message.is_null() {
        "native extension raised an error".into()
    } else {
        unsafe { CStr::from_ptr(message) }
            .to_string_lossy()
            .into_owned()
    };
    state.error = Some(message);
}

unsafe extern "C" fn get_config_option_callback(
    ctx: *mut c_void,
    name: *const c_char,
    missing_ok: bool,
) -> *const c_char {
    let Some((_, state)) = spi_context_and_state(ctx) else {
        return std::ptr::null();
    };
    if name.is_null() {
        state.error = Some("GetConfigOptionByName received a null name".into());
        return std::ptr::null();
    }
    let name = unsafe { CStr::from_ptr(name) }
        .to_string_lossy()
        .into_owned();
    match execute_spi_query(
        &format!(
            "SELECT current_setting({}, true)",
            sql_string_literal(&name)
        ),
        true,
        1,
    ) {
        Ok(result) => {
            let value = result
                .rows
                .first()
                .and_then(|row| row.first())
                .and_then(Value::to_text);
            if let Some(value) = value {
                match CString::new(value) {
                    Ok(value) => {
                        state.config_values.push(value);
                        state
                            .config_values
                            .last()
                            .map(|value| value.as_ptr())
                            .unwrap_or(std::ptr::null())
                    }
                    Err(_) => {
                        state.error = Some(format!(
                            "configuration parameter {name:?} contains an interior NUL"
                        ));
                        std::ptr::null()
                    }
                }
            } else {
                if !missing_ok {
                    state.error = Some(format!("unrecognized configuration parameter \"{name}\""));
                }
                std::ptr::null()
            }
        }
        Err(e) => {
            state.error = Some(e);
            std::ptr::null()
        }
    }
}

unsafe extern "C" fn search_syscache1_callback(
    ctx: *mut c_void,
    cache_id: c_int,
    key: PgDatum,
) -> *mut PgHeapTupleData {
    let Some((_, state)) = spi_context_and_state(ctx) else {
        return std::ptr::null_mut();
    };
    let Some((data, kind)) = lookup_syscache_form(state, cache_id, key) else {
        return std::ptr::null_mut();
    };
    syscache_tuple_from_form(state, data, kind)
}

unsafe extern "C" fn search_syscache2_callback(
    ctx: *mut c_void,
    cache_id: c_int,
    key1: PgDatum,
    key2: PgDatum,
) -> *mut PgHeapTupleData {
    let Some((_, state)) = spi_context_and_state(ctx) else {
        return std::ptr::null_mut();
    };
    let Some((data, kind)) = lookup_syscache_form2(state, cache_id, key1, key2) else {
        return std::ptr::null_mut();
    };
    syscache_tuple_from_form(state, data, kind)
}

fn syscache_tuple_from_form(
    state: &mut NativeSpiState,
    data: *mut c_void,
    kind: c_int,
) -> *mut PgHeapTupleData {
    let tuple = Box::into_raw(Box::new(PgHeapTupleData {
        row_index: 0,
        data,
        kind,
        natts: 0,
        values: std::ptr::null_mut(),
        isnull: std::ptr::null_mut(),
    }));
    let Some(non_null) = NonNull::new(tuple) else {
        unsafe {
            free_syscache_form(data, kind);
        }
        return std::ptr::null_mut();
    };
    state.syscache_tuples.push(non_null);
    tuple
}

unsafe extern "C" fn release_syscache_callback(ctx: *mut c_void, tuple: *mut PgHeapTupleData) {
    let Some((_, state)) = spi_context_and_state(ctx) else {
        return;
    };
    release_syscache_tuple(state, tuple);
}

unsafe extern "C" fn memory_context_alloc_callback(
    context: *mut PgMemoryContextData,
    size: usize,
    zero: bool,
) -> *mut c_void {
    let Some(state) = memory_context_state(context) else {
        return std::ptr::null_mut();
    };
    state.alloc(size, zero)
}

unsafe extern "C" fn memory_context_realloc_callback(
    context: *mut PgMemoryContextData,
    ptr: *mut c_void,
    size: usize,
) -> *mut c_void {
    let Some(state) = memory_context_state(context) else {
        return std::ptr::null_mut();
    };
    state.realloc(ptr, size)
}

unsafe extern "C" fn memory_context_free_callback(
    context: *mut PgMemoryContextData,
    ptr: *mut c_void,
) {
    let Some(state) = memory_context_state(context) else {
        return;
    };
    state.free(ptr);
}

fn memory_context_state<'a>(
    context: *mut PgMemoryContextData,
) -> Option<&'a mut NativeMemoryState> {
    if context.is_null() {
        return None;
    }
    let context = unsafe { &mut *context };
    if context.state.is_null() {
        return None;
    }
    Some(unsafe { &mut *(context.state as *mut NativeMemoryState) })
}

fn native_memory_state<'a>(ctx: *mut c_void) -> Option<&'a mut NativeMemoryState> {
    if ctx.is_null() {
        return None;
    }
    let context = unsafe { &mut *(ctx as *mut PgExtensionContext) };
    if context.memory_state.is_null() {
        return None;
    }
    Some(unsafe { &mut *(context.memory_state as *mut NativeMemoryState) })
}

unsafe extern "C" fn spi_getvalue_callback(
    ctx: *mut c_void,
    row_index: usize,
    column_index: c_int,
) -> *const c_char {
    let Some((_, state)) = spi_context_and_state(ctx) else {
        return std::ptr::null();
    };
    if column_index < 0 {
        return std::ptr::null();
    }
    state
        .rows
        .get(row_index)
        .and_then(|row| row.get(column_index as usize))
        .and_then(|value| value.as_ref())
        .map(|value| value.as_ptr())
        .unwrap_or(std::ptr::null())
}

unsafe extern "C" fn spi_getbinval_callback(
    ctx: *mut c_void,
    row_index: usize,
    column_index: c_int,
    isnull: *mut bool,
) -> PgDatum {
    let Some((_, state)) = spi_context_and_state(ctx) else {
        set_spi_binval_null(isnull);
        return 0;
    };
    if column_index < 0 {
        set_spi_binval_null(isnull);
        return 0;
    }
    let Some(datum) = state
        .datum_rows
        .get(row_index)
        .and_then(|row| row.get(column_index as usize))
        .copied()
    else {
        set_spi_binval_null(isnull);
        return 0;
    };
    if !isnull.is_null() {
        unsafe {
            *isnull = datum.isnull;
        }
    }
    datum.value
}

fn set_spi_binval_null(isnull: *mut bool) {
    if !isnull.is_null() {
        unsafe {
            *isnull = true;
        }
    }
}

fn spi_context_and_state<'a>(
    ctx: *mut c_void,
) -> Option<(&'a mut PgExtensionContext, &'a mut NativeSpiState)> {
    if ctx.is_null() {
        return None;
    }
    let context = unsafe { &mut *(ctx as *mut PgExtensionContext) };
    if context.spi_state.is_null() {
        return None;
    }
    let state = unsafe { &mut *(context.spi_state as *mut NativeSpiState) };
    Some((context, state))
}

fn execute_spi_query(query: &str, read_only: bool, count: i64) -> Result<NativeSpiResult, String> {
    if let Some(result) = SPI_HANDLER.with(|cell| {
        let slot = cell.borrow().as_ref().copied()?;
        Some(unsafe { (slot.handler)(slot.ctx, query, read_only, count) })
    }) {
        return result;
    }
    spi_execute_select(query, count).map(NativeSpiResult::select)
}

fn execute_spi_query_with_params(
    query: &str,
    params: &[Value],
    read_only: bool,
    count: i64,
) -> Result<NativeSpiResult, String> {
    let mut stmts: Vec<Statement> = Parser::parse_sql(query)?
        .into_iter()
        .filter(|stmt| !matches!(stmt, Statement::Empty))
        .collect();
    if stmts.len() != 1 {
        return Err("SPI_execute_with_args supports one statement".into());
    }
    let Some(mut stmt) = stmts.pop() else {
        return Err("SPI_execute_with_args requires a statement".into());
    };
    bind_spi_statement(&mut stmt, params)?;
    execute_spi_query(&statement_to_sql(&stmt), read_only, count)
}

fn spi_execute_select(query: &str, count: i64) -> Result<Vec<Vec<Value>>, String> {
    let mut stmts: Vec<Statement> = Parser::parse_sql(query)?
        .into_iter()
        .filter(|stmt| !matches!(stmt, Statement::Empty))
        .collect();
    if stmts.len() != 1 {
        return Err("SPI_execute supports one SELECT statement".into());
    }
    let Some(stmt) = stmts.pop() else {
        return Err("SPI_execute requires a SELECT statement".into());
    };
    let Statement::Select(select) = stmt else {
        return Err("SPI_execute currently supports SELECT only".into());
    };
    if select.from.is_some()
        || !select.ctes.is_empty()
        || select.filter.is_some()
        || !select.group_by.is_empty()
        || !select.grouping_sets.is_empty()
        || select.having.is_some()
        || !select.set_ops.is_empty()
    {
        return Err("SPI_execute currently supports scalar SELECT without FROM".into());
    }
    if count == 0 {
        return Ok(vec![eval_spi_projection(&select.projection)?]);
    }
    if count > 0 {
        return Ok(vec![eval_spi_projection(&select.projection)?]
            .into_iter()
            .take(count as usize)
            .collect());
    }
    Ok(vec![eval_spi_projection(&select.projection)?])
}

fn eval_spi_projection(items: &[SelectItem]) -> Result<Vec<Value>, String> {
    let mut row = Vec::new();
    for item in items {
        match item {
            SelectItem::Expr { expr, .. } => row.push(eval_expr(expr, &[], &[])?),
            SelectItem::Wildcard => return Err("SPI scalar SELECT cannot project *".into()),
        }
    }
    Ok(row)
}

fn sql_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

unsafe fn spi_params_from_datums(
    nargs: c_int,
    argtypes: *const u32,
    values: *const PgDatum,
    nulls: *const c_char,
) -> Result<Vec<Value>, String> {
    if nargs < 0 {
        return Err("SPI_execute_with_args received a negative nargs".into());
    }
    if nargs > 0 && values.is_null() {
        return Err("SPI_execute_with_args received null values for nonzero nargs".into());
    }
    let mut params = Vec::with_capacity(nargs as usize);
    for idx in 0..nargs as usize {
        let isnull = if nulls.is_null() {
            false
        } else {
            unsafe { *nulls.add(idx) == b'n' as c_char }
        };
        if isnull {
            params.push(Value::Null);
            continue;
        }
        let oid = if argtypes.is_null() {
            0
        } else {
            unsafe { *argtypes.add(idx) as i32 }
        };
        let datum = unsafe { *values.add(idx) };
        params.push(spi_datum_to_value(datum, oid)?);
    }
    Ok(params)
}

fn spi_datum_to_value(datum: PgDatum, oid: i32) -> Result<Value, String> {
    match oid {
        0 | 25 => datum_text(datum).map(Value::Text),
        16 => Ok(Value::Bool(datum != 0)),
        20 => Ok(Value::Int(datum as i64)),
        21 => Ok(Value::Int((datum as i16) as i64)),
        23 => Ok(Value::Int((datum as i32) as i64)),
        700 => Ok(Value::Float(f32::from_bits(datum as u32) as f64)),
        701 => Ok(Value::Float(f64::from_bits(datum as u64))),
        _ => {
            let Some(dt) = data_type_for_oid(oid) else {
                return datum_text(datum).map(Value::Text);
            };
            datum_to_value(datum, Some(dt))
        }
    }
}

fn data_type_for_oid(oid: i32) -> Option<DataType> {
    DataType::ALL.iter().copied().find(|dt| dt.oid() == oid)
}

fn bind_spi_statement(stmt: &mut Statement, params: &[Value]) -> Result<(), String> {
    match stmt {
        Statement::Insert(i) => {
            for tuple in &mut i.rows {
                for expr in tuple {
                    bind_spi_expr(expr, params)?;
                }
            }
            if let Some(select) = &mut i.select {
                bind_spi_select(select, params)?;
            }
        }
        Statement::Select(select) => bind_spi_select(select, params)?,
        Statement::Update(update) => {
            for (_, expr) in &mut update.assignments {
                bind_spi_expr(expr, params)?;
            }
            if let Some(filter) = &mut update.filter {
                bind_spi_expr(filter, params)?;
            }
        }
        Statement::Delete(delete) => {
            if let Some(filter) = &mut delete.filter {
                bind_spi_expr(filter, params)?;
            }
        }
        Statement::Explain(explain) => bind_spi_statement(&mut explain.statement, params)?,
        _ => {}
    }
    Ok(())
}

fn bind_spi_select(select: &mut Select, params: &[Value]) -> Result<(), String> {
    for item in &mut select.projection {
        if let SelectItem::Expr { expr, .. } = item {
            bind_spi_expr(expr, params)?;
        }
    }
    if let Some(filter) = &mut select.filter {
        bind_spi_expr(filter, params)?;
    }
    if let Some(from) = &mut select.from {
        bind_spi_table_ref(&mut from.base, params)?;
        for join in &mut from.joins {
            bind_spi_table_ref(&mut join.table, params)?;
            if let Some(on) = &mut join.on {
                bind_spi_expr(on, params)?;
            }
        }
    }
    for group in &mut select.group_by {
        bind_spi_expr(group, params)?;
    }
    if let Some(having) = &mut select.having {
        bind_spi_expr(having, params)?;
    }
    for order in &mut select.order_by {
        bind_spi_expr(&mut order.expr, params)?;
    }
    if let Some(limit) = &mut select.limit {
        bind_spi_expr(limit, params)?;
    }
    if let Some(offset) = &mut select.offset {
        bind_spi_expr(offset, params)?;
    }
    for set_op in &mut select.set_ops {
        bind_spi_select(&mut set_op.select, params)?;
    }
    Ok(())
}

fn bind_spi_table_ref(table: &mut TableRef, params: &[Value]) -> Result<(), String> {
    for arg in &mut table.args {
        bind_spi_expr(arg, params)?;
    }
    if let Some(subquery) = &mut table.subquery {
        bind_spi_select(subquery, params)?;
    }
    Ok(())
}

fn bind_spi_expr(expr: &mut Expr, params: &[Value]) -> Result<(), String> {
    match expr {
        Expr::Param(n) => {
            let idx = (*n as usize)
                .checked_sub(1)
                .ok_or_else(|| "parameter $0 is invalid".to_string())?;
            let value = params.get(idx).ok_or_else(|| {
                format!("SPI_execute_with_args supplies too few parameters for ${n}")
            })?;
            *expr = value_to_expr(value);
        }
        Expr::Unary { expr, .. } => bind_spi_expr(expr, params)?,
        Expr::Binary { left, right, .. } => {
            bind_spi_expr(left, params)?;
            bind_spi_expr(right, params)?;
        }
        Expr::QuantifiedCompare { left, list, .. } => {
            bind_spi_expr(left, params)?;
            for item in list {
                bind_spi_expr(item, params)?;
            }
        }
        Expr::Row(items) | Expr::Array(items) => {
            for item in items {
                bind_spi_expr(item, params)?;
            }
        }
        Expr::IsNull { expr, .. } => bind_spi_expr(expr, params)?,
        Expr::IsDistinctFrom { left, right, .. } => {
            bind_spi_expr(left, params)?;
            bind_spi_expr(right, params)?;
        }
        Expr::Like { expr, pattern, .. } => {
            bind_spi_expr(expr, params)?;
            bind_spi_expr(pattern, params)?;
        }
        Expr::InList { expr, list, .. } => {
            bind_spi_expr(expr, params)?;
            for item in list {
                bind_spi_expr(item, params)?;
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            bind_spi_expr(expr, params)?;
            bind_spi_expr(low, params)?;
            bind_spi_expr(high, params)?;
        }
        Expr::Case {
            operand,
            whens,
            else_expr,
        } => {
            if let Some(operand) = operand {
                bind_spi_expr(operand, params)?;
            }
            for (condition, result) in whens {
                bind_spi_expr(condition, params)?;
                bind_spi_expr(result, params)?;
            }
            if let Some(else_expr) = else_expr {
                bind_spi_expr(else_expr, params)?;
            }
        }
        Expr::Cast { expr, .. } => bind_spi_expr(expr, params)?,
        Expr::InSubquery { expr, .. } => bind_spi_expr(expr, params)?,
        Expr::Function { args, filter, .. } => {
            for arg in args {
                bind_spi_expr(arg, params)?;
            }
            if let Some(filter) = filter {
                bind_spi_expr(filter, params)?;
            }
        }
        Expr::ScalarSubquery(_) | Expr::Exists(_) => {}
        _ => {}
    }
    Ok(())
}

fn value_to_expr(value: &Value) -> Expr {
    match value {
        Value::Null => Expr::Null,
        Value::Int(i) => Expr::Int(*i),
        Value::Float(f) => Expr::Float(*f),
        Value::Numeric(n) => Expr::Cast {
            expr: Box::new(Expr::Str(n.to_canonical_string())),
            target: DataType::Numeric,
        },
        Value::Text(s) => Expr::Str(s.clone()),
        Value::Bool(b) => Expr::Bool(*b),
    }
}

fn lookup_syscache_form(
    state: &NativeSpiState,
    cache_id: c_int,
    key: PgDatum,
) -> Option<(*mut c_void, c_int)> {
    if let Some(form) = lookup_type_form(cache_id, key) {
        return Some((
            Box::into_raw(Box::new(form)) as *mut c_void,
            HEAP_TUPLE_KIND_PG_TYPE,
        ));
    }
    if let Some(form) = lookup_proc_form(state, cache_id, key) {
        return Some((
            Box::into_raw(Box::new(form)) as *mut c_void,
            HEAP_TUPLE_KIND_PG_PROC,
        ));
    }
    if let Some(form) = lookup_namespace_form(cache_id, key) {
        return Some((
            Box::into_raw(Box::new(form)) as *mut c_void,
            HEAP_TUPLE_KIND_PG_NAMESPACE,
        ));
    }
    None
}

fn lookup_syscache_form2(
    _state: &NativeSpiState,
    cache_id: c_int,
    key1: PgDatum,
    key2: PgDatum,
) -> Option<(*mut c_void, c_int)> {
    if let Some(form) = lookup_type_form2(cache_id, key1, key2) {
        return Some((
            Box::into_raw(Box::new(form)) as *mut c_void,
            HEAP_TUPLE_KIND_PG_TYPE,
        ));
    }
    None
}

fn lookup_type_form(cache_id: c_int, key: PgDatum) -> Option<PgTypeFormData> {
    match cache_id {
        SYSCACHE_TYPEOID => {
            let oid = key as i32;
            DataType::ALL
                .iter()
                .copied()
                .find(|dt| dt.oid() == oid)
                .map(pg_type_form)
        }
        SYSCACHE_TYPENAMENSP => {
            if key == 0 {
                return None;
            }
            let name = unsafe { CStr::from_ptr(key as *const c_char) }
                .to_string_lossy()
                .into_owned();
            DataType::ALL
                .iter()
                .copied()
                .find(|dt| dt.pg_type_name() == name)
                .map(pg_type_form)
        }
        _ => None,
    }
}

fn lookup_type_form2(cache_id: c_int, key1: PgDatum, key2: PgDatum) -> Option<PgTypeFormData> {
    match cache_id {
        SYSCACHE_TYPENAMENSP => {
            if key1 == 0 || key2 as u32 != PG_CATALOG_NAMESPACE_OID {
                return None;
            }
            let name = unsafe { CStr::from_ptr(key1 as *const c_char) }
                .to_string_lossy()
                .into_owned();
            DataType::ALL
                .iter()
                .copied()
                .find(|dt| dt.pg_type_name() == name)
                .map(pg_type_form)
        }
        _ => None,
    }
}

fn lookup_proc_form(
    state: &NativeSpiState,
    cache_id: c_int,
    key: PgDatum,
) -> Option<PgProcFormData> {
    match cache_id {
        SYSCACHE_PROCOID => {
            let oid = key as u32;
            state
                .current_proc
                .as_ref()
                .filter(|proc| proc.oid == oid)
                .cloned()
        }
        _ => None,
    }
}

fn lookup_namespace_form(cache_id: c_int, key: PgDatum) -> Option<PgNamespaceFormData> {
    match cache_id {
        SYSCACHE_NAMESPACEOID => {
            if key as u32 == PG_CATALOG_NAMESPACE_OID {
                Some(pg_catalog_namespace_form())
            } else {
                None
            }
        }
        SYSCACHE_NAMESPACENAME => {
            if key == 0 {
                return None;
            }
            let name = unsafe { CStr::from_ptr(key as *const c_char) }
                .to_string_lossy()
                .into_owned();
            if name == "pg_catalog" {
                Some(pg_catalog_namespace_form())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn pg_type_form(dt: DataType) -> PgTypeFormData {
    let mut typname = [0 as c_char; 64];
    copy_c_name(&mut typname, dt.pg_type_name());
    PgTypeFormData {
        oid: dt.oid() as u32,
        typname,
        typnamespace: PG_CATALOG_NAMESPACE_OID,
        typlen: dt.type_size(),
        typbyval: dt.type_size() > 0 && dt.type_size() <= 8,
        typtype: b'b' as c_char,
        typcategory: native_type_category(dt) as c_char,
        typcollation: if dt == DataType::Text {
            DEFAULT_COLLATION_OID
        } else {
            0
        },
        typelem: 0,
        typrelid: 0,
        typalign: b'i' as c_char,
    }
}

fn pg_proc_form_for_udf(udf: &NativeUdf) -> PgProcFormData {
    let mut proname = [0 as c_char; 64];
    copy_c_name(&mut proname, &udf.name);
    PgProcFormData {
        oid: udf.oid,
        proname,
        pronamespace: PG_CATALOG_NAMESPACE_OID,
        proowner: 10,
        prolang: C_LANGUAGE_OID,
        prokind: b'f' as c_char,
        proisstrict: udf.strict,
        proretset: false,
        prorettype: udf.return_type.map_or(0, |dt| dt.oid() as u32),
        pronargs: udf.arg_types.len() as i16,
    }
}

fn pg_catalog_namespace_form() -> PgNamespaceFormData {
    let mut nspname = [0 as c_char; 64];
    copy_c_name(&mut nspname, "pg_catalog");
    PgNamespaceFormData {
        oid: PG_CATALOG_NAMESPACE_OID,
        nspname,
        nspowner: 10,
        nspacl: std::ptr::null_mut(),
    }
}

fn pg_attribute_form(attnum: i16, name: &str, data_type: DataType) -> PgAttributeFormData {
    let mut attname = [0 as c_char; 64];
    copy_c_name(&mut attname, name);
    PgAttributeFormData {
        attrelid: 0,
        attname,
        atttypid: data_type.oid() as u32,
        attlen: data_type.type_size(),
        attnum,
        attbyval: data_type.type_size() > 0 && data_type.type_size() <= 8,
        attalign: b'i' as c_char,
        attnotnull: false,
    }
}

fn infer_spi_column_type(rows: &[Vec<Value>], column_index: usize) -> DataType {
    rows.iter()
        .filter_map(|row| row.get(column_index))
        .find(|value| !value.is_null())
        .map(Value::inferred_type)
        .unwrap_or(DataType::Text)
}

fn copy_c_name(out: &mut [c_char], value: &str) {
    let bytes = value.as_bytes();
    let copy_len = bytes.len().min(out.len().saturating_sub(1));
    for (idx, byte) in bytes.iter().take(copy_len).enumerate() {
        out[idx] = *byte as c_char;
    }
}

fn native_type_category(dt: DataType) -> u8 {
    match dt {
        DataType::Bool => b'B',
        DataType::Int2
        | DataType::Int4
        | DataType::Int8
        | DataType::Float4
        | DataType::Float8
        | DataType::Numeric
        | DataType::Money => b'N',
        DataType::Date
        | DataType::Time
        | DataType::TimeTz
        | DataType::Interval
        | DataType::Timestamp
        | DataType::TimestampTz => b'D',
        DataType::Json | DataType::Jsonb => b'U',
        DataType::Text
        | DataType::Bytea
        | DataType::Inet
        | DataType::Cidr
        | DataType::Macaddr
        | DataType::Macaddr8
        | DataType::Uuid
        | DataType::Xml
        | DataType::TsVector
        | DataType::TsQuery => b'S',
    }
}

fn release_syscache_tuple(state: &mut NativeSpiState, tuple: *mut PgHeapTupleData) {
    let Some(non_null) = NonNull::new(tuple) else {
        return;
    };
    if let Some(pos) = state
        .syscache_tuples
        .iter()
        .position(|cached| cached.as_ptr() == tuple)
    {
        state.syscache_tuples.swap_remove(pos);
        unsafe {
            free_syscache_tuple(non_null);
        }
    }
}

unsafe fn free_syscache_tuple(tuple: NonNull<PgHeapTupleData>) {
    let tuple = unsafe { Box::from_raw(tuple.as_ptr()) };
    unsafe {
        free_syscache_form(tuple.data, tuple.kind);
    }
}

unsafe fn free_syscache_form(data: *mut c_void, kind: c_int) {
    if data.is_null() {
        return;
    }
    match kind {
        HEAP_TUPLE_KIND_PG_TYPE => unsafe {
            drop(Box::from_raw(data as *mut PgTypeFormData));
        },
        HEAP_TUPLE_KIND_PG_PROC => unsafe {
            drop(Box::from_raw(data as *mut PgProcFormData));
        },
        HEAP_TUPLE_KIND_PG_NAMESPACE => unsafe {
            drop(Box::from_raw(data as *mut PgNamespaceFormData));
        },
        _ => {}
    }
}

impl NativeSpiState {
    fn set_rows(&mut self, rows: Vec<Vec<Value>>, fields: Vec<NativeSpiField>) {
        self.rows.clear();
        self.datum_rows.clear();
        self.datum_storage.clear();
        self.tuple_attrs.clear();
        let attr_count = fields.len().max(rows.first().map_or(0, Vec::len));
        for idx in 0..attr_count {
            let data_type = fields
                .get(idx)
                .map(|field| field.data_type)
                .unwrap_or_else(|| infer_spi_column_type(&rows, idx));
            let name = fields
                .get(idx)
                .map(|field| field.name.as_str())
                .filter(|name| !name.is_empty())
                .unwrap_or("?column?");
            self.tuple_attrs
                .push(pg_attribute_form(idx as i16 + 1, name, data_type));
        }
        for row in rows {
            let mut text_row = Vec::with_capacity(row.len());
            let mut datum_row = Vec::with_capacity(row.len());
            for value in row {
                text_row.push(value.to_text().and_then(|text| CString::new(text).ok()));
                datum_row.push(value_to_datum(&value, &mut self.datum_storage));
            }
            self.rows.push(text_row);
            self.datum_rows.push(datum_row);
        }
        self.heap_rows = (0..self.rows.len())
            .map(|idx| {
                Box::new(PgHeapTupleData {
                    row_index: idx,
                    data: std::ptr::null_mut(),
                    kind: HEAP_TUPLE_KIND_SPI,
                    natts: 0,
                    values: std::ptr::null_mut(),
                    isnull: std::ptr::null_mut(),
                })
            })
            .collect();
        self.row_ptrs = self
            .heap_rows
            .iter()
            .map(|row| row.as_ref() as *const PgHeapTupleData)
            .collect();
        let natts = attr_count as c_int;
        self.tuple_desc = Some(Box::new(PgTupleDescData {
            natts,
            attrs: self.tuple_attrs.as_ptr(),
        }));
        self.tuple_table = Some(Box::new(PgSpiTupleTable {
            tupdesc: self
                .tuple_desc
                .as_deref()
                .map(|desc| desc as *const PgTupleDescData)
                .unwrap_or(std::ptr::null()),
            vals: self.row_ptrs.as_ptr(),
            numvals: self.row_ptrs.len() as u64,
        }));
    }

    fn clear_rows(&mut self) {
        self.rows.clear();
        self.datum_rows.clear();
        self.datum_storage.clear();
        self.heap_rows.clear();
        self.row_ptrs.clear();
        self.tuple_attrs.clear();
        self.tuple_desc = None;
        self.tuple_table = None;
    }
}

impl Drop for NativeSpiState {
    fn drop(&mut self) {
        for tuple in std::mem::take(&mut self.syscache_tuples) {
            unsafe {
                free_syscache_tuple(tuple);
            }
        }
    }
}

impl NativeMemoryState {
    fn alloc(&mut self, size: usize, zero: bool) -> *mut c_void {
        let Ok(layout) = native_layout(size) else {
            return std::ptr::null_mut();
        };
        let ptr = unsafe {
            if zero {
                alloc_zeroed(layout)
            } else {
                alloc(layout)
            }
        } as *mut c_void;
        let Some(non_null) = NonNull::new(ptr) else {
            return std::ptr::null_mut();
        };
        self.allocations.insert(non_null, layout);
        ptr
    }

    fn realloc(&mut self, ptr: *mut c_void, size: usize) -> *mut c_void {
        if ptr.is_null() {
            return self.alloc(size, false);
        }
        let Some(non_null) = NonNull::new(ptr) else {
            return std::ptr::null_mut();
        };
        let Some(old_layout) = self.allocations.remove(&non_null) else {
            return std::ptr::null_mut();
        };
        let Ok(new_layout) = native_layout(size) else {
            self.allocations.insert(non_null, old_layout);
            return std::ptr::null_mut();
        };
        let new_ptr =
            unsafe { realloc(ptr as *mut u8, old_layout, new_layout.size()) } as *mut c_void;
        let Some(new_non_null) = NonNull::new(new_ptr) else {
            self.allocations.insert(non_null, old_layout);
            return std::ptr::null_mut();
        };
        self.allocations.insert(new_non_null, new_layout);
        new_ptr
    }

    fn free(&mut self, ptr: *mut c_void) {
        let Some(non_null) = NonNull::new(ptr) else {
            return;
        };
        if let Some(layout) = self.allocations.remove(&non_null) {
            unsafe {
                dealloc(ptr as *mut u8, layout);
            }
        }
    }

    fn free_all_except_fmgr_extra(&mut self) {
        let mut retained = HashMap::new();
        let allocations = std::mem::take(&mut self.allocations);
        for (ptr, layout) in allocations {
            if is_fmgr_extra(ptr.as_ptr()) {
                retained.insert(ptr, layout);
            } else {
                unsafe {
                    dealloc(ptr.as_ptr() as *mut u8, layout);
                }
            }
        }
        self.allocations = retained;
    }
}

fn native_layout(size: usize) -> Result<Layout, std::alloc::LayoutError> {
    Layout::from_size_align(size.max(1), std::mem::align_of::<usize>())
}

fn is_fmgr_extra(ptr: *mut c_void) -> bool {
    if ptr.is_null() {
        return false;
    }
    FMGR_INFOS.with(|cell| {
        cell.borrow()
            .values()
            .any(|state| std::ptr::eq(state.info.fn_extra, ptr))
    })
}

fn value_to_datum(value: &Value, text_storage: &mut Vec<Vec<u8>>) -> PgNullableDatum {
    if value.is_null() {
        return PgNullableDatum {
            value: 0,
            isnull: true,
        };
    }

    let value = match value {
        Value::Null => 0,
        Value::Int(i) => *i as PgDatum,
        Value::Float(f) => f.to_bits() as PgDatum,
        Value::Numeric(n) => {
            text_storage.push(varlena_bytes(n.to_canonical_string().as_bytes()));
            text_storage.last().map_or(0, |s| s.as_ptr() as PgDatum)
        }
        Value::Text(s) => {
            text_storage.push(varlena_bytes(s.as_bytes()));
            text_storage.last().map_or(0, |s| s.as_ptr() as PgDatum)
        }
        Value::Bool(b) => usize::from(*b),
    };
    PgNullableDatum {
        value,
        isnull: false,
    }
}

fn datum_to_value(datum: PgDatum, return_type: Option<DataType>) -> Result<Value, String> {
    let Some(return_type) = return_type else {
        return Err("native function must declare a scalar return type".into());
    };
    match return_type {
        DataType::Int2 => Ok(Value::Int((datum as i16) as i64)),
        DataType::Int4 => Ok(Value::Int((datum as i32) as i64)),
        DataType::Int8 => Ok(Value::Int(datum as i64)),
        DataType::Float4 => Ok(Value::Float(f32::from_bits(datum as u32) as f64)),
        DataType::Float8 | DataType::Numeric | DataType::Money => {
            Ok(Value::Float(f64::from_bits(datum as u64)))
        }
        DataType::Bool => Ok(Value::Bool(datum != 0)),
        DataType::Text
        | DataType::Bytea
        | DataType::Date
        | DataType::Time
        | DataType::TimeTz
        | DataType::Interval
        | DataType::Timestamp
        | DataType::TimestampTz
        | DataType::Inet
        | DataType::Cidr
        | DataType::Macaddr
        | DataType::Macaddr8
        | DataType::Uuid
        | DataType::Json
        | DataType::Jsonb
        | DataType::Xml
        | DataType::TsVector
        | DataType::TsQuery => datum_text(datum).map(Value::Text),
    }
}

fn varlena_bytes(bytes: &[u8]) -> Vec<u8> {
    let total_len = bytes.len() + VARLENA_HEADER_LEN;
    let mut out = Vec::with_capacity(total_len);
    out.extend_from_slice(&(total_len as u32).to_ne_bytes());
    out.extend_from_slice(bytes);
    out
}

fn datum_text(datum: PgDatum) -> Result<String, String> {
    if datum == 0 {
        return Err("native function returned a null text pointer without setting isnull".into());
    }
    let ptr = datum as *const u8;
    let len = unsafe {
        let mut raw = [0u8; VARLENA_HEADER_LEN];
        std::ptr::copy_nonoverlapping(ptr, raw.as_mut_ptr(), VARLENA_HEADER_LEN);
        u32::from_ne_bytes(raw) as usize
    };
    if len < VARLENA_HEADER_LEN {
        return Err(format!(
            "native function returned invalid varlena length {len}"
        ));
    }
    let payload_len = len - VARLENA_HEADER_LEN;
    let payload = unsafe { std::slice::from_raw_parts(ptr.add(VARLENA_HEADER_LEN), payload_len) };
    String::from_utf8(payload.to_vec())
        .map_err(|e| format!("native function returned invalid UTF-8: {e}"))
}

fn load_symbol(library_path: &str, symbol: &str) -> Result<PgFmgrFn, String> {
    let handle = load_library(library_path)?;
    validate_function_info(handle, library_path, symbol)?;
    let symbol_c = CString::new(symbol)
        .map_err(|_| format!("native symbol contains interior NUL: {symbol:?}"))?;
    clear_dlerror();
    let ptr = unsafe { dlsym(handle, symbol_c.as_ptr()) };
    if ptr.is_null() {
        return Err(format!(
            "failed to load native symbol {symbol:?} from {library_path:?}: {}",
            dlerror_string()
        ));
    }
    Ok(unsafe { std::mem::transmute::<*mut c_void, PgFmgrFn>(ptr) })
}

fn load_library(library_path: &str) -> Result<*mut c_void, String> {
    let resolved = resolve_library_path(library_path);
    LIBRARIES.with(|cell| {
        if let Some(library) = cell.borrow().get(&resolved).copied() {
            return Ok(library.handle);
        }
        let path_c = CString::new(resolved.as_str())
            .map_err(|_| format!("native library path contains interior NUL: {resolved:?}"))?;
        clear_dlerror();
        let handle = unsafe { dlopen(path_c.as_ptr(), RTLD_NOW | RTLD_LOCAL) };
        if handle.is_null() {
            return Err(format!(
                "failed to load native library {resolved:?}: {}",
                dlerror_string()
            ));
        }
        validate_module_magic(handle, &resolved)?;
        call_pg_init(handle)?;
        cell.borrow_mut().insert(resolved, NativeLibrary { handle });
        Ok(handle)
    })
}

fn validate_module_magic(handle: *mut c_void, library_path: &str) -> Result<(), String> {
    let ptr = lookup_required_symbol(handle, library_path, "Pg_magic_func")?;
    let magic_fn = unsafe { std::mem::transmute::<*mut c_void, PgModuleMagicFn>(ptr) };
    let magic = unsafe { magic_fn() };
    if magic.is_null() {
        return Err(format!(
            "native library {library_path:?} returned null module magic"
        ));
    }
    let magic = unsafe { &*magic };
    if magic.len as usize != std::mem::size_of::<PgMagicStruct>() {
        return Err(format!(
            "native library {library_path:?} has incompatible module magic length {}",
            magic.len
        ));
    }
    if magic.version != PG_MAGIC_VERSION {
        return Err(format!(
            "native library {library_path:?} has incompatible module magic version {}, expected {}",
            magic.version, PG_MAGIC_VERSION
        ));
    }
    Ok(())
}

fn validate_function_info(
    handle: *mut c_void,
    library_path: &str,
    symbol: &str,
) -> Result<(), String> {
    let finfo_symbol = format!("pg_finfo_{symbol}");
    let ptr = lookup_required_symbol(handle, library_path, &finfo_symbol)?;
    let finfo_fn = unsafe { std::mem::transmute::<*mut c_void, PgFinfoFn>(ptr) };
    let finfo = unsafe { finfo_fn() };
    if finfo.is_null() {
        return Err(format!(
            "native symbol {symbol:?} in {library_path:?} returned null function metadata"
        ));
    }
    let finfo = unsafe { &*finfo };
    if finfo.api_version != 1 {
        return Err(format!(
            "native symbol {symbol:?} in {library_path:?} uses unsupported function API version {}",
            finfo.api_version
        ));
    }
    Ok(())
}

fn call_pg_init(handle: *mut c_void) -> Result<(), String> {
    clear_dlerror();
    let name = CString::new("_PG_init").expect("static symbol has no nul");
    let ptr = unsafe { dlsym(handle, name.as_ptr()) };
    if ptr.is_null() {
        clear_dlerror();
        return Ok(());
    }
    let init_fn = unsafe { std::mem::transmute::<*mut c_void, PgInitFn>(ptr) };
    unsafe {
        init_fn();
    }
    Ok(())
}

fn lookup_required_symbol(
    handle: *mut c_void,
    library_path: &str,
    symbol: &str,
) -> Result<*mut c_void, String> {
    let symbol_c = CString::new(symbol)
        .map_err(|_| format!("native symbol contains interior NUL: {symbol:?}"))?;
    clear_dlerror();
    let ptr = unsafe { dlsym(handle, symbol_c.as_ptr()) };
    if ptr.is_null() {
        return Err(format!(
            "failed to load required native symbol {symbol:?} from {library_path:?}: {}",
            dlerror_string()
        ));
    }
    Ok(ptr)
}

fn resolve_library_path(library_path: &str) -> String {
    if let Some(rest) = library_path.strip_prefix("$libdir/") {
        if let Ok(dir) = env::var("PGRS_EXTENSION_DIR") {
            return PathBuf::from(dir).join(rest).display().to_string();
        }
    }
    if library_path.contains('/') {
        return library_path.to_string();
    }
    if let Ok(dir) = env::var("PGRS_EXTENSION_DIR") {
        return PathBuf::from(dir).join(library_path).display().to_string();
    }
    library_path.to_string()
}

fn clear_dlerror() {
    unsafe {
        dlerror();
    }
}

fn dlerror_string() -> String {
    let err = unsafe { dlerror() };
    if err.is_null() {
        return "unknown dynamic loader error".into();
    }
    unsafe { CStr::from_ptr(err) }
        .to_string_lossy()
        .into_owned()
}
