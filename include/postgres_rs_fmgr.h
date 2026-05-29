#ifndef POSTGRES_RS_FMGR_H
#define POSTGRES_RS_FMGR_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

/*
 * Small PostgreSQL-style fmgr compatibility header for postgres-rs native
 * scalar functions. It intentionally covers only the Datum/FunctionCallInfo
 * surface currently supported by the engine.
 */

typedef uintptr_t Datum;
typedef uint32_t Oid;
typedef struct PgrsMemoryContextData *MemoryContext;

#define InvalidOid ((Oid)0)
#define TEXTOID ((Oid)25)
#define INT4OID ((Oid)23)
#define INT8OID ((Oid)20)
#define FLOAT8OID ((Oid)701)
#define BOOLOID ((Oid)16)
#define PG_CATALOG_NAMESPACE_OID ((Oid)11)

#define TYPEOID 1
#define TYPENAMENSP 2
#define PROCOID 3

#ifndef PGDLLEXPORT
#define PGDLLEXPORT
#endif

typedef struct FunctionCallInfoData *FunctionCallInfo;
typedef Datum (*PGFunction)(FunctionCallInfo fcinfo);

typedef struct FmgrInfo {
    PGFunction fn_addr;
    uint32_t fn_oid;
    int16_t fn_nargs;
    bool fn_strict;
    bool fn_retset;
    uint8_t fn_stats;
    void *fn_extra;
    MemoryContext fn_mcxt;
    void *fn_expr;
} FmgrInfo;

typedef struct Pg_magic_struct {
    int len;
    int version;
} Pg_magic_struct;

#define PG_MODULE_MAGIC_DATA                                             \
    const Pg_magic_struct Pg_magic_data = {                              \
        sizeof(Pg_magic_struct),                                         \
        160000                                                           \
    };

#define PG_MODULE_MAGIC                                                  \
    PG_MODULE_MAGIC_DATA                                                 \
    PGDLLEXPORT const Pg_magic_struct *Pg_magic_func(void);              \
    const Pg_magic_struct *Pg_magic_func(void) { return &Pg_magic_data; }\

typedef struct Pg_finfo_record {
    int api_version;
} Pg_finfo_record;

#define PG_FUNCTION_INFO_V1(funcname)                                    \
    PGDLLEXPORT const Pg_finfo_record *pg_finfo_##funcname(void);        \
    const Pg_finfo_record *pg_finfo_##funcname(void) {                   \
        static const Pg_finfo_record record = {1};                       \
        return &record;                                                  \
    }

typedef struct NullableDatum {
    Datum value;
    bool isnull;
} NullableDatum;

#define PGRS_MAX_FMGR_ARGS 32

typedef struct FunctionCallInfoData {
    FmgrInfo *flinfo;
    void *context;
    void *resultinfo;
    uint32_t fncollation;
    bool isnull;
    uint16_t nargs;
    NullableDatum args[PGRS_MAX_FMGR_ARGS];
} FunctionCallInfoData;

typedef struct HeapTupleData {
    size_t row_index;
    void *data;
    int kind;
} HeapTupleData;

typedef HeapTupleData *HeapTuple;

typedef struct TupleDescData {
    int natts;
} TupleDescData;

typedef TupleDescData *TupleDesc;

typedef struct SPITupleTable {
    TupleDesc tupdesc;
    HeapTuple *vals;
    uint64_t numvals;
} SPITupleTable;

typedef const char *SPIPlanPtr;

