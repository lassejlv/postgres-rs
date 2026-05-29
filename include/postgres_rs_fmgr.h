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
typedef char *CString;
typedef struct PgrsMemoryContextData *MemoryContext;

#define InvalidOid ((Oid)0)
#define NAMEDATALEN 64

typedef struct NameData {
    char data[NAMEDATALEN];
} NameData;

typedef NameData *Name;

#define NameStr(name) ((name).data)

#define BOOLOID ((Oid)16)
#define BYTEAOID ((Oid)17)
#define INT8OID ((Oid)20)
#define INT2OID ((Oid)21)
#define INT4OID ((Oid)23)
#define TEXTOID ((Oid)25)
#define JSONOID ((Oid)114)
#define XMLOID ((Oid)142)
#define CIDROID ((Oid)650)
#define FLOAT4OID ((Oid)700)
#define FLOAT8OID ((Oid)701)
#define MACADDR8OID ((Oid)774)
#define MACADDROID ((Oid)829)
#define INETOID ((Oid)869)
#define DATEOID ((Oid)1082)
#define TIMEOID ((Oid)1083)
#define TIMESTAMPOID ((Oid)1114)
#define TIMESTAMPTZOID ((Oid)1184)
#define INTERVALOID ((Oid)1186)
#define TIMETZOID ((Oid)1266)
#define NUMERICOID ((Oid)1700)
#define UUIDOID ((Oid)2950)
#define JSONBOID ((Oid)3802)
#define TSVECTOROID ((Oid)3614)
#define TSQUERYOID ((Oid)3615)
#define PG_CATALOG_NAMESPACE_OID ((Oid)11)

#define TYPEOID 1
#define TYPENAMENSP 2
#define PROCOID 3
#define NAMESPACEOID 4
#define NAMESPACENAME 5

#define PGRS_HEAP_TUPLE_KIND_SPI 0
#define PGRS_HEAP_TUPLE_KIND_PG_TYPE 1
#define PGRS_HEAP_TUPLE_KIND_PG_PROC 2
#define PGRS_HEAP_TUPLE_KIND_PG_NAMESPACE 3
#define PGRS_HEAP_TUPLE_KIND_FORMED 4

#define Anum_pg_type_oid 1
#define Anum_pg_proc_oid 1
#define Anum_pg_namespace_oid 1

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

#define LOCAL_FCINFO(name, nargs) FunctionCallInfoData name##_data; FunctionCallInfo name = &name##_data

static inline void pgrs_init_function_call_info(
    FunctionCallInfo fcinfo,
    FmgrInfo *flinfo,
    uint16_t nargs,
    Oid collation,
    void *context,
    void *resultinfo
) {
    fcinfo->flinfo = flinfo;
    fcinfo->context = context;
    fcinfo->resultinfo = resultinfo;
    fcinfo->fncollation = collation;
    fcinfo->isnull = false;
    fcinfo->nargs = nargs;
    for (uint16_t i = 0; i < PGRS_MAX_FMGR_ARGS; i++) {
        fcinfo->args[i].value = (Datum)0;
        fcinfo->args[i].isnull = true;
    }
}

#define InitFunctionCallInfoData(fcinfo, flinfo, nargs, collation, context, resultinfo) \
    pgrs_init_function_call_info(&(fcinfo), flinfo, nargs, collation, context, resultinfo)

static inline Datum FunctionCallInvoke(FunctionCallInfo fcinfo) {
    if (fcinfo == NULL || fcinfo->flinfo == NULL || fcinfo->flinfo->fn_addr == NULL) {
        if (fcinfo != NULL) {
            fcinfo->isnull = true;
        }
        return (Datum)0;
    }
    return fcinfo->flinfo->fn_addr(fcinfo);
}

static inline void *pgrs_palloc0(FunctionCallInfo fcinfo, size_t size);
static inline void pgrs_pfree(FunctionCallInfo fcinfo, void *ptr);
static inline void pgrs_get_typlenbyvalalign(FunctionCallInfo fcinfo, Oid typid, int16_t *typlen, bool *typbyval, char *typalign);

typedef struct HeapTupleData {
    size_t row_index;
    void *data;
    int kind;
    int natts;
    Datum *values;
    bool *isnull;
} HeapTupleData;

typedef HeapTupleData *HeapTuple;

typedef struct FormData_pg_attribute {
    Oid attrelid;
    char attname[64];
    Oid atttypid;
    int16_t attlen;
    int16_t attnum;
    bool attbyval;
    char attalign;
    bool attnotnull;
} FormData_pg_attribute;

typedef FormData_pg_attribute *Form_pg_attribute;

typedef struct TupleDescData {
    int natts;
    Form_pg_attribute attrs;
} TupleDescData;

typedef TupleDescData *TupleDesc;

typedef struct StringInfoData {
    char *data;
    int len;
    int maxlen;
    int cursor;
} StringInfoData;

typedef StringInfoData *StringInfo;

typedef struct SPITupleTable {
    TupleDesc tupdesc;
    HeapTuple *vals;
    uint64_t numvals;
} SPITupleTable;

typedef struct PgrsSpiPlanData {
    char *query;
    int nargs;
    Oid *argtypes;
} PgrsSpiPlanData;

typedef PgrsSpiPlanData *SPIPlanPtr;

typedef int (*PgrsSpiExecuteFn)(void *ctx, const char *query, bool read_only, int64_t count);
typedef int (*PgrsSpiExecuteWithArgsFn)(void *ctx, const char *query, int nargs, Oid *argtypes, Datum *values, const char *nulls, bool read_only, int64_t count);
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
    PgrsSpiExecuteWithArgsFn spi_execute_with_args;
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

typedef struct FormData_pg_namespace {
    Oid oid;
    char nspname[64];
    Oid nspowner;
    void *nspacl;
} FormData_pg_namespace;

typedef FormData_pg_namespace *Form_pg_namespace;

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

#define MCXT_ALLOC_HUGE 0x01
#define MCXT_ALLOC_NO_OOM 0x02
#define MCXT_ALLOC_ZERO 0x04

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