typedef int (*PgrsSpiExecuteFn)(void *ctx, const char *query, bool read_only, int64_t count);
typedef char *(*PgrsSpiGetValueFn)(void *ctx, size_t row_index, int column_index);
typedef Datum (*PgrsSpiGetBinValFn)(void *ctx, size_t row_index, int column_index, bool *isnull);
typedef void *(*PgrsMemoryAllocFn)(void *ctx, size_t size, bool zero);
typedef void *(*PgrsMemoryReallocFn)(void *ctx, void *ptr, size_t size);
typedef void (*PgrsMemoryFreeFn)(void *ctx, void *ptr);
typedef void (*PgrsReportErrorFn)(void *ctx, int elevel, const char *message);
typedef char *(*PgrsGetConfigOptionFn)(void *ctx, const char *name, bool missing_ok);
typedef HeapTuple (*PgrsSearchSysCache1Fn)(void *ctx, int cache_id, Datum key);
typedef HeapTuple (*PgrsSearchSysCache2Fn)(void *ctx, int cache_id, Datum key1, Datum key2);
typedef void (*PgrsReleaseSysCacheFn)(void *ctx, HeapTuple tuple);
typedef void *(*PgrsMemoryContextAllocFn)(MemoryContext context, size_t size, bool zero);
typedef void *(*PgrsMemoryContextReallocFn)(MemoryContext context, void *ptr, size_t size);
typedef void (*PgrsMemoryContextFreeFn)(MemoryContext context, void *ptr);

typedef struct PgrsMemoryContextData {
    void *state;
    PgrsMemoryContextAllocFn alloc;
    PgrsMemoryContextReallocFn realloc;
    PgrsMemoryContextFreeFn free;
} PgrsMemoryContextData;

typedef struct PgrsExtensionContext {
    void *spi_state;
    void *memory_state;
    MemoryContext current_memory_context;
    PgrsSpiExecuteFn spi_execute;
    PgrsSpiGetValueFn spi_getvalue;
    PgrsSpiGetBinValFn spi_getbinval;
    PgrsMemoryAllocFn memory_alloc;
    PgrsMemoryReallocFn memory_realloc;
    PgrsMemoryFreeFn memory_free;
    PgrsReportErrorFn report_error;
    PgrsGetConfigOptionFn get_config_option;
    PgrsSearchSysCache1Fn search_syscache1;
    PgrsSearchSysCache2Fn search_syscache2;
    PgrsReleaseSysCacheFn release_syscache;
    size_t spi_processed;
    SPITupleTable *spi_tuptable;
    int spi_result;
} PgrsExtensionContext;

typedef struct FormData_pg_type {
    Oid oid;
    char typname[64];
    Oid typnamespace;
    int16_t typlen;
    bool typbyval;
    char typtype;
    char typcategory;
    Oid typcollation;
    Oid typelem;
    Oid typrelid;
    char typalign;
} FormData_pg_type;

typedef FormData_pg_type *Form_pg_type;

typedef struct FormData_pg_proc {
    Oid oid;
    char proname[64];
    Oid pronamespace;
    Oid proowner;
    Oid prolang;
    char prokind;
    bool proisstrict;
    bool proretset;
    Oid prorettype;
    int16_t pronargs;
} FormData_pg_proc;

typedef FormData_pg_proc *Form_pg_proc;

typedef enum GucContext {
    PGC_INTERNAL,
    PGC_POSTMASTER,
    PGC_SIGHUP,
    PGC_SU_BACKEND,
    PGC_BACKEND,
    PGC_SUSET,
    PGC_USERSET,
} GucContext;

typedef enum GucSource {
    PGC_S_DEFAULT,
    PGC_S_DYNAMIC_DEFAULT,
    PGC_S_ENV_VAR,
    PGC_S_FILE,
    PGC_S_ARGV,
    PGC_S_GLOBAL,
    PGC_S_DATABASE,
    PGC_S_USER,
    PGC_S_DATABASE_USER,
    PGC_S_CLIENT,
    PGC_S_OVERRIDE,
    PGC_S_INTERACTIVE,
    PGC_S_TEST,
    PGC_S_SESSION,
} GucSource;

#define GUC_LIST_INPUT 0x0001
#define GUC_LIST_QUOTE 0x0002
#define GUC_NO_SHOW_ALL 0x0004
#define GUC_NO_RESET_ALL 0x0008
#define GUC_NOT_IN_SAMPLE 0x0010
#define GUC_DISALLOW_IN_FILE 0x0020
#define GUC_CUSTOM_PLACEHOLDER 0x0040

#define DEBUG1 10
#define LOG 15
#define INFO 17
#define NOTICE 18
#define WARNING 19
#define ERROR 20
#define FATAL 21
#define PANIC 22

#define ERRCODE_INVALID_PARAMETER_VALUE 22023
#define ERRCODE_FEATURE_NOT_SUPPORTED 0

#define SPI_OK_CONNECT 1
#define SPI_OK_FINISH 2
#define SPI_OK_UTILITY 4
#define SPI_OK_SELECT 5
#define SPI_OK_INSERT 7
#define SPI_OK_DELETE 8
#define SPI_OK_UPDATE 9
#define SPI_OK_INSERT_RETURNING 11
#define SPI_OK_DELETE_RETURNING 12
#define SPI_OK_UPDATE_RETURNING 13
#define SPI_OK_MERGE 18

#define PG_FUNCTION_ARGS FunctionCallInfo fcinfo
#define PG_GET_COLLATION() (fcinfo->fncollation)
#define PG_NARGS() (fcinfo->nargs)
#define PG_ARGISNULL(n) (fcinfo->args[(n)].isnull)

#define SPI_processed (((PgrsExtensionContext *)fcinfo->context)->spi_processed)
#define SPI_tuptable (((PgrsExtensionContext *)fcinfo->context)->spi_tuptable)
#define SPI_result (((PgrsExtensionContext *)fcinfo->context)->spi_result)

static inline int pgrs_spi_connect(FunctionCallInfo fcinfo) {
    (void)fcinfo;
    return SPI_OK_CONNECT;
}

static inline int pgrs_spi_finish(FunctionCallInfo fcinfo) {
    (void)fcinfo;
    return SPI_OK_FINISH;
}

static inline int pgrs_spi_execute(FunctionCallInfo fcinfo, const char *query, bool read_only, int64_t count) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL || ctx->spi_execute == NULL) {
        return -1;
    }
    return ctx->spi_execute(ctx, query, read_only, count);
}

static inline char *pgrs_spi_getvalue(FunctionCallInfo fcinfo, HeapTuple tuple, TupleDesc tupdesc, int fnumber) {
    (void)tupdesc;
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL || ctx->spi_getvalue == NULL || tuple == NULL || fnumber < 1) {
        return NULL;
    }
    return ctx->spi_getvalue(ctx, tuple->row_index, fnumber - 1);
}

static inline Datum pgrs_spi_getbinval(FunctionCallInfo fcinfo, HeapTuple tuple, TupleDesc tupdesc, int fnumber, bool *isnull) {
    (void)tupdesc;
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL || ctx->spi_getbinval == NULL || tuple == NULL || fnumber < 1) {
        if (isnull != NULL) {
            *isnull = true;
        }
        return (Datum)0;
    }
    return ctx->spi_getbinval(ctx, tuple->row_index, fnumber - 1, isnull);
}

static inline HeapTuple pgrs_search_syscache1(FunctionCallInfo fcinfo, int cache_id, Datum key) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL || ctx->search_syscache1 == NULL) {
        return NULL;
    }
    return ctx->search_syscache1(ctx, cache_id, key);
}

static inline HeapTuple pgrs_search_syscache2(FunctionCallInfo fcinfo, int cache_id, Datum key1, Datum key2) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL || ctx->search_syscache2 == NULL) {
        return NULL;
    }
    return ctx->search_syscache2(ctx, cache_id, key1, key2);
}

static inline void pgrs_release_syscache(FunctionCallInfo fcinfo, HeapTuple tuple) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL || ctx->release_syscache == NULL || tuple == NULL) {
        return;
    }
    ctx->release_syscache(ctx, tuple);
}

static inline SPIPlanPtr pgrs_spi_prepare(FunctionCallInfo fcinfo, const char *query, int nargs, Oid *argtypes) {
    (void)nargs;
    (void)argtypes;
    if (query == NULL) {
        return NULL;
    }
    (void)fcinfo;
    size_t len = strlen(query) + 1;
    char *copy = (char *)malloc(len);
    if (copy == NULL) {
        return NULL;
    }
    memcpy(copy, query, len);
    return (SPIPlanPtr)copy;
}