static inline int pgrs_spi_execute_with_args(
    FunctionCallInfo fcinfo,
    const char *query,
    int nargs,
    Oid *argtypes,
    Datum *values,
    const char *nulls,
    bool read_only,
    int64_t count
) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL || ctx->spi_execute_with_args == NULL) {
        return -1;
    }
    return ctx->spi_execute_with_args(ctx, query, nargs, argtypes, values, nulls, read_only, count);
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

static inline Form_pg_attribute pgrs_tuple_desc_attr(TupleDesc tupdesc, int attr_index) {
    if (tupdesc == NULL || tupdesc->attrs == NULL || attr_index < 0 || attr_index >= tupdesc->natts) {
        return NULL;
    }
    return &tupdesc->attrs[attr_index];
}

static inline Oid pgrs_spi_gettypeid(TupleDesc tupdesc, int fnumber) {
    Form_pg_attribute attr = pgrs_tuple_desc_attr(tupdesc, fnumber - 1);
    if (attr == NULL) {
        return InvalidOid;
    }
    return attr->atttypid;
}

static inline TupleDesc pgrs_create_template_tuple_desc(FunctionCallInfo fcinfo, int natts) {
    if (natts < 0) {
        return NULL;
    }
    TupleDesc tupdesc = (TupleDesc)pgrs_palloc0(fcinfo, sizeof(TupleDescData));
    if (tupdesc == NULL) {
        return NULL;
    }
    tupdesc->natts = natts;
    if (natts == 0) {
        return tupdesc;
    }
    tupdesc->attrs = (Form_pg_attribute)pgrs_palloc0(fcinfo, sizeof(FormData_pg_attribute) * (size_t)natts);
    if (tupdesc->attrs == NULL) {
        pgrs_pfree(fcinfo, tupdesc);
        return NULL;
    }
    for (int i = 0; i < natts; i++) {
        tupdesc->attrs[i].attnum = (int16_t)(i + 1);
        tupdesc->attrs[i].atttypid = InvalidOid;
        tupdesc->attrs[i].attlen = -1;
        tupdesc->attrs[i].attalign = 'i';
    }
    return tupdesc;
}

static inline void pgrs_tuple_desc_init_entry(
    FunctionCallInfo fcinfo,
    TupleDesc tupdesc,
    int attributeNumber,
    const char *attributeName,
    Oid oidtypeid,
    int32_t typmod,
    int attdim
) {
    (void)typmod;
    (void)attdim;
    if (tupdesc == NULL || tupdesc->attrs == NULL || attributeNumber < 1 || attributeNumber > tupdesc->natts) {
        return;
    }
    Form_pg_attribute attr = &tupdesc->attrs[attributeNumber - 1];
    attr->attnum = (int16_t)attributeNumber;
    attr->atttypid = oidtypeid;
    attr->attnotnull = false;
    attr->attrelid = InvalidOid;
    if (attributeName != NULL) {
        strncpy(attr->attname, attributeName, NAMEDATALEN - 1);
        attr->attname[NAMEDATALEN - 1] = '\0';
    }
    pgrs_get_typlenbyvalalign(fcinfo, oidtypeid, &attr->attlen, &attr->attbyval, &attr->attalign);
}

static inline TupleDesc pgrs_bless_tuple_desc(FunctionCallInfo fcinfo, TupleDesc tupdesc) {
    (void)fcinfo;
    return tupdesc;
}

static inline TupleDesc pgrs_tuple_desc_copy(FunctionCallInfo fcinfo, TupleDesc tupdesc) {
    if (tupdesc == NULL || tupdesc->natts < 0) {
        return NULL;
    }
    TupleDesc copy = pgrs_create_template_tuple_desc(fcinfo, tupdesc->natts);
    if (copy == NULL) {
        return NULL;
    }
    if (tupdesc->natts > 0 && tupdesc->attrs != NULL && copy->attrs != NULL) {
        memcpy(copy->attrs, tupdesc->attrs, sizeof(FormData_pg_attribute) * (size_t)tupdesc->natts);
    }
    return copy;
}

static inline void pgrs_free_tuple_desc(FunctionCallInfo fcinfo, TupleDesc tupdesc) {
    if (tupdesc == NULL) {
        return;
    }
    if (tupdesc->attrs != NULL) {
        pgrs_pfree(fcinfo, tupdesc->attrs);
    }
    pgrs_pfree(fcinfo, tupdesc);
}

static inline Datum pgrs_heap_getattr(FunctionCallInfo fcinfo, HeapTuple tuple, int fnumber, TupleDesc tupdesc, bool *isnull) {
    if (tuple == NULL || fnumber < 1) {
        if (isnull != NULL) {
            *isnull = true;
        }
        return (Datum)0;
    }
    if (tuple->kind == PGRS_HEAP_TUPLE_KIND_FORMED) {
        int idx = fnumber - 1;
        int natts = tuple->natts;
        if (natts == 0 && tupdesc != NULL) {
            natts = tupdesc->natts;
        }
        if (idx < 0 || idx >= natts || tuple->values == NULL) {
            if (isnull != NULL) {
                *isnull = true;
            }
            return (Datum)0;
        }
        bool attr_isnull = tuple->isnull != NULL && tuple->isnull[idx];
        if (isnull != NULL) {
            *isnull = attr_isnull;
        }
        if (attr_isnull) {
            return (Datum)0;
        }
        return tuple->values[idx];
    }
    if (tuple->kind != PGRS_HEAP_TUPLE_KIND_SPI) {
        if (isnull != NULL) {
            *isnull = true;
        }
        return (Datum)0;
    }
    return pgrs_spi_getbinval(fcinfo, tuple, tupdesc, fnumber, isnull);
}

static inline bool pgrs_heap_attisnull(FunctionCallInfo fcinfo, HeapTuple tuple, int fnumber, TupleDesc tupdesc) {
    bool isnull = true;
    (void)pgrs_heap_getattr(fcinfo, tuple, fnumber, tupdesc, &isnull);
    return isnull;
}

static inline HeapTuple pgrs_heap_form_tuple(FunctionCallInfo fcinfo, TupleDesc tupdesc, Datum *values, bool *isnull) {
    if (tupdesc == NULL || tupdesc->natts < 0) {
        return NULL;
    }
    HeapTuple tuple = (HeapTuple)pgrs_palloc0(fcinfo, sizeof(HeapTupleData));
    if (tuple == NULL) {
        return NULL;
    }
    tuple->kind = PGRS_HEAP_TUPLE_KIND_FORMED;
    tuple->natts = tupdesc->natts;
    if (tuple->natts == 0) {
        return tuple;
    }
    tuple->values = (Datum *)pgrs_palloc0(fcinfo, sizeof(Datum) * (size_t)tuple->natts);
    tuple->isnull = (bool *)pgrs_palloc0(fcinfo, sizeof(bool) * (size_t)tuple->natts);
    if (tuple->values == NULL || tuple->isnull == NULL) {
        return NULL;
    }
    for (int i = 0; i < tuple->natts; i++) {
        bool attr_isnull = isnull != NULL && isnull[i];
        tuple->isnull[i] = attr_isnull;
        tuple->values[i] = attr_isnull || values == NULL ? (Datum)0 : values[i];
    }
    return tuple;
}

static inline void pgrs_heap_deform_tuple(FunctionCallInfo fcinfo, HeapTuple tuple, TupleDesc tupdesc, Datum *values, bool *isnull) {
    if (tupdesc == NULL || tupdesc->natts < 0) {
        return;
    }
    for (int i = 0; i < tupdesc->natts; i++) {
        bool attr_isnull = true;
        Datum value = pgrs_heap_getattr(fcinfo, tuple, i + 1, tupdesc, &attr_isnull);
        if (values != NULL) {
            values[i] = value;
        }
        if (isnull != NULL) {
            isnull[i] = attr_isnull;
        }
    }
}