static inline int pgrs_spi_execute_plan(FunctionCallInfo fcinfo, SPIPlanPtr plan, Datum *values, const char *nulls, bool read_only, int64_t count) {
    (void)values;
    (void)nulls;
    if (plan == NULL) {
        return -1;
    }
    return pgrs_spi_execute(fcinfo, plan, read_only, count);
}

static inline void *MemoryContextAlloc(MemoryContext context, size_t size) {
    if (context == NULL || context->alloc == NULL) {
        return malloc(size);
    }
    return context->alloc(context, size, false);
}

static inline void *MemoryContextAllocZero(MemoryContext context, size_t size) {
    if (context == NULL || context->alloc == NULL) {
        return calloc(1, size);
    }
    return context->alloc(context, size, true);
}

static inline void *MemoryContextAllocZeroAligned(MemoryContext context, size_t size) {
    return MemoryContextAllocZero(context, size);
}

static inline char *MemoryContextStrdup(MemoryContext context, const char *s) {
    size_t len = strlen(s) + 1;
    char *out = (char *)MemoryContextAlloc(context, len);
    if (out != NULL) {
        memcpy(out, s, len);
    }
    return out;
}

static inline void *pgrs_memory_context_realloc(MemoryContext context, void *ptr, size_t size) {
    if (context == NULL || context->realloc == NULL) {
        return realloc(ptr, size);
    }
    return context->realloc(context, ptr, size);
}

static inline void pgrs_memory_context_free(MemoryContext context, void *ptr) {
    if (context == NULL || context->free == NULL) {
        free(ptr);
        return;
    }
    context->free(context, ptr);
}

static inline MemoryContext pgrs_memory_context_switch_to(FunctionCallInfo fcinfo, MemoryContext context) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL) {
        return NULL;
    }
    MemoryContext previous = ctx->current_memory_context;
    ctx->current_memory_context = context;
    return previous;
}

#define SPI_connect() pgrs_spi_connect(fcinfo)
#define SPI_finish() pgrs_spi_finish(fcinfo)
#define SPI_execute(query, read_only, count) pgrs_spi_execute(fcinfo, query, read_only, count)
#define SPI_exec(query, count) pgrs_spi_execute(fcinfo, query, false, count)
#define SPI_prepare(query, nargs, argtypes) pgrs_spi_prepare(fcinfo, query, nargs, argtypes)
#define SPI_execute_plan(plan, values, nulls, read_only, count) pgrs_spi_execute_plan(fcinfo, plan, values, nulls, read_only, count)
#define SPI_saveplan(plan) (plan)
#define SPI_freeplan(plan) free((void *)(plan))
#define SPI_getvalue(tuple, tupdesc, fnumber) pgrs_spi_getvalue(fcinfo, tuple, tupdesc, fnumber)
#define SPI_getbinval(tuple, tupdesc, fnumber, isnull) pgrs_spi_getbinval(fcinfo, tuple, tupdesc, fnumber, isnull)
#define SPI_freetuptable(tuptable) ((void)(tuptable))
#define CurrentMemoryContext (((PgrsExtensionContext *)fcinfo->context)->current_memory_context)
#define MemoryContextSwitchTo(context) pgrs_memory_context_switch_to(fcinfo, context)
#define SearchSysCache1(cache_id, key1) pgrs_search_syscache1(fcinfo, cache_id, key1)
#define SearchSysCache2(cache_id, key1, key2) pgrs_search_syscache2(fcinfo, cache_id, key1, key2)
#define ReleaseSysCache(tuple) pgrs_release_syscache(fcinfo, tuple)
#define HeapTupleIsValid(tuple) ((tuple) != NULL)
#define GETSTRUCT(tuple) ((tuple)->data)