static inline void pgrs_heap_freetuple(FunctionCallInfo fcinfo, HeapTuple tuple) {
    if (tuple == NULL || tuple->kind != PGRS_HEAP_TUPLE_KIND_FORMED) {
        return;
    }
    if (tuple->values != NULL) {
        pgrs_pfree(fcinfo, tuple->values);
    }
    if (tuple->isnull != NULL) {
        pgrs_pfree(fcinfo, tuple->isnull);
    }
    pgrs_pfree(fcinfo, tuple);
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

static inline bool pgrs_search_syscache_exists1(FunctionCallInfo fcinfo, int cache_id, Datum key1) {
    HeapTuple tuple = pgrs_search_syscache1(fcinfo, cache_id, key1);
    bool found = tuple != NULL;
    if (found) {
        pgrs_release_syscache(fcinfo, tuple);
    }
    return found;
}

static inline bool pgrs_search_syscache_exists2(FunctionCallInfo fcinfo, int cache_id, Datum key1, Datum key2) {
    HeapTuple tuple = pgrs_search_syscache2(fcinfo, cache_id, key1, key2);
    bool found = tuple != NULL;
    if (found) {
        pgrs_release_syscache(fcinfo, tuple);
    }
    return found;
}

static inline Oid pgrs_heap_tuple_get_oid(HeapTuple tuple) {
    if (tuple == NULL || tuple->data == NULL) {
        return InvalidOid;
    }
    return *((Oid *)tuple->data);
}

static inline Oid pgrs_get_syscache_oid1(FunctionCallInfo fcinfo, int cache_id, int oidcol, Datum key1) {
    (void)oidcol;
    HeapTuple tuple = pgrs_search_syscache1(fcinfo, cache_id, key1);
    if (tuple == NULL) {
        return InvalidOid;
    }
    Oid oid = pgrs_heap_tuple_get_oid(tuple);
    pgrs_release_syscache(fcinfo, tuple);
    return oid;
}

static inline Oid pgrs_get_syscache_oid2(FunctionCallInfo fcinfo, int cache_id, int oidcol, Datum key1, Datum key2) {
    (void)oidcol;
    HeapTuple tuple = pgrs_search_syscache2(fcinfo, cache_id, key1, key2);
    if (tuple == NULL) {
        return InvalidOid;
    }
    Oid oid = pgrs_heap_tuple_get_oid(tuple);
    pgrs_release_syscache(fcinfo, tuple);
    return oid;
}

static inline SPIPlanPtr pgrs_spi_prepare(FunctionCallInfo fcinfo, const char *query, int nargs, Oid *argtypes) {
    if (query == NULL) {
        return NULL;
    }
    (void)fcinfo;
    SPIPlanPtr plan = (SPIPlanPtr)calloc(1, sizeof(PgrsSpiPlanData));
    if (plan == NULL) {
        return NULL;
    }
    size_t len = strlen(query) + 1;
    plan->query = (char *)malloc(len);
    if (plan->query == NULL) {
        free(plan);
        return NULL;
    }
    memcpy(plan->query, query, len);
    plan->nargs = nargs < 0 ? 0 : nargs;
    if (plan->nargs > 0 && argtypes != NULL) {
        plan->argtypes = (Oid *)malloc(sizeof(Oid) * (size_t)plan->nargs);
        if (plan->argtypes == NULL) {
            free(plan->query);
            free(plan);
            return NULL;
        }
        memcpy(plan->argtypes, argtypes, sizeof(Oid) * (size_t)plan->nargs);
    }
    return plan;
}

static inline int pgrs_spi_execute_plan(FunctionCallInfo fcinfo, SPIPlanPtr plan, Datum *values, const char *nulls, bool read_only, int64_t count) {
    if (plan == NULL || plan->query == NULL) {
        return -1;
    }
    if (plan->nargs > 0) {
        return pgrs_spi_execute_with_args(fcinfo, plan->query, plan->nargs, plan->argtypes, values, nulls, read_only, count);
    }
    return pgrs_spi_execute(fcinfo, plan->query, read_only, count);
}

static inline void pgrs_spi_freeplan(SPIPlanPtr plan) {
    if (plan == NULL) {
        return;
    }
    free(plan->query);
    free(plan->argtypes);
    free(plan);
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

static inline void *MemoryContextAllocExtended(MemoryContext context, size_t size, int flags) {
    bool zero = (flags & MCXT_ALLOC_ZERO) != 0;
    (void)flags;
    if (context == NULL || context->alloc == NULL) {
        return zero ? calloc(1, size) : malloc(size);
    }
    return context->alloc(context, size, zero);
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

static inline Datum pgrs_function_call(FmgrInfo *flinfo, Oid collation, int nargs, Datum *values) {
    if (flinfo == NULL || nargs < 0 || nargs > PGRS_MAX_FMGR_ARGS) {
        return (Datum)0;
    }
    LOCAL_FCINFO(local_fcinfo, PGRS_MAX_FMGR_ARGS);
    pgrs_init_function_call_info(local_fcinfo, flinfo, (uint16_t)nargs, collation, NULL, NULL);
    for (int i = 0; i < nargs; i++) {
        local_fcinfo->args[i].value = values[i];
        local_fcinfo->args[i].isnull = false;
    }
    return FunctionCallInvoke(local_fcinfo);
}

static inline Datum pgrs_direct_function_call(PGFunction func, Oid collation, int nargs, Datum *values) {
    if (func == NULL || nargs < 0 || nargs > PGRS_MAX_FMGR_ARGS) {
        return (Datum)0;
    }
    FmgrInfo flinfo;
    flinfo.fn_addr = func;
    flinfo.fn_oid = InvalidOid;
    flinfo.fn_nargs = (int16_t)nargs;
    flinfo.fn_strict = false;
    flinfo.fn_retset = false;
    flinfo.fn_stats = 0;
    flinfo.fn_extra = NULL;
    flinfo.fn_mcxt = NULL;
    flinfo.fn_expr = NULL;
    return pgrs_function_call(&flinfo, collation, nargs, values);
}

static inline Datum FunctionCall0Coll(FmgrInfo *flinfo, Oid collation) {
    return pgrs_function_call(flinfo, collation, 0, NULL);
}

static inline Datum FunctionCall1Coll(FmgrInfo *flinfo, Oid collation, Datum arg1) {
    Datum values[1] = {arg1};
    return pgrs_function_call(flinfo, collation, 1, values);
}

static inline Datum FunctionCall2Coll(FmgrInfo *flinfo, Oid collation, Datum arg1, Datum arg2) {
    Datum values[2] = {arg1, arg2};
    return pgrs_function_call(flinfo, collation, 2, values);
}

static inline Datum FunctionCall3Coll(FmgrInfo *flinfo, Oid collation, Datum arg1, Datum arg2, Datum arg3) {
    Datum values[3] = {arg1, arg2, arg3};
    return pgrs_function_call(flinfo, collation, 3, values);
}

static inline Datum FunctionCall4Coll(FmgrInfo *flinfo, Oid collation, Datum arg1, Datum arg2, Datum arg3, Datum arg4) {
    Datum values[4] = {arg1, arg2, arg3, arg4};
    return pgrs_function_call(flinfo, collation, 4, values);
}

static inline Datum FunctionCall5Coll(FmgrInfo *flinfo, Oid collation, Datum arg1, Datum arg2, Datum arg3, Datum arg4, Datum arg5) {
    Datum values[5] = {arg1, arg2, arg3, arg4, arg5};
    return pgrs_function_call(flinfo, collation, 5, values);
}

static inline Datum FunctionCall0(FmgrInfo *flinfo) {
    return FunctionCall0Coll(flinfo, InvalidOid);
}

static inline Datum FunctionCall1(FmgrInfo *flinfo, Datum arg1) {
    return FunctionCall1Coll(flinfo, InvalidOid, arg1);
}

static inline Datum FunctionCall2(FmgrInfo *flinfo, Datum arg1, Datum arg2) {
    return FunctionCall2Coll(flinfo, InvalidOid, arg1, arg2);
}

static inline Datum FunctionCall3(FmgrInfo *flinfo, Datum arg1, Datum arg2, Datum arg3) {
    return FunctionCall3Coll(flinfo, InvalidOid, arg1, arg2, arg3);
}

static inline Datum FunctionCall4(FmgrInfo *flinfo, Datum arg1, Datum arg2, Datum arg3, Datum arg4) {
    return FunctionCall4Coll(flinfo, InvalidOid, arg1, arg2, arg3, arg4);
}

static inline Datum FunctionCall5(FmgrInfo *flinfo, Datum arg1, Datum arg2, Datum arg3, Datum arg4, Datum arg5) {
    return FunctionCall5Coll(flinfo, InvalidOid, arg1, arg2, arg3, arg4, arg5);
}

static inline Datum DirectFunctionCall0Coll(PGFunction func, Oid collation) {
    return pgrs_direct_function_call(func, collation, 0, NULL);
}

static inline Datum DirectFunctionCall1Coll(PGFunction func, Oid collation, Datum arg1) {
    Datum values[1] = {arg1};
    return pgrs_direct_function_call(func, collation, 1, values);
}

static inline Datum DirectFunctionCall2Coll(PGFunction func, Oid collation, Datum arg1, Datum arg2) {
    Datum values[2] = {arg1, arg2};
    return pgrs_direct_function_call(func, collation, 2, values);
}

static inline Datum DirectFunctionCall3Coll(PGFunction func, Oid collation, Datum arg1, Datum arg2, Datum arg3) {
    Datum values[3] = {arg1, arg2, arg3};
    return pgrs_direct_function_call(func, collation, 3, values);
}

static inline Datum DirectFunctionCall4Coll(PGFunction func, Oid collation, Datum arg1, Datum arg2, Datum arg3, Datum arg4) {
    Datum values[4] = {arg1, arg2, arg3, arg4};
    return pgrs_direct_function_call(func, collation, 4, values);
}

static inline Datum DirectFunctionCall5Coll(PGFunction func, Oid collation, Datum arg1, Datum arg2, Datum arg3, Datum arg4, Datum arg5) {
    Datum values[5] = {arg1, arg2, arg3, arg4, arg5};
    return pgrs_direct_function_call(func, collation, 5, values);
}

static inline Datum DirectFunctionCall0(PGFunction func) {
    return DirectFunctionCall0Coll(func, InvalidOid);
}

static inline Datum DirectFunctionCall1(PGFunction func, Datum arg1) {
    return DirectFunctionCall1Coll(func, InvalidOid, arg1);
}

static inline Datum DirectFunctionCall2(PGFunction func, Datum arg1, Datum arg2) {
    return DirectFunctionCall2Coll(func, InvalidOid, arg1, arg2);
}

static inline Datum DirectFunctionCall3(PGFunction func, Datum arg1, Datum arg2, Datum arg3) {
    return DirectFunctionCall3Coll(func, InvalidOid, arg1, arg2, arg3);
}

static inline Datum DirectFunctionCall4(PGFunction func, Datum arg1, Datum arg2, Datum arg3, Datum arg4) {
    return DirectFunctionCall4Coll(func, InvalidOid, arg1, arg2, arg3, arg4);
}

static inline Datum DirectFunctionCall5(PGFunction func, Datum arg1, Datum arg2, Datum arg3, Datum arg4, Datum arg5) {
    return DirectFunctionCall5Coll(func, InvalidOid, arg1, arg2, arg3, arg4, arg5);
}

#define SPI_connect() pgrs_spi_connect(fcinfo)
#define SPI_finish() pgrs_spi_finish(fcinfo)
#define SPI_execute(query, read_only, count) pgrs_spi_execute(fcinfo, query, read_only, count)
#define SPI_exec(query, count) pgrs_spi_execute(fcinfo, query, false, count)
#define SPI_execute_with_args(query, nargs, argtypes, values, nulls, read_only, count) pgrs_spi_execute_with_args(fcinfo, query, nargs, argtypes, values, nulls, read_only, count)
#define SPI_prepare(query, nargs, argtypes) pgrs_spi_prepare(fcinfo, query, nargs, argtypes)
#define SPI_execute_plan(plan, values, nulls, read_only, count) pgrs_spi_execute_plan(fcinfo, plan, values, nulls, read_only, count)
#define SPI_saveplan(plan) (plan)
#define SPI_freeplan(plan) pgrs_spi_freeplan(plan)
#define SPI_getvalue(tuple, tupdesc, fnumber) pgrs_spi_getvalue(fcinfo, tuple, tupdesc, fnumber)
#define SPI_getbinval(tuple, tupdesc, fnumber, isnull) pgrs_spi_getbinval(fcinfo, tuple, tupdesc, fnumber, isnull)
#define SPI_gettypeid(tupdesc, fnumber) pgrs_spi_gettypeid(tupdesc, fnumber)
#define SPI_gettype(tupdesc, fnumber) pgrs_spi_gettype(fcinfo, tupdesc, fnumber)
#define SPI_freetuptable(tuptable) ((void)(tuptable))
#define CurrentMemoryContext (((PgrsExtensionContext *)fcinfo->context)->current_memory_context)
#define MemoryContextSwitchTo(context) pgrs_memory_context_switch_to(fcinfo, context)
#define SearchSysCache1(cache_id, key1) pgrs_search_syscache1(fcinfo, cache_id, key1)
#define SearchSysCache2(cache_id, key1, key2) pgrs_search_syscache2(fcinfo, cache_id, key1, key2)
#define SearchSysCacheExists1(cache_id, key1) pgrs_search_syscache_exists1(fcinfo, cache_id, key1)
#define SearchSysCacheExists2(cache_id, key1, key2) pgrs_search_syscache_exists2(fcinfo, cache_id, key1, key2)
#define GetSysCacheOid1(cache_id, oidcol, key1) pgrs_get_syscache_oid1(fcinfo, cache_id, oidcol, key1)
#define GetSysCacheOid2(cache_id, oidcol, key1, key2) pgrs_get_syscache_oid2(fcinfo, cache_id, oidcol, key1, key2)
#define ReleaseSysCache(tuple) pgrs_release_syscache(fcinfo, tuple)
#define HeapTupleIsValid(tuple) ((tuple) != NULL)
#define GETSTRUCT(tuple) ((tuple)->data)
#define HeapTupleGetOid(tuple) pgrs_heap_tuple_get_oid(tuple)
#define TupleDescAttr(tupdesc, attr_index) pgrs_tuple_desc_attr(tupdesc, attr_index)
#define CreateTemplateTupleDesc(natts) pgrs_create_template_tuple_desc(fcinfo, natts)
#define TupleDescInitEntry(tupdesc, attributeNumber, attributeName, oidtypeid, typmod, attdim) pgrs_tuple_desc_init_entry(fcinfo, tupdesc, attributeNumber, attributeName, oidtypeid, typmod, attdim)
#define BlessTupleDesc(tupdesc) pgrs_bless_tuple_desc(fcinfo, tupdesc)
#define TupleDescCopy(tupdesc) pgrs_tuple_desc_copy(fcinfo, tupdesc)
#define FreeTupleDesc(tupdesc) pgrs_free_tuple_desc(fcinfo, tupdesc)
#define heap_getattr(tuple, fnumber, tupdesc, isnull) pgrs_heap_getattr(fcinfo, tuple, fnumber, tupdesc, isnull)
#define heap_attisnull(tuple, fnumber, tupdesc) pgrs_heap_attisnull(fcinfo, tuple, fnumber, tupdesc)
#define heap_form_tuple(tupdesc, values, isnull) pgrs_heap_form_tuple(fcinfo, tupdesc, values, isnull)
#define heap_deform_tuple(tuple, tupdesc, values, isnull) pgrs_heap_deform_tuple(fcinfo, tuple, tupdesc, values, isnull)
#define heap_freetuple(tuple) pgrs_heap_freetuple(fcinfo, tuple)

#define PG_GETARG_DATUM(n) (fcinfo->args[(n)].value)
#define PG_GETARG_POINTER(n) ((void *)(uintptr_t)PG_GETARG_DATUM(n))
#define PG_GETARG_INT16(n) ((int16_t)PG_GETARG_DATUM(n))
#define PG_GETARG_INT32(n) ((int32_t)PG_GETARG_DATUM(n))
#define PG_GETARG_INT64(n) ((int64_t)PG_GETARG_DATUM(n))
#define PG_GETARG_UINT16(n) ((uint16_t)PG_GETARG_DATUM(n))
#define PG_GETARG_UINT32(n) ((uint32_t)PG_GETARG_DATUM(n))
#define PG_GETARG_UINT64(n) ((uint64_t)PG_GETARG_DATUM(n))
#define PG_GETARG_BOOL(n) (PG_GETARG_DATUM(n) != 0)
#define PG_GETARG_CHAR(n) ((char)PG_GETARG_DATUM(n))
#define PG_GETARG_OID(n) DatumGetObjectId(PG_GETARG_DATUM(n))
#define PG_GETARG_CSTRING(n) DatumGetCString(PG_GETARG_DATUM(n))

#define Int16GetDatum(x) ((Datum)((int16_t)(x)))
#define Int32GetDatum(x) ((Datum)((int32_t)(x)))
#define Int64GetDatum(x) ((Datum)((int64_t)(x)))
#define UInt16GetDatum(x) ((Datum)((uint16_t)(x)))
#define UInt32GetDatum(x) ((Datum)((uint32_t)(x)))
#define UInt64GetDatum(x) ((Datum)((uint64_t)(x)))
#define BoolGetDatum(x) ((Datum)((x) ? 1 : 0))
#define CharGetDatum(x) ((Datum)((char)(x)))
#define OidGetDatum(x) ObjectIdGetDatum(x)
#define DatumGetInt16(x) ((int16_t)(x))
#define DatumGetInt32(x) ((int32_t)(x))
#define DatumGetInt64(x) ((int64_t)(x))
#define DatumGetUInt16(x) ((uint16_t)(x))
#define DatumGetUInt32(x) ((uint32_t)(x))
#define DatumGetUInt64(x) ((uint64_t)(x))
#define DatumGetBool(x) ((bool)((x) != 0))
#define DatumGetChar(x) ((char)(x))
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

#define pstrdup(s) pgrs_pstrdup(fcinfo, s)
#define pnstrdup(s, len) pgrs_pnstrdup(fcinfo, s, len)
#define psprintf(fmt, ...) pgrs_psprintf(fcinfo, fmt, ##__VA_ARGS__)
#define initStringInfo(str) pgrs_init_string_info(fcinfo, str)
#define makeStringInfo() pgrs_make_string_info(fcinfo)
#define resetStringInfo(str) pgrs_reset_string_info(str)
#define enlargeStringInfo(str, needed) pgrs_enlarge_string_info(fcinfo, str, needed)
#define appendBinaryStringInfo(str, data, datalen) pgrs_append_binary_string_info(fcinfo, str, data, datalen)
#define appendStringInfoString(str, s) pgrs_append_string_info_string(fcinfo, str, s)
#define appendStringInfoChar(str, ch) pgrs_append_string_info_char(fcinfo, str, ch)
#define appendStringInfoSpaces(str, count) pgrs_append_string_info_spaces(fcinfo, str, count)
#define appendStringInfo(str, fmt, ...) pgrs_append_string_info(fcinfo, str, fmt, ##__VA_ARGS__)

static inline void namestrcpy(Name name, const char *str) {
    if (name == NULL) {
        return;
    }
    if (str == NULL) {
        name->data[0] = '\0';
        return;
    }
    strncpy(name->data, str, NAMEDATALEN - 1);
    name->data[NAMEDATALEN - 1] = '\0';
}

static inline void namestrncpy(Name name, const char *str, size_t len) {
    if (name == NULL) {
        return;
    }
    size_t copy_len = len < (NAMEDATALEN - 1) ? len : (NAMEDATALEN - 1);
    if (str != NULL && copy_len > 0) {
        memcpy(name->data, str, copy_len);
    }
    name->data[copy_len] = '\0';
}

static inline int namestrcmp(Name name, const char *str) {
    const char *lhs = name == NULL ? "" : name->data;
    const char *rhs = str == NULL ? "" : str;
    return strcmp(lhs, rhs);
}

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
typedef varlena bytea;
typedef varlena Jsonb;
typedef varlena pg_uuid_t;

#define VARHDRSZ ((int32_t)sizeof(uint32_t))
#define PG_GETARG_VARLENA_PP(n) ((varlena *)PG_GETARG_DATUM(n))
#define PG_GETARG_VARLENA_P(n) PG_GETARG_VARLENA_PP(n)
#define PG_GETARG_TEXT_PP(n) ((varlena *)PG_GETARG_DATUM(n))
#define PG_GETARG_TEXT_P(n) PG_GETARG_TEXT_PP(n)
#define PG_GETARG_BYTEA_PP(n) ((bytea *)PG_GETARG_DATUM(n))
#define PG_GETARG_BYTEA_P(n) PG_GETARG_BYTEA_PP(n)
#define PG_GETARG_JSONB_P(n) ((Jsonb *)PG_GETARG_DATUM(n))
#define PG_GETARG_UUID_P(n) ((pg_uuid_t *)PG_GETARG_DATUM(n))
#define VARDATA_ANY(vlena) ((char *)((vlena)->data))
#define VARDATA(vlena) VARDATA_ANY(vlena)
#define VARSIZE_ANY(vlena) ((size_t)((vlena)->len))
#define VARSIZE(vlena) VARSIZE_ANY(vlena)
#define VARSIZE_ANY_EXHDR(vlena) (VARSIZE_ANY(vlena) - sizeof(uint32_t))
#define VARSIZE_EXHDR(vlena) VARSIZE_ANY_EXHDR(vlena)
#define SET_VARSIZE(vlena, size) ((vlena)->len = (uint32_t)(size))
#define DatumGetVarLenaP(x) ((varlena *)DatumGetPointer(x))
#define DatumGetVarLenaPP(x) DatumGetVarLenaP(x)
#define VarLenaPGetDatum(x) PointerGetDatum(x)
#define DatumGetTextP(x) ((text *)DatumGetPointer(x))
#define DatumGetTextPP(x) DatumGetTextP(x)
#define TextPGetDatum(x) PointerGetDatum(x)
#define DatumGetByteaP(x) ((bytea *)DatumGetPointer(x))
#define DatumGetByteaPP(x) DatumGetByteaP(x)
#define ByteaPGetDatum(x) PointerGetDatum(x)
#define DatumGetJsonbP(x) ((Jsonb *)DatumGetPointer(x))
#define JsonbPGetDatum(x) PointerGetDatum(x)
#define DatumGetUUIDP(x) ((pg_uuid_t *)DatumGetPointer(x))
#define UUIDPGetDatum(x) PointerGetDatum(x)

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

static inline void *pgrs_palloc_extended(FunctionCallInfo fcinfo, size_t size, int flags) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    bool zero = (flags & MCXT_ALLOC_ZERO) != 0;
    if (ctx == NULL) {
        return zero ? calloc(1, size) : malloc(size);
    }
    return MemoryContextAllocExtended(ctx->current_memory_context, size, flags);
}

static inline void *pgrs_repalloc(FunctionCallInfo fcinfo, void *ptr, size_t size) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL) {
        return realloc(ptr, size);
    }
    return pgrs_memory_context_realloc(ctx->current_memory_context, ptr, size);
}

static inline void *pgrs_repalloc0(FunctionCallInfo fcinfo, void *ptr, size_t oldsize, size_t size) {
    void *newptr = pgrs_repalloc(fcinfo, ptr, size);
    if (newptr != NULL && size > oldsize) {
        memset((char *)newptr + oldsize, 0, size - oldsize);
    }
    return newptr;
}

static inline void pgrs_pfree(FunctionCallInfo fcinfo, void *ptr) {
    PgrsExtensionContext *ctx = (PgrsExtensionContext *)fcinfo->context;
    if (ctx == NULL) {
        free(ptr);
        return;
    }
    pgrs_memory_context_free(ctx->current_memory_context, ptr);
}

static inline char *pgrs_pnstrdup(FunctionCallInfo fcinfo, const char *s, size_t len) {
    char *out = (char *)pgrs_palloc(fcinfo, len + 1);
    if (out == NULL) {
        return NULL;
    }
    if (s != NULL && len > 0) {
        memcpy(out, s, len);
    }
    out[len] = '\0';
    return out;
}

static inline char *pgrs_pstrdup(FunctionCallInfo fcinfo, const char *s) {
    if (s == NULL) {
        return NULL;
    }
    return pgrs_pnstrdup(fcinfo, s, strlen(s));
}

static inline varlena *pgrs_detoast_datum_copy(FunctionCallInfo fcinfo, Datum datum) {
    varlena *input = DatumGetVarLenaP(datum);
    if (input == NULL) {
        return NULL;
    }
    size_t size = VARSIZE_ANY(input);
    varlena *copy = (varlena *)pgrs_palloc(fcinfo, size);
    if (copy == NULL) {
        return NULL;
    }
    memcpy(copy, input, size);
    return copy;
}