#define PG_GETARG_DATUM(n) (fcinfo->args[(n)].value)
#define PG_GETARG_POINTER(n) ((void *)(uintptr_t)PG_GETARG_DATUM(n))
#define PG_GETARG_INT16(n) ((int16_t)PG_GETARG_DATUM(n))
#define PG_GETARG_INT32(n) ((int32_t)PG_GETARG_DATUM(n))
#define PG_GETARG_INT64(n) ((int64_t)PG_GETARG_DATUM(n))
#define PG_GETARG_BOOL(n) (PG_GETARG_DATUM(n) != 0)
#define PG_GETARG_OID(n) DatumGetObjectId(PG_GETARG_DATUM(n))

#define Int16GetDatum(x) ((Datum)((int16_t)(x)))
#define Int32GetDatum(x) ((Datum)((int32_t)(x)))
#define Int64GetDatum(x) ((Datum)((int64_t)(x)))
#define BoolGetDatum(x) ((Datum)((x) ? 1 : 0))
#define OidGetDatum(x) ObjectIdGetDatum(x)
#define DatumGetInt16(x) ((int16_t)(x))
#define DatumGetInt32(x) ((int32_t)(x))
#define DatumGetInt64(x) ((int64_t)(x))
#define DatumGetBool(x) ((bool)((x) != 0))
#define DatumGetPointer(x) ((void *)(uintptr_t)(x))
#define PointerGetDatum(x) ((Datum)(uintptr_t)(x))
#define ObjectIdGetDatum(x) ((Datum)(Oid)(x))
#define DatumGetObjectId(x) ((Oid)(x))
#define CStringGetDatum(x) PointerGetDatum(x)
#define DatumGetCString(x) ((char *)DatumGetPointer(x))
#define PG_FREE_IF_COPY(ptr, n)                 \
    do {                                        \
        if ((void *)(ptr) != PG_GETARG_POINTER(n)) { \
            pfree(ptr);                         \
        }                                       \
    } while (0)

static inline float pgrs_get_float4(Datum datum) {
    union {
        uint32_t bits;
        float value;
    } u;
    u.bits = (uint32_t)datum;
    return u.value;
}

static inline double pgrs_get_float8(Datum datum) {
    union {
        uint64_t bits;
        double value;
    } u;
    u.bits = (uint64_t)datum;
    return u.value;
}

#define PG_GETARG_FLOAT4(n) pgrs_get_float4(PG_GETARG_DATUM(n))
#define PG_GETARG_FLOAT8(n) pgrs_get_float8(PG_GETARG_DATUM(n))

typedef struct varlena {
    uint32_t len;
    char data[];
} varlena;

typedef varlena text;

#define VARHDRSZ ((int32_t)sizeof(uint32_t))
#define PG_GETARG_TEXT_PP(n) ((varlena *)PG_GETARG_DATUM(n))
#define PG_GETARG_TEXT_P(n) PG_GETARG_TEXT_PP(n)
#define VARDATA_ANY(vlena) ((char *)((vlena)->data))
#define VARDATA(vlena) VARDATA_ANY(vlena)
#define VARSIZE_ANY(vlena) ((size_t)((vlena)->len))
#define VARSIZE(vlena) VARSIZE_ANY(vlena)
#define VARSIZE_ANY_EXHDR(vlena) (VARSIZE_ANY(vlena) - sizeof(uint32_t))
#define VARSIZE_EXHDR(vlena) VARSIZE_ANY_EXHDR(vlena)
#define SET_VARSIZE(vlena, size) ((vlena)->len = (uint32_t)(size))

static inline void *pgrs_palloc(FunctionCallInfo fcinfo, size_t size) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL) {
        return malloc(size);
    }
    return MemoryContextAlloc(ctx->current_memory_context, size);
}

static inline void *pgrs_palloc0(FunctionCallInfo fcinfo, size_t size) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL) {
        return calloc(1, size);
    }
    return MemoryContextAllocZero(ctx->current_memory_context, size);
}

static inline void *pgrs_repalloc(FunctionCallInfo fcinfo, void *ptr, size_t size) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL) {
        return realloc(ptr, size);
    }
    return pgrs_memory_context_realloc(ctx->current_memory_context, ptr, size);
}

static inline void pgrs_pfree(FunctionCallInfo fcinfo, void *ptr) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL) {
        free(ptr);
        return;
    }
    pgrs_memory_context_free(ctx->current_memory_context, ptr);
}

static inline bool pgrs_lookup_type(Form_pg_type *form, HeapTuple *tuple, FunctionCallInfo fcinfo, Oid typid) {
    HeapTuple found = SearchSysCache1(TYPEOID, ObjectIdGetDatum(typid));
    if (!HeapTupleIsValid(found)) {
        if (form != NULL) {
            *form = NULL;
        }
        if (tuple != NULL) {
            *tuple = NULL;
        }
        return false;
    }
    if (form != NULL) {
        *form = (Form_pg_type)GETSTRUCT(found);
    }
    if (tuple != NULL) {
        *tuple = found;
    } else {
        ReleaseSysCache(found);
    }
    return true;
}

static inline void pgrs_get_typlenbyvalalign(FunctionCallInfo fcinfo, Oid typid, int16_t *typlen, bool *typbyval, char *typalign) {
    Form_pg_type form = NULL;
    HeapTuple tuple = NULL;
    if (pgrs_lookup_type(&form, &tuple, fcinfo, typid)) {
        if (typlen != NULL) {
            *typlen = form->typlen;
        }
        if (typbyval != NULL) {
            *typbyval = form->typbyval;
        }
        if (typalign != NULL) {
            *typalign = form->typalign;
        }
        ReleaseSysCache(tuple);
        return;
    }
    if (typlen != NULL) {
        *typlen = -1;
    }
    if (typbyval != NULL) {
        *typbyval = false;
    }
    if (typalign != NULL) {
        *typalign = 'i';
    }
}

static inline int16_t pgrs_get_typlen(FunctionCallInfo fcinfo, Oid typid) {
    int16_t typlen = -1;
    pgrs_get_typlenbyvalalign(fcinfo, typid, &typlen, NULL, NULL);
    return typlen;
}

static inline bool pgrs_get_typbyval(FunctionCallInfo fcinfo, Oid typid) {
    bool typbyval = false;
    pgrs_get_typlenbyvalalign(fcinfo, typid, NULL, &typbyval, NULL);
    return typbyval;
}

static inline char pgrs_get_typalign(FunctionCallInfo fcinfo, Oid typid) {
    char typalign = 'i';
    pgrs_get_typlenbyvalalign(fcinfo, typid, NULL, NULL, &typalign);
    return typalign;
}

static inline char *pgrs_format_type_be(FunctionCallInfo fcinfo, Oid typid) {
    Form_pg_type form = NULL;
    HeapTuple tuple = NULL;
    char *out = NULL;
    if (pgrs_lookup_type(&form, &tuple, fcinfo, typid)) {
        out = MemoryContextStrdup(CurrentMemoryContext, form->typname);
        ReleaseSysCache(tuple);
        return out;
    }
    return MemoryContextStrdup(CurrentMemoryContext, "unknown");
}

static inline char *pgrs_get_func_name(FunctionCallInfo fcinfo, Oid funcid) {
    HeapTuple tuple = SearchSysCache1(PROCOID, ObjectIdGetDatum(funcid));
    if (!HeapTupleIsValid(tuple)) {
        return NULL;
    }
    Form_pg_proc form = (Form_pg_proc)GETSTRUCT(tuple);
    char *out = MemoryContextStrdup(CurrentMemoryContext, form->proname);
    ReleaseSysCache(tuple);
    return out;
}

static inline Oid pgrs_get_func_rettype(FunctionCallInfo fcinfo, Oid funcid) {
    HeapTuple tuple = SearchSysCache1(PROCOID, ObjectIdGetDatum(funcid));
    if (!HeapTupleIsValid(tuple)) {
        return InvalidOid;
    }
    Form_pg_proc form = (Form_pg_proc)GETSTRUCT(tuple);
    Oid rettype = form->prorettype;
    ReleaseSysCache(tuple);
    return rettype;
}