static inline char *pgrs_psprintf(FunctionCallInfo fcinfo, const char *fmt, ...) {
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

static inline bool pgrs_enlarge_string_info(FunctionCallInfo fcinfo, StringInfo str, int needed) {
    if (str == NULL || needed < 0) {
        return false;
    }
    if (str->data == NULL) {
        str->maxlen = 1024;
        while (str->maxlen <= needed) {
            str->maxlen *= 2;
        }
        str->data = (char *)pgrs_palloc(fcinfo, (size_t)str->maxlen);
        if (str->data == NULL) {
            str->maxlen = 0;
            return false;
        }
        str->len = 0;
        str->cursor = 0;
        str->data[0] = '\0';
        return true;
    }
    if (needed <= str->maxlen - str->len - 1) {
        return true;
    }
    int newlen = str->maxlen <= 0 ? 1024 : str->maxlen;
    while (needed > newlen - str->len - 1) {
        newlen *= 2;
    }
    char *newdata = (char *)pgrs_repalloc(fcinfo, str->data, (size_t)newlen);
    if (newdata == NULL) {
        return false;
    }
    str->data = newdata;
    str->maxlen = newlen;
    return true;
}

static inline void pgrs_init_string_info(FunctionCallInfo fcinfo, StringInfo str) {
    if (str == NULL) {
        return;
    }
    str->data = NULL;
    str->len = 0;
    str->maxlen = 0;
    str->cursor = 0;
    (void)pgrs_enlarge_string_info(fcinfo, str, 0);
}

static inline StringInfo pgrs_make_string_info(FunctionCallInfo fcinfo) {
    StringInfo str = (StringInfo)pgrs_palloc0(fcinfo, sizeof(StringInfoData));
    if (str != NULL) {
        pgrs_init_string_info(fcinfo, str);
    }
    return str;
}

static inline void pgrs_reset_string_info(StringInfo str) {
    if (str == NULL || str->data == NULL) {
        return;
    }
    str->len = 0;
    str->cursor = 0;
    str->data[0] = '\0';
}

static inline void pgrs_append_binary_string_info(FunctionCallInfo fcinfo, StringInfo str, const char *data, int datalen) {
    if (str == NULL || data == NULL || datalen <= 0) {
        return;
    }
    if (!pgrs_enlarge_string_info(fcinfo, str, datalen)) {
        return;
    }
    memcpy(str->data + str->len, data, (size_t)datalen);
    str->len += datalen;
    str->data[str->len] = '\0';
}

static inline void pgrs_append_string_info_string(FunctionCallInfo fcinfo, StringInfo str, const char *s) {
    if (s == NULL) {
        return;
    }
    pgrs_append_binary_string_info(fcinfo, str, s, (int)strlen(s));
}

static inline void pgrs_append_string_info_char(FunctionCallInfo fcinfo, StringInfo str, char ch) {
    if (str == NULL) {
        return;
    }
    if (!pgrs_enlarge_string_info(fcinfo, str, 1)) {
        return;
    }
    str->data[str->len++] = ch;
    str->data[str->len] = '\0';
}

static inline void pgrs_append_string_info_spaces(FunctionCallInfo fcinfo, StringInfo str, int count) {
    for (int i = 0; i < count; i++) {
        pgrs_append_string_info_char(fcinfo, str, ' ');
    }
}

static inline void pgrs_append_string_info(FunctionCallInfo fcinfo, StringInfo str, const char *fmt, ...) {
    if (str == NULL || fmt == NULL) {
        return;
    }
    va_list args;
    va_start(args, fmt);
    va_list copy;
    va_copy(copy, args);
    int needed = vsnprintf(NULL, 0, fmt, copy);
    va_end(copy);
    if (needed < 0) {
        va_end(args);
        return;
    }
    if (pgrs_enlarge_string_info(fcinfo, str, needed)) {
        vsnprintf(str->data + str->len, (size_t)needed + 1, fmt, args);
        str->len += needed;
    }
    va_end(args);
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

static inline char *pgrs_spi_gettype(FunctionCallInfo fcinfo, TupleDesc tupdesc, int fnumber) {
    Oid typid = pgrs_spi_gettypeid(tupdesc, fnumber);
    if (typid == InvalidOid) {
        return NULL;
    }
    return pgrs_format_type_be(fcinfo, typid);
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

static inline char *pgrs_get_namespace_name(FunctionCallInfo fcinfo, Oid nspid) {
    HeapTuple tuple = SearchSysCache1(NAMESPACEOID, ObjectIdGetDatum(nspid));
    if (!HeapTupleIsValid(tuple)) {
        return NULL;
    }
    Form_pg_namespace form = (Form_pg_namespace)GETSTRUCT(tuple);
    char *out = MemoryContextStrdup(CurrentMemoryContext, form->nspname);
    ReleaseSysCache(tuple);
    return out;
}

static inline Oid pgrs_get_namespace_oid(FunctionCallInfo fcinfo, const char *nspname, bool missing_ok) {
    (void)missing_ok;
    HeapTuple tuple = SearchSysCache1(NAMESPACENAME, CStringGetDatum(nspname));
    if (!HeapTupleIsValid(tuple)) {
        return InvalidOid;
    }
    Form_pg_namespace form = (Form_pg_namespace)GETSTRUCT(tuple);
    Oid nspid = form->oid;
    ReleaseSysCache(tuple);
    return nspid;
}

#define get_typlenbyvalalign(typid, typlen, typbyval, typalign) pgrs_get_typlenbyvalalign(fcinfo, typid, typlen, typbyval, typalign)
#define get_typlen(typid) pgrs_get_typlen(fcinfo, typid)
#define get_typbyval(typid) pgrs_get_typbyval(fcinfo, typid)
#define get_typalign(typid) pgrs_get_typalign(fcinfo, typid)
#define format_type_be(typid) pgrs_format_type_be(fcinfo, typid)
#define get_func_name(funcid) pgrs_get_func_name(fcinfo, funcid)
#define get_func_rettype(funcid) pgrs_get_func_rettype(fcinfo, funcid)
#define get_func_nargs(funcid) pgrs_get_func_nargs(fcinfo, funcid)
#define get_namespace_name(nspid) pgrs_get_namespace_name(fcinfo, nspid)
#define get_namespace_oid(nspname, missing_ok) pgrs_get_namespace_oid(fcinfo, nspname, missing_ok)

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

static inline text *pgrs_cstring_to_text_with_len(FunctionCallInfo fcinfo, const char *s, int len) {
    size_t out_len = len < 0 ? 0 : (size_t)len;
    text *out = (text *)pgrs_palloc(fcinfo, VARHDRSZ + out_len);
    SET_VARSIZE(out, VARHDRSZ + out_len);
    if (s != NULL && out_len > 0) {
        memcpy(VARDATA(out), s, out_len);
    }
    return out;
}

static inline void pgrs_text_to_cstring_buffer(const text *src, char *dst, size_t dst_len) {
    if (dst == NULL || dst_len == 0) {
        return;
    }
    if (src == NULL) {
        dst[0] = '\0';
        return;
    }
    size_t len = VARSIZE_ANY_EXHDR(src);
    size_t copy_len = len < (dst_len - 1) ? len : (dst_len - 1);
    memcpy(dst, VARDATA_ANY(src), copy_len);
    dst[copy_len] = '\0';
}

#define palloc(size) pgrs_palloc(fcinfo, size)
#define palloc0(size) pgrs_palloc0(fcinfo, size)
#define palloc_extended(size, flags) pgrs_palloc_extended(fcinfo, size, flags)
#define repalloc(ptr, size) pgrs_repalloc(fcinfo, ptr, size)
#define repalloc0(ptr, oldsize, size) pgrs_repalloc0(fcinfo, ptr, oldsize, size)
#define pfree(ptr) pgrs_pfree(fcinfo, ptr)
#define PG_DETOAST_DATUM(datum) DatumGetVarLenaP(datum)
#define PG_DETOAST_DATUM_COPY(datum) pgrs_detoast_datum_copy(fcinfo, datum)
#define cstring_to_text(s) pgrs_cstring_to_text(fcinfo, s)
#define cstring_to_text_with_len(s, len) pgrs_cstring_to_text_with_len(fcinfo, s, len)
#define text_to_cstring(t) pgrs_text_to_cstring(fcinfo, t)
#define text_to_cstring_buffer(src, dst, dst_len) pgrs_text_to_cstring_buffer(src, dst, dst_len)

#define PG_RETURN_NULL()          \
    do {                          \
        fcinfo->isnull = true;    \
        return (Datum)0;          \
    } while (0)

#define PG_RETURN_DATUM(x) return (Datum)(x)
#define PG_RETURN_INT16(x) return (Datum)((int16_t)(x))
#define PG_RETURN_INT32(x) return (Datum)((int32_t)(x))
#define PG_RETURN_INT64(x) return (Datum)((int64_t)(x))
#define PG_RETURN_UINT16(x) return UInt16GetDatum(x)
#define PG_RETURN_UINT32(x) return UInt32GetDatum(x)
#define PG_RETURN_UINT64(x) return UInt64GetDatum(x)
#define PG_RETURN_BOOL(x) return (Datum)((x) ? 1 : 0)
#define PG_RETURN_CHAR(x) return CharGetDatum(x)
#define PG_RETURN_OID(x) return ObjectIdGetDatum(x)
#define PG_RETURN_CSTRING(x) return CStringGetDatum(x)
#define PG_RETURN_POINTER(x) return (Datum)(uintptr_t)(x)
#define PG_RETURN_VARLENA_P(x) PG_RETURN_POINTER(x)
#define PG_RETURN_TEXT_P(x) PG_RETURN_POINTER(x)
#define PG_RETURN_BYTEA_P(x) PG_RETURN_POINTER(x)
#define PG_RETURN_JSONB_P(x) PG_RETURN_POINTER(x)
#define PG_RETURN_UUID_P(x) PG_RETURN_POINTER(x)

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