static inline int pgrs_get_func_nargs(FunctionCallInfo fcinfo, Oid funcid) {
    HeapTuple tuple = SearchSysCache1(PROCOID, ObjectIdGetDatum(funcid));
    if (!HeapTupleIsValid(tuple)) {
        return -1;
    }
    Form_pg_proc form = (Form_pg_proc)GETSTRUCT(tuple);
    int nargs = form->pronargs;
    ReleaseSysCache(tuple);
    return nargs;
}

#define get_typlenbyvalalign(typid, typlen, typbyval, typalign) pgrs_get_typlenbyvalalign(fcinfo, typid, typlen, typbyval, typalign)
#define get_typlen(typid) pgrs_get_typlen(fcinfo, typid)
#define get_typbyval(typid) pgrs_get_typbyval(fcinfo, typid)
#define get_typalign(typid) pgrs_get_typalign(fcinfo, typid)
#define format_type_be(typid) pgrs_format_type_be(fcinfo, typid)
#define get_func_name(funcid) pgrs_get_func_name(fcinfo, funcid)
#define get_func_rettype(funcid) pgrs_get_func_rettype(fcinfo, funcid)
#define get_func_nargs(funcid) pgrs_get_func_nargs(fcinfo, funcid)

static inline char *pgrs_format_message(FunctionCallInfo fcinfo, const char *fmt, ...) {
    va_list args;
    va_start(args, fmt);
    va_list copy;
    va_copy(copy, args);
    int needed = vsnprintf(NULL, 0, fmt, copy);
    va_end(copy);
    if (needed < 0) {
        va_end(args);
        return NULL;
    }
    char *out = (char *)pgrs_palloc(fcinfo, (size_t)needed + 1);
    if (out != NULL) {
        vsnprintf(out, (size_t)needed + 1, fmt, args);
    }
    va_end(args);
    return out;
}

static inline void pgrs_report(FunctionCallInfo fcinfo, int elevel, const char *message) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL || ctx->report_error == NULL) {
        return;
    }
    ctx->report_error(ctx, elevel, message);
}

static inline int pgrs_errcode(int code) {
    (void)code;
    return 0;
}

static inline const char *pgrs_get_config_option(FunctionCallInfo fcinfo, const char *name, bool missing_ok) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL || ctx->get_config_option == NULL) {
        return NULL;
    }
    return ctx->get_config_option(ctx, name, missing_ok);
}

static inline const char *pgrs_get_config_option_by_name(FunctionCallInfo fcinfo, const char *name, const char **varname, bool missing_ok) {
    if (varname != NULL) {
        *varname = name;
    }
    return pgrs_get_config_option(fcinfo, name, missing_ok);
}

static inline void DefineCustomStringVariable(
    const char *name,
    const char *short_desc,
    const char *long_desc,
    char **valueAddr,
    const char *bootValue,
    GucContext context,
    int flags,
    void *check_hook,
    void *assign_hook,
    void *show_hook
) {
    (void)name;
    (void)short_desc;
    (void)long_desc;
    (void)context;
    (void)flags;
    (void)check_hook;
    (void)assign_hook;
    (void)show_hook;
    if (valueAddr != NULL) {
        *valueAddr = (char *)bootValue;
    }
}

static inline void DefineCustomBoolVariable(
    const char *name,
    const char *short_desc,
    const char *long_desc,
    bool *valueAddr,
    bool bootValue,
    GucContext context,
    int flags,
    void *check_hook,
    void *assign_hook,
    void *show_hook
) {
    (void)name;
    (void)short_desc;
    (void)long_desc;
    (void)context;
    (void)flags;
    (void)check_hook;
    (void)assign_hook;
    (void)show_hook;
    if (valueAddr != NULL) {
        *valueAddr = bootValue;
    }
}

static inline void DefineCustomIntVariable(
    const char *name,
    const char *short_desc,
    const char *long_desc,
    int *valueAddr,
    int bootValue,
    int minValue,
    int maxValue,
    GucContext context,
    int flags,
    void *check_hook,
    void *assign_hook,
    void *show_hook
) {
    (void)name;
    (void)short_desc;
    (void)long_desc;
    (void)minValue;
    (void)maxValue;
    (void)context;
    (void)flags;
    (void)check_hook;
    (void)assign_hook;
    (void)show_hook;
    if (valueAddr != NULL) {
        *valueAddr = bootValue;
    }
}

#define GetConfigOption(name, missing_ok, restrict_privileged) pgrs_get_config_option(fcinfo, name, missing_ok)
#define GetConfigOptionByName(name, varname, missing_ok) pgrs_get_config_option_by_name(fcinfo, name, varname, missing_ok)

#define errmsg(fmt, ...) pgrs_format_message(fcinfo, fmt, ##__VA_ARGS__)
#define errcode(code) pgrs_errcode(code)
#define errdetail(fmt, ...) (0)
#define errhint(fmt, ...) (0)
#define elog(elevel, fmt, ...)                                            \
    do {                                                                  \
        char *pgrs_message = pgrs_format_message(fcinfo, fmt, ##__VA_ARGS__); \
        pgrs_report(fcinfo, elevel, pgrs_message);                        \
        if ((elevel) >= ERROR) {                                          \
            PG_RETURN_NULL();                                             \
        }                                                                 \
    } while (0)
#define ereport(elevel, rest)                                             \
    do {                                                                  \
        const char *pgrs_message = rest;                                  \
        pgrs_report(fcinfo, elevel, pgrs_message);                        \
        if ((elevel) >= ERROR) {                                          \
            PG_RETURN_NULL();                                             \
        }                                                                 \
    } while (0)

static inline text *pgrs_cstring_to_text(FunctionCallInfo fcinfo, const char *s) {
    size_t len = strlen(s);
    text *out = (text *)pgrs_palloc(fcinfo, VARHDRSZ + len);
    SET_VARSIZE(out, VARHDRSZ + len);
    memcpy(VARDATA(out), s, len);
    return out;
}

static inline char *pgrs_text_to_cstring(FunctionCallInfo fcinfo, const text *t) {
    size_t len = VARSIZE_ANY_EXHDR(t);
    char *out = (char *)pgrs_palloc(fcinfo, len + 1);
    memcpy(out, VARDATA_ANY(t), len);
    out[len] = '\0';
    return out;
}

#define palloc(size) pgrs_palloc(fcinfo, size)
#define palloc0(size) pgrs_palloc0(fcinfo, size)
#define repalloc(ptr, size) pgrs_repalloc(fcinfo, ptr, size)
#define pfree(ptr) pgrs_pfree(fcinfo, ptr)
#define cstring_to_text(s) pgrs_cstring_to_text(fcinfo, s)
#define text_to_cstring(t) pgrs_text_to_cstring(fcinfo, t)

#define PG_RETURN_NULL()          \
    do {                          \
        fcinfo->isnull = true;    \
        return (Datum)0;          \
    } while (0)

#define PG_RETURN_DATUM(x) return (Datum)(x)
#define PG_RETURN_INT16(x) return (Datum)((int16_t)(x))
#define PG_RETURN_INT32(x) return (Datum)((int32_t)(x))
#define PG_RETURN_INT64(x) return (Datum)((int64_t)(x))
#define PG_RETURN_BOOL(x) return (Datum)((x) ? 1 : 0)
#define PG_RETURN_OID(x) return ObjectIdGetDatum(x)
#define PG_RETURN_POINTER(x) return (Datum)(uintptr_t)(x)
#define PG_RETURN_TEXT_P(x) PG_RETURN_POINTER(x)

static inline Datum Float4GetDatum(float value) {
    union {
        float value;
        uint32_t bits;
    } u;
    u.value = value;
    return (Datum)u.bits;
}

static inline Datum Float8GetDatum(double value) {
    union {
        double value;
        uint64_t bits;
    } u;
    u.value = value;
    return (Datum)u.bits;
}

#define PG_RETURN_FLOAT4(x) return Float4GetDatum((float)(x))
#define PG_RETURN_FLOAT8(x) return Float8GetDatum((double)(x))

#endif
