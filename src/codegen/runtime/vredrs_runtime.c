/* ============================================================================
 * Vredrs runtime library — C implementation of dynamic containers and builtins.
 *
 * Compiled by `vredrs build` (via clang) and linked with the generated LLVM
 * IR. Provides:
 *   - Heap-allocated string / list / dict / tuple / set with simple refcounting
 *   - Tagged union %vredrs.value = { i8 tag, i64 payload } for dynamic values
 *   - Object instances with dynamic field table (string -> vredrs_value)
 *   - Class vtables (set up by the generated IR; this file only allocates)
 *   - All standard-library builtin functions: len, str, int, range, print, ...
 *   - File I/O helpers: open / read / write / close
 *   - Exception state for setjmp/longjmp based throw/catch
 *   - Coroutine state machine helpers
 * ==========================================================================*/

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <stdint.h>
#include <setjmp.h>
#include <stdarg.h>
#include <math.h>   /* math.h — required for sqrt/pow/sin/cos/etc. used by the native math builtins. */
#include <time.h>   /* time.h — for clock() used by debug.timeit native helpers. */
#include <sys/stat.h> /* sys/stat.h — for struct stat used by is_file/is_dir. */
#include <unistd.h>   /* unistd.h — for access/read/write syscalls. */
#include <ctype.h>    /* ctype.h — for toupper/tolower used by str.upper/str.lower. */

/* --------------------------------------------------------------------------
 * Public type declarations. All struct bodies are defined here at the top so
 * the tagged-value helpers can access their fields directly.
 * ------------------------------------------------------------------------ */
struct vredrs_str;
struct vredrs_list;
struct vredrs_dict;
struct vredrs_tuple;
struct vredrs_set;
struct vredrs_object;
struct vredrs_vtable;
struct vredrs_coro;
struct vredrs_closure;

typedef struct vredrs_str     vredrs_str;
typedef struct vredrs_list    vredrs_list;
typedef struct vredrs_dict    vredrs_dict;
typedef struct vredrs_tuple   vredrs_tuple;
typedef struct vredrs_set     vredrs_set;
typedef struct vredrs_object  vredrs_object;
typedef struct vredrs_vtable  vredrs_vtable;
typedef struct vredrs_coro    vredrs_coro;
typedef struct vredrs_closure vredrs_closure;

/* --------------------------------------------------------------------------
 * Tagged value: { i8 tag, i64 payload }. Used as the element type for
 * containers (list, dict, tuple, set) so that reads recover the original
 * type without IR-level type inference.
 *
 * Tag constants:
 *   0 = nil
 *   1 = i64
 *   2 = f64 (payload is the bit pattern)
 *   3 = bool (payload is 0 or 1)
 *   4 = str (payload is vredrs_str*)
 *   5 = list
 *   6 = dict
 *   7 = tuple
 *   8 = object
 *   9 = coroutine
 * ------------------------------------------------------------------------ */
typedef struct vredrs_value { int8_t tag; int64_t payload; } vredrs_value;
/* Auto-generated forward declarations (non-static only). */
vredrs_str* vredrs_str_new(const char *src, int64_t len);
vredrs_str* vredrs_str_from_cstr(const char *src);
void vredrs_str_free(vredrs_str *s);
const char* vredrs_str_data(vredrs_str *s);
int64_t vredrs_str_len(vredrs_str *s);
vredrs_str* vredrs_str_concat(vredrs_str *a, vredrs_str *b);
int64_t vredrs_str_eq(vredrs_str *a, vredrs_str *b);
vredrs_str* vredrs_str_repeat(vredrs_str *s, int64_t n);
vredrs_list* vredrs_list_repeat(vredrs_list *l, int64_t n);
vredrs_str* vredrs_str_from_i64(int64_t v);
vredrs_str* vredrs_str_from_f64(double v);
vredrs_str* vredrs_str_from_bool(int64_t b);
vredrs_value vredrs_value_make_i64(int64_t v);
vredrs_value vredrs_value_make_f64(double v);
vredrs_value vredrs_value_make_bool(int64_t b);
vredrs_value vredrs_value_make_str(vredrs_str *s);
vredrs_value vredrs_value_make_list(vredrs_list *l);
vredrs_value vredrs_value_make_dict(vredrs_dict *d);
vredrs_value vredrs_value_make_tuple(vredrs_tuple *t);
vredrs_value vredrs_value_make_obj(vredrs_object *o);
vredrs_value vredrs_value_make_coro(vredrs_coro *c);
vredrs_value vredrs_value_nil(void);
int64_t vredrs_value_tag(vredrs_value v);
int64_t vredrs_value_get_i64(vredrs_value v);
double vredrs_value_get_f64(vredrs_value v);
int64_t vredrs_value_get_bool(vredrs_value v);
vredrs_str* vredrs_value_get_str(vredrs_value v);
vredrs_list* vredrs_value_get_list(vredrs_value v);
vredrs_dict* vredrs_value_get_dict(vredrs_value v);
vredrs_tuple* vredrs_value_get_tuple(vredrs_value v);
vredrs_object* vredrs_value_get_obj(vredrs_value v);
vredrs_str* vredrs_value_to_str(vredrs_value v);
void vredrs_value_print(vredrs_value v);
void vredrs_value_print_fast(vredrs_value v);
int64_t vredrs_value_truthy(vredrs_value v);
int64_t vredrs_value_eq(vredrs_value a, vredrs_value b);
vredrs_value vredrs_value_add(vredrs_value a, vredrs_value b);
vredrs_value vredrs_value_sub(vredrs_value a, vredrs_value b);
vredrs_value vredrs_value_mul(vredrs_value a, vredrs_value b);
vredrs_value vredrs_value_div(vredrs_value a, vredrs_value b);
vredrs_value vredrs_value_mod(vredrs_value a, vredrs_value b);
int64_t vredrs_value_lt(vredrs_value a, vredrs_value b);
int64_t vredrs_value_gt(vredrs_value a, vredrs_value b);
int64_t vredrs_value_le(vredrs_value a, vredrs_value b);
int64_t vredrs_value_ge(vredrs_value a, vredrs_value b);
int64_t vredrs_value_ne(vredrs_value a, vredrs_value b);
vredrs_list* vredrs_list_new(void);
void vredrs_list_free(vredrs_list *l);
int64_t vredrs_list_len(vredrs_list *l);
void vredrs_list_reserve(vredrs_list *l, int64_t need);
void vredrs_list_push(vredrs_list *l, vredrs_value v);
vredrs_value vredrs_list_get(vredrs_list *l, int64_t i);
void vredrs_list_set(vredrs_list *l, int64_t i, vredrs_value v);
vredrs_value vredrs_list_pop(vredrs_list *l);
int64_t vredrs_list_contains(vredrs_list *l, vredrs_value v);
vredrs_list* vredrs_list_slice(vredrs_list *l, int64_t start, int64_t end);
vredrs_list* vredrs_list_slice_full(vredrs_list *l,
                                    int64_t start, int64_t end, int64_t step);
vredrs_str* vredrs_str_slice_full(vredrs_str *s,
                                  int64_t start, int64_t end, int64_t step);
static int64_t vredrs_dict_hash(vredrs_str *k);
vredrs_dict* vredrs_dict_new(void);
void vredrs_dict_free(vredrs_dict *d);
int64_t vredrs_dict_len(vredrs_dict *d);
static int64_t vredrs_dict_lookup(vredrs_dict *d, vredrs_str *k);
static void vredrs_dict_grow(vredrs_dict *d);
void vredrs_dict_set(vredrs_dict *d, vredrs_str *k, vredrs_value v);
vredrs_value vredrs_dict_get(vredrs_dict *d, vredrs_str *k, vredrs_value dflt);
int64_t vredrs_dict_has(vredrs_dict *d, vredrs_str *k);
void vredrs_dict_del(vredrs_dict *d, vredrs_str *k);
vredrs_list* vredrs_dict_keys(vredrs_dict *d);
vredrs_list* vredrs_dict_values(vredrs_dict *d);
vredrs_tuple* vredrs_tuple_new(int64_t n);
void vredrs_tuple_set_init(vredrs_tuple *t, int64_t i, vredrs_value v);
vredrs_value vredrs_tuple_get(vredrs_tuple *t, int64_t i);
int64_t vredrs_tuple_len(vredrs_tuple *t);
void vredrs_tuple_free(vredrs_tuple *t);
vredrs_dict* vredrs_set_new(void);
void vredrs_set_add(vredrs_dict *s, vredrs_str *k);
int64_t vredrs_set_has(vredrs_dict *s, vredrs_str *k);
int64_t vredrs_set_len(vredrs_dict *s);
vredrs_object* vredrs_object_new(vredrs_vtable *vt);
void vredrs_object_free(vredrs_object *o);
void vredrs_object_set_field(vredrs_object *o, vredrs_str *k, vredrs_value v);
vredrs_value vredrs_object_get_field(vredrs_object *o, vredrs_str *k, vredrs_value dflt);
int64_t vredrs_object_has_field(vredrs_object *o, vredrs_str *k);
void vredrs_object_del_field(vredrs_object *o, vredrs_str *k);
void* vredrs_object_get_method(vredrs_object *o, int64_t index);
void vredrs_inc_ref(vredrs_object *o);
void vredrs_dec_ref(vredrs_object *o);
vredrs_vtable* vredrs_object_vtable(vredrs_object *o);
vredrs_vtable* vredrs_vtable_parent(vredrs_vtable *vt);
vredrs_coro* vredrs_coro_alloc(void);
int64_t vredrs_coro_result(vredrs_coro *c);
void vredrs_coro_store_result(vredrs_coro *c, int64_t v);
void vredrs_coro_set_state(vredrs_coro *c, int64_t s);
int64_t vredrs_coro_state(vredrs_coro *c);
void vredrs_set_jmp_top(void *buf);
void* vredrs_get_jmp_top(void);
int64_t vredrs_get_exception_i64(void);
vredrs_str* vredrs_get_exception_str(void);
void vredrs_clear_exception(void);
void* vredrs_try_begin(void);
int32_t vredrs_try_setjmp(void *buf);
void vredrs_try_end(void *buf);
void vredrs_throw_i64(int64_t v);
void vredrs_throw_str(vredrs_str *s);
int32_t vredrs_setjmp_impl(void *buf);
void vredrs_longjmp_impl(void *buf, int32_t val);
int64_t vredrs_object_is_frozen(vredrs_object *o);
void vredrs_object_freeze(vredrs_object *o);
void vredrs_object_set_field_checked(vredrs_object *o, vredrs_str *k, vredrs_value v);
int64_t vredrs_len_str(vredrs_str *s);
int64_t vredrs_len_list(vredrs_list *l);
int64_t vredrs_len_dict(vredrs_dict *d);
int64_t vredrs_len_tuple(vredrs_tuple *t);
int64_t vredrs_len_set(vredrs_dict *s);
vredrs_str* vredrs_str_of_i64(int64_t v);
vredrs_str* vredrs_str_of_f64(double v);
vredrs_str* vredrs_str_of_bool(int64_t b);
vredrs_str* vredrs_str_of_str(vredrs_str *s);
vredrs_str* vredrs_str_of_ptr(void *p);
int64_t vredrs_int_of_str(vredrs_str *s);
int64_t vredrs_int_of_f64(double v);
int64_t vredrs_int_of_bool(int64_t b);
double vredrs_float_of_str(vredrs_str *s);
double vredrs_float_of_i64(int64_t v);
int64_t vredrs_bool_of_str(vredrs_str *s);
vredrs_list* vredrs_range(int64_t start, int64_t end);
vredrs_list* vredrs_range1(int64_t end);
vredrs_list* vredrs_enumerate(vredrs_list *l);
vredrs_list* vredrs_zip(vredrs_list *a, vredrs_list *b);
int64_t vredrs_sum(vredrs_list *l);
int64_t vredrs_min(vredrs_list *l);
int64_t vredrs_max(vredrs_list *l);
static int vredrs_cmp_value_asc(const void *a, const void *b);
vredrs_list* vredrs_sorted(vredrs_list *l);
vredrs_list* vredrs_reversed(vredrs_list *l);
int64_t vredrs_print_str(vredrs_str *s);
int64_t vredrs_print_i64(int64_t v);
int64_t vredrs_print_f64(double v);
int64_t vredrs_print_bool(int64_t b);
int64_t vredrs_print_cstr(const char *s);
int64_t vredrs_println(void);
int64_t vredrs_print_space(void);
int64_t vredrs_print_value(vredrs_value v);
vredrs_str* vredrs_input(vredrs_str *prompt);
int64_t vredrs_open(vredrs_str *path, vredrs_str *mode);
void vredrs_close(int64_t handle);
vredrs_str* vredrs_read(int64_t handle, int64_t n);
int64_t vredrs_write(int64_t handle, vredrs_str *s);
vredrs_str* vredrs_read_file(vredrs_str *path);
int64_t vredrs_write_file(vredrs_str *path, vredrs_str *content);
int64_t vredrs_file_exists(vredrs_str *path);
void vredrs_exit(int64_t code);
void vredrs_abort(const char *msg);
vredrs_list* vredrs_coro_get_list(vredrs_coro *c);
void vredrs_coro_set_list(vredrs_coro *c, vredrs_list *l);
int64_t vredrs_coro_get_index(vredrs_coro *c);
void vredrs_coro_set_index(vredrs_coro *c, int64_t idx);
void vredrs_value_free(vredrs_value v);
void vredrs_value_fprint(FILE *f, vredrs_value v);
const char* vredrs_value_class_name(vredrs_value v);
vredrs_value vredrs_value_index(vredrs_value container, vredrs_value idx);
vredrs_list* vredrs_list_concat(vredrs_list *a, vredrs_list *b);
vredrs_list* vredrs_sorted_desc(vredrs_list *l);
double vredrs_math_radians(double deg);
double vredrs_math_degrees(double rad);
void vredrs_sleep(int64_t ms);
void vredrs_assert_fail(void);
vredrs_str* vredrs_os_cwd(void);
vredrs_str* vredrs_os_get_env(vredrs_str *name);
int64_t vredrs_is_file(vredrs_str *path);
int64_t vredrs_is_dir(vredrs_str *path);
int64_t vredrs_os_mkdir(vredrs_str *path);
vredrs_list* vredrs_os_args(void);
vredrs_str* vredrs_split_helper(vredrs_str *s, vredrs_str *sep);
vredrs_str* vredrs_trim(vredrs_str *s);
vredrs_str* vredrs_upper(vredrs_str *s);
vredrs_str* vredrs_lower(vredrs_str *s);
int64_t vredrs_contains(vredrs_str *haystack, vredrs_str *needle);
vredrs_str* vredrs_path_join(vredrs_str *a, vredrs_str *b);
vredrs_str* vredrs_path_dirname(vredrs_str *p);
vredrs_str* vredrs_path_basename(vredrs_str *p);
vredrs_str* vredrs_path_ext(vredrs_str *p);
int64_t vredrs_path_exists(vredrs_str *p);
int64_t vredrs_path_is_abs(vredrs_str *p);
vredrs_str* vredrs_path_abs(vredrs_str *p);


/* --------------------------------------------------------------------------
 * String: { i64 len; i8* data } — data is owned, NUL-terminated for printf.
 * ------------------------------------------------------------------------ */
struct vredrs_str {
    int64_t len;
    char   *data;
};

/* --------------------------------------------------------------------------
 * List: contiguous vredrs_value array with capacity doubling.
 * ------------------------------------------------------------------------ */
struct vredrs_list {
    int64_t        len;
    int64_t        cap;
    vredrs_value  *data;
};

/* --------------------------------------------------------------------------
 * Dict: linear-probing hash map keyed by vredrs_str*. Values are vredrs_value.
 * ------------------------------------------------------------------------ */
struct vredrs_dict {
    int64_t        len;
    int64_t        cap;
    vredrs_str   **keys;
    vredrs_value  *vals;
};

/* --------------------------------------------------------------------------
 * Tuple: immutable list-like. Stores vredrs_value elements.
 * ------------------------------------------------------------------------ */
struct vredrs_tuple {
    int64_t        len;
    vredrs_value  *data;
};

/* --------------------------------------------------------------------------
 * Object: vtable* + dynamic field table (string -> vredrs_value) + refcount.
 * ------------------------------------------------------------------------ */
struct vredrs_vtable {
    int64_t method_count;
    void  **methods;
    struct  vredrs_vtable *parent;
    const char *class_name;
};

struct vredrs_object {
    vredrs_vtable *vt;
    vredrs_dict    *fields;
    int64_t         rc;
};

/* --------------------------------------------------------------------------
 * Coroutine state machine storage.
 * ------------------------------------------------------------------------ */
struct vredrs_coro {
    int64_t state;
    int64_t result;
    void   *impl;
    int64_t gen_index;
};

/* --------------------------------------------------------------------------
 * String operations
 * ------------------------------------------------------------------------ */
vredrs_str *vredrs_str_new(const char *src, int64_t len) {
    vredrs_str *s = (vredrs_str *)malloc(sizeof(vredrs_str));
    if (!s) { fputs("vredrs: out of memory (str header)\n", stderr); exit(70); }
    if (len < 0) len = 0;
    if (src && len == 0) len = (int64_t)strlen(src);
    s->len = len;
    s->data = (char *)malloc((size_t)len + 1);
    if (!s->data) { fputs("vredrs: out of memory (str body)\n", stderr); exit(70); }
    if (src) memcpy(s->data, src, (size_t)len);
    s->data[len] = '\0';
    return s;
}

vredrs_str *vredrs_str_from_cstr(const char *src) {
    return vredrs_str_new(src, src ? (int64_t)strlen(src) : 0);
}

void vredrs_str_free(vredrs_str *s) {
    if (!s) return;
    free(s->data);
    free(s);
}

const char *vredrs_str_data(vredrs_str *s) { return s ? s->data : ""; }
int64_t     vredrs_str_len(vredrs_str *s)  { return s ? s->len : 0; }

vredrs_str *vredrs_str_concat(vredrs_str *a, vredrs_str *b) {
    int64_t la = a ? a->len : 0;
    int64_t lb = b ? b->len : 0;
    vredrs_str *r = (vredrs_str *)malloc(sizeof(vredrs_str));
    r->len = la + lb;
    r->data = (char *)malloc((size_t)(la + lb) + 1);
    if (a && la) memcpy(r->data, a->data, (size_t)la);
    if (b && lb) memcpy(r->data + la, b->data, (size_t)lb);
    r->data[la + lb] = '\0';
    return r;
}

int64_t vredrs_str_eq(vredrs_str *a, vredrs_str *b) {
    if (a == b) return 1;
    if (!a || !b) return 0;
    if (a->len != b->len) return 0;
    return memcmp(a->data, b->data, (size_t)a->len) == 0 ? 1 : 0;
}

/* Repeat string `s` exactly `n` times. `n <= 0` returns an empty string. */
vredrs_str *vredrs_str_repeat(vredrs_str *s, int64_t n) {
    int64_t slen = s ? s->len : 0;
    if (n <= 0 || slen <= 0) {
        return vredrs_str_new("", 0);
    }
    int64_t out_len = slen * n;
    vredrs_str *r = (vredrs_str *)malloc(sizeof(vredrs_str));
    r->len = out_len;
    r->data = (char *)malloc((size_t)out_len + 1);
    for (int64_t i = 0; i < n; i++) {
        memcpy(r->data + i * slen, s->data, (size_t)slen);
    }
    r->data[out_len] = '\0';
    return r;
}

/* Repeat list `l` exactly `n` times, producing a new list whose length
   is `l->len * n`. `n <= 0` returns an empty list. */
vredrs_list *vredrs_list_repeat(vredrs_list *l, int64_t n) {
    vredrs_list *out = vredrs_list_new();
    if (n <= 0 || !l || l->len <= 0) return out;
    for (int64_t i = 0; i < n; i++) {
        for (int64_t j = 0; j < l->len; j++) {
            vredrs_list_push(out, l->data[j]);
        }
    }
    return out;
}

vredrs_str *vredrs_str_from_i64(int64_t v) {
    char buf[32];
    int n = snprintf(buf, sizeof(buf), "%lld", (long long)v);
    return vredrs_str_new(buf, n);
}

vredrs_str *vredrs_str_from_f64(double v) {
    char buf[64];
    int n;
    if (v == (double)(int64_t)v && v >= -1e15 && v <= 1e15) {
        n = snprintf(buf, sizeof(buf), "%lld", (long long)v);
    } else {
        n = snprintf(buf, sizeof(buf), "%g", v);
    }
    return vredrs_str_new(buf, n);
}

vredrs_str *vredrs_str_from_bool(int64_t b) {
    return vredrs_str_from_cstr(b ? "true" : "false");
}

/* --------------------------------------------------------------------------
 * Tagged value constructors & accessors
 * ------------------------------------------------------------------------ */
vredrs_value vredrs_value_make_i64(int64_t v)        { vredrs_value r; r.tag = 1; r.payload = v; return r; }
vredrs_value vredrs_value_make_f64(double v)         { vredrs_value r; r.tag = 2; int64_t bits; memcpy(&bits, &v, 8); r.payload = bits; return r; }
vredrs_value vredrs_value_make_bool(int64_t b)       { vredrs_value r; r.tag = 3; r.payload = b ? 1 : 0; return r; }
vredrs_value vredrs_value_make_str(vredrs_str *s)    { vredrs_value r; r.tag = 4; r.payload = (int64_t)(intptr_t)s; return r; }
vredrs_value vredrs_value_make_list(vredrs_list *l)  { vredrs_value r; r.tag = 5; r.payload = (int64_t)(intptr_t)l; return r; }
vredrs_value vredrs_value_make_dict(vredrs_dict *d)  { vredrs_value r; r.tag = 6; r.payload = (int64_t)(intptr_t)d; return r; }
vredrs_value vredrs_value_make_tuple(vredrs_tuple *t){ vredrs_value r; r.tag = 7; r.payload = (int64_t)(intptr_t)t; return r; }
vredrs_value vredrs_value_make_obj(vredrs_object *o) { vredrs_value r; r.tag = 8; r.payload = (int64_t)(intptr_t)o; return r; }
vredrs_value vredrs_value_make_coro(vredrs_coro *c)  { vredrs_value r; r.tag = 9; r.payload = (int64_t)(intptr_t)c; return r; }
vredrs_value vredrs_value_nil(void)                  { vredrs_value r; r.tag = 0; r.payload = 0; return r; }

int64_t      vredrs_value_tag(vredrs_value v)         { return v.tag; }
int64_t      vredrs_value_get_i64(vredrs_value v)     { return v.payload; }
double       vredrs_value_get_f64(vredrs_value v)     { double d; int64_t b = v.payload; memcpy(&d, &b, 8); return d; }
int64_t      vredrs_value_get_bool(vredrs_value v)    { return v.payload != 0 ? 1 : 0; }
vredrs_str  *vredrs_value_get_str(vredrs_value v)     { return (vredrs_str *)(intptr_t)v.payload; }
vredrs_list *vredrs_value_get_list(vredrs_value v)    { return (vredrs_list *)(intptr_t)v.payload; }
vredrs_dict *vredrs_value_get_dict(vredrs_value v)    { return (vredrs_dict *)(intptr_t)v.payload; }
vredrs_tuple*vredrs_value_get_tuple(vredrs_value v)   { return (vredrs_tuple *)(intptr_t)v.payload; }
vredrs_object* vredrs_value_get_obj(vredrs_value v)   { return (vredrs_object *)(intptr_t)v.payload; }

vredrs_str *vredrs_value_to_str(vredrs_value v) {
    switch (v.tag) {
        case 0: return vredrs_str_from_cstr("null");
        case 1: return vredrs_str_from_i64(v.payload);
        case 2: { double d; int64_t b = v.payload; memcpy(&d, &b, 8); return vredrs_str_from_f64(d); }
        case 3: return vredrs_str_from_bool(v.payload);
        case 4: return (vredrs_str *)(intptr_t)v.payload;
        case 5: return vredrs_str_from_cstr("<list>");
        case 6: return vredrs_str_from_cstr("<dict>");
        case 7: return vredrs_str_from_cstr("<tuple>");
        case 8: return vredrs_str_from_cstr("<object>");
        case 9: return vredrs_str_from_cstr("<coroutine>");
        default: return vredrs_str_from_cstr("<unknown>");
    }
}

void vredrs_value_print(vredrs_value v) {
    vredrs_str *s = vredrs_value_to_str(v);
    if (s) {
        fputs(s->data, stdout);
        vredrs_str_free(s);
    }
}

/* Print a tagged value without allocating a vredrs_str wrapper.
   This is the hot path for `paste`/`println` with primitive args. */
void vredrs_value_print_fast(vredrs_value v) {
    switch (v.tag) {
        case 0: fputs("null", stdout); break;
        case 1: printf("%lld", (long long)v.payload); break;
        case 2: {
            double d; int64_t b = v.payload; memcpy(&d, &b, 8);
            if (d == (double)(int64_t)d && d >= -1e15 && d <= 1e15)
                printf("%lld", (long long)d);
            else
                printf("%g", d);
            break;
        }
        case 3: fputs(v.payload ? "true" : "false", stdout); break;
        case 4: {
            vredrs_str *s = (vredrs_str *)(intptr_t)v.payload;
            if (s) fputs(s->data, stdout);
            break;
        }
        case 5: fputs("<list>", stdout); break;
        case 6: fputs("<dict>", stdout); break;
        case 7: fputs("<tuple>", stdout); break;
        case 8: fputs("<object>", stdout); break;
        case 9: fputs("<coroutine>", stdout); break;
        default: fputs("?", stdout); break;
    }
}

int64_t vredrs_value_truthy(vredrs_value v) {
    switch (v.tag) {
        case 0: return 0;
        case 1: return v.payload != 0 ? 1 : 0;
        case 2: { double d; int64_t b = v.payload; memcpy(&d, &b, 8); return d != 0.0 ? 1 : 0; }
        case 3: return v.payload != 0 ? 1 : 0;
        case 4: return v.payload != 0 ? 1 : 0;
        case 5: { vredrs_list *l = (vredrs_list *)(intptr_t)v.payload; return (l && l->len > 0) ? 1 : 0; }
        case 6: { vredrs_dict *d = (vredrs_dict *)(intptr_t)v.payload; return (d && d->len > 0) ? 1 : 0; }
        case 7: { vredrs_tuple *t = (vredrs_tuple *)(intptr_t)v.payload; return (t && t->len > 0) ? 1 : 0; }
        case 8: return v.payload != 0 ? 1 : 0;
        default: return v.payload != 0 ? 1 : 0;
    }
}

int64_t vredrs_value_eq(vredrs_value a, vredrs_value b) {
    if (a.tag != b.tag) return 0;
    if (a.tag == 4) return vredrs_str_eq((vredrs_str *)(intptr_t)a.payload, (vredrs_str *)(intptr_t)b.payload);
    return a.payload == b.payload ? 1 : 0;
}

vredrs_value vredrs_value_add(vredrs_value a, vredrs_value b) {
    if (a.tag == 1 && b.tag == 1) return vredrs_value_make_i64(a.payload + b.payload);
    if (a.tag == 2 && b.tag == 2) return vredrs_value_make_f64(vredrs_value_get_f64(a) + vredrs_value_get_f64(b));
    if (a.tag == 4 && b.tag == 4) return vredrs_value_make_str(vredrs_str_concat((vredrs_str *)(intptr_t)a.payload, (vredrs_str *)(intptr_t)b.payload));
    return vredrs_value_make_i64(a.payload + b.payload);
}
vredrs_value vredrs_value_sub(vredrs_value a, vredrs_value b) {
    if (a.tag == 1 && b.tag == 1) return vredrs_value_make_i64(a.payload - b.payload);
    if (a.tag == 2 && b.tag == 2) return vredrs_value_make_f64(vredrs_value_get_f64(a) - vredrs_value_get_f64(b));
    return vredrs_value_make_i64(a.payload - b.payload);
}
vredrs_value vredrs_value_mul(vredrs_value a, vredrs_value b) {
    if (a.tag == 1 && b.tag == 1) return vredrs_value_make_i64(a.payload * b.payload);
    if (a.tag == 2 && b.tag == 2) return vredrs_value_make_f64(vredrs_value_get_f64(a) * vredrs_value_get_f64(b));
    /* string * int: repeat the string N times (both directions) */
    if (a.tag == 4 && b.tag == 1) {
        vredrs_str* s = (vredrs_str*)(intptr_t)a.payload;
        int64_t n = b.payload;
        if (n <= 0) return vredrs_value_make_str(vredrs_str_new("", 0));
        size_t len = s->len * n;
        char* buf = malloc(len + 1);
        buf[0] = '\0';
        for (int64_t i = 0; i < n; i++) {
            memcpy(buf + i * s->len, s->data, s->len);
        }
        buf[len] = '\0';
        vredrs_value r = vredrs_value_make_str(vredrs_str_new(buf, len));
        free(buf);
        return r;
    }
    if (a.tag == 1 && b.tag == 4) {
        return vredrs_value_mul(b, a); /* symmetric */
    }
    return vredrs_value_make_i64(a.payload * b.payload);
}
vredrs_value vredrs_value_div(vredrs_value a, vredrs_value b) {
    if (a.tag == 1 && b.tag == 1) {
        if (b.payload == 0) { fputs("vredrs: division by zero\n", stderr); exit(70); }
        return vredrs_value_make_i64(a.payload / b.payload);
    }
    if (a.tag == 2 && b.tag == 2) return vredrs_value_make_f64(vredrs_value_get_f64(a) / vredrs_value_get_f64(b));
    return vredrs_value_make_i64(a.payload / b.payload);
}
vredrs_value vredrs_value_mod(vredrs_value a, vredrs_value b) {
    if (a.tag == 2 && b.tag == 2) {
        double av = vredrs_value_get_f64(a);
        double bv = vredrs_value_get_f64(b);
        if (bv == 0.0) { fputs("vredrs: modulo by zero\n", stderr); exit(70); }
        return vredrs_value_make_f64(fmod(av, bv));
    }
    if (b.payload == 0) { fputs("vredrs: modulo by zero\n", stderr); exit(70); }
    return vredrs_value_make_i64(a.payload % b.payload);
}
int64_t vredrs_value_lt(vredrs_value a, vredrs_value b) {
    if (a.tag == 2 || b.tag == 2) return vredrs_value_get_f64(a) < vredrs_value_get_f64(b) ? 1 : 0;
    return a.payload < b.payload ? 1 : 0;
}
int64_t vredrs_value_gt(vredrs_value a, vredrs_value b) {
    if (a.tag == 2 || b.tag == 2) return vredrs_value_get_f64(a) > vredrs_value_get_f64(b) ? 1 : 0;
    return a.payload > b.payload ? 1 : 0;
}
int64_t vredrs_value_le(vredrs_value a, vredrs_value b) { return !vredrs_value_gt(a, b); }
int64_t vredrs_value_ge(vredrs_value a, vredrs_value b) { return !vredrs_value_lt(a, b); }
int64_t vredrs_value_ne(vredrs_value a, vredrs_value b) { return vredrs_value_eq(a, b) ? 0 : 1; }

/* --------------------------------------------------------------------------
 * List operations
 * ------------------------------------------------------------------------ */
vredrs_list *vredrs_list_new(void) {
    vredrs_list *l = (vredrs_list *)malloc(sizeof(vredrs_list));
    l->len = 0; l->cap = 8;
    l->data = (vredrs_value *)calloc((size_t)l->cap, sizeof(vredrs_value));
    return l;
}
void vredrs_list_free(vredrs_list *l) { if (!l) return; free(l->data); free(l); }
int64_t vredrs_list_len(vredrs_list *l) { return l ? l->len : 0; }
void vredrs_list_reserve(vredrs_list *l, int64_t need) {
    if (l->cap >= need) return;
    while (l->cap < need) l->cap *= 2;
    vredrs_value *nd = (vredrs_value *)realloc(l->data, (size_t)l->cap * sizeof(vredrs_value));
    if (!nd) { fputs("vredrs: out of memory (list grow)\n", stderr); exit(70); }
    l->data = nd;
}
void vredrs_list_push(vredrs_list *l, vredrs_value v) {
    if (!l) return;
    vredrs_list_reserve(l, l->len + 1);
    l->data[l->len++] = v;
}
/* vredrs_list_append is an alias for vredrs_list_push (append to end). */
void vredrs_list_append(vredrs_list *l, vredrs_value v) {
    vredrs_list_push(l, v);
}
vredrs_value vredrs_list_get(vredrs_list *l, int64_t i) {
    if (!l || i < 0 || i >= l->len) { fputs("vredrs: list index out of range\n", stderr); exit(70); }
    return l->data[i];
}
void vredrs_list_set(vredrs_list *l, int64_t i, vredrs_value v) {
    if (!l || i < 0 || i >= l->len) { fputs("vredrs: list index out of range\n", stderr); exit(70); }
    l->data[i] = v;
}
vredrs_value vredrs_list_pop(vredrs_list *l) {
    if (!l || l->len == 0) { fputs("vredrs: pop from empty list\n", stderr); exit(70); }
    return l->data[--l->len];
}
int64_t vredrs_list_contains(vredrs_list *l, vredrs_value v) {
    if (!l) return 0;
    for (int64_t i = 0; i < l->len; i++) if (vredrs_value_eq(l->data[i], v)) return 1;
    return 0;
}
vredrs_list *vredrs_list_slice(vredrs_list *l, int64_t start, int64_t end) {
    if (!l) return vredrs_list_new();
    if (start < 0) start = 0;
    if (end > l->len) end = l->len;
    if (end < start) end = start;
    vredrs_list *r = vredrs_list_new();
    for (int64_t i = start; i < end; i++) vredrs_list_push(r, l->data[i]);
    return r;
}

/* Full Python-style slice with start/end/step, all optional.
 * Sentinel value INT64_MIN means "absent" (None in Vredrs source).
 * Negative indices count from the end; negative step reverses. */
vredrs_list *vredrs_list_slice_full(vredrs_list *l,
                                    int64_t start, int64_t end, int64_t step) {
    if (!l) return vredrs_list_new();
    int64_t n = l->len;
    if (step == 0) step = 1;
    if (step > 0) {
        int64_t def_start = 0;
        int64_t def_end = n;
        if (start == INT64_MIN) start = def_start;
        else if (start < 0) { start += n; if (start < 0) start = 0; }
        else if (start > n) start = n;
        if (end == INT64_MIN) end = def_end;
        else if (end < 0) { end += n; if (end < 0) end = 0; }
        else if (end > n) end = n;
        vredrs_list *r = vredrs_list_new();
        for (int64_t i = start; i < end; i += step) vredrs_list_push(r, l->data[i]);
        return r;
    } else {
        int64_t def_start = n - 1;
        int64_t def_end = -1;
        if (start == INT64_MIN) start = def_start;
        else if (start < 0) { start += n; if (start < 0) start = -1; }
        else if (start >= n) start = n - 1;
        if (end == INT64_MIN) end = def_end;
        else if (end < 0) { end += n; if (end < 0) end = -1; }
        else if (end >= n) end = n - 1;
        vredrs_list *r = vredrs_list_new();
        for (int64_t i = start; i > end; i += step) {
            if (i < 0 || i >= n) break;
            vredrs_list_push(r, l->data[i]);
        }
        return r;
    }
}

/* String slice with same semantics as list_slice_full. */
vredrs_str *vredrs_str_slice_full(vredrs_str *s,
                                  int64_t start, int64_t end, int64_t step) {
    if (!s) return vredrs_str_from_cstr("");
    int64_t n = s->len;
    if (step == 0) step = 1;
    int64_t out_len = 0;
    /* Compute output length first. */
    if (step > 0) {
        int64_t st = start, en = end;
        if (st == INT64_MIN) st = 0;
        else if (st < 0) { st += n; if (st < 0) st = 0; }
        else if (st > n) st = n;
        if (en == INT64_MIN) en = n;
        else if (en < 0) { en += n; if (en < 0) en = 0; }
        else if (en > n) en = n;
        if (en > st) out_len = (en - st + step - 1) / step;
    } else {
        int64_t st = start, en = end;
        if (st == INT64_MIN) st = n - 1;
        else if (st < 0) { st += n; if (st < 0) st = -1; }
        else if (st >= n) st = n - 1;
        if (en == INT64_MIN) en = -1;
        else if (en < 0) { en += n; if (en < 0) en = -1; }
        else if (en >= n) en = n - 1;
        if (st > en) out_len = (st - en + (-step) - 1) / (-step);
    }
    if (out_len < 0) out_len = 0;
    char *buf = (char *)malloc((size_t)out_len + 1);
    int64_t j = 0;
    if (step > 0) {
        int64_t st = start, en = end;
        if (st == INT64_MIN) st = 0;
        else if (st < 0) { st += n; if (st < 0) st = 0; }
        else if (st > n) st = n;
        if (en == INT64_MIN) en = n;
        else if (en < 0) { en += n; if (en < 0) en = 0; }
        else if (en > n) en = n;
        for (int64_t i = st; i < en; i += step) buf[j++] = s->data[i];
    } else {
        int64_t st = start, en = end;
        if (st == INT64_MIN) st = n - 1;
        else if (st < 0) { st += n; if (st < 0) st = -1; }
        else if (st >= n) st = n - 1;
        if (en == INT64_MIN) en = -1;
        else if (en < 0) { en += n; if (en < 0) en = -1; }
        else if (en >= n) en = n - 1;
        for (int64_t i = st; i > en; i += step) {
            if (i < 0 || i >= n) break;
            buf[j++] = s->data[i];
        }
    }
    buf[j] = '\0';
    vredrs_str *r = vredrs_str_new(buf, j);
    free(buf);
    return r;
}

/* --------------------------------------------------------------------------
 * Dict operations
 * ------------------------------------------------------------------------ */
static int64_t vredrs_dict_hash(vredrs_str *k) {
    /* FNV-1a hash by string content so different allocations of the same
       key text land in the same bucket. */
    if (!k) return 0;
    int64_t h = 1469598103934665603ULL;
    for (int64_t i = 0; i < k->len; i++) {
        h ^= (int64_t)(unsigned char)k->data[i];
        h *= 1099511628211ULL;
    }
    return h < 0 ? -h : h;
}
vredrs_dict *vredrs_dict_new(void) {
    vredrs_dict *d = (vredrs_dict *)malloc(sizeof(vredrs_dict));
    d->len = 0; d->cap = 16;
    d->keys = (vredrs_str **)calloc((size_t)d->cap, sizeof(vredrs_str *));
    d->vals = (vredrs_value *)calloc((size_t)d->cap, sizeof(vredrs_value));
    return d;
}
void vredrs_dict_free(vredrs_dict *d) { if (!d) return; free(d->keys); free(d->vals); free(d); }
int64_t vredrs_dict_len(vredrs_dict *d) { return d ? d->len : 0; }
static int64_t vredrs_dict_lookup(vredrs_dict *d, vredrs_str *k) {
    if (!d || !k) return -1;
    int64_t mask = d->cap - 1;
    int64_t h = vredrs_dict_hash(k) & mask;
    for (int64_t i = 0; i < d->cap; i++) {
        int64_t slot = (h + i) & mask;
        if (d->keys[slot] == NULL) return -1;
        if (d->keys[slot] == k || vredrs_str_eq(d->keys[slot], k)) return slot;
    }
    return -1;
}
static void vredrs_dict_grow(vredrs_dict *d) {
    int64_t old_cap = d->cap;
    vredrs_str **old_keys = d->keys;
    vredrs_value *old_vals = d->vals;
    d->cap *= 2;
    d->keys = (vredrs_str **)calloc((size_t)d->cap, sizeof(vredrs_str *));
    d->vals = (vredrs_value *)calloc((size_t)d->cap, sizeof(vredrs_value));
    d->len = 0;
    for (int64_t i = 0; i < old_cap; i++) {
        if (old_keys[i]) {
            int64_t mask = d->cap - 1;
            int64_t h = vredrs_dict_hash(old_keys[i]) & mask;
            for (int64_t j = 0; j < d->cap; j++) {
                int64_t slot = (h + j) & mask;
                if (d->keys[slot] == NULL) {
                    d->keys[slot] = old_keys[i];
                    d->vals[slot] = old_vals[i];
                    d->len++;
                    break;
                }
            }
        }
    }
    free(old_keys); free(old_vals);
}
void vredrs_dict_set(vredrs_dict *d, vredrs_str *k, vredrs_value v) {
    if (!d || !k) return;
    int64_t slot = vredrs_dict_lookup(d, k);
    if (slot >= 0) { d->vals[slot] = v; return; }
    if ((d->len + 1) * 2 > d->cap) vredrs_dict_grow(d);
    int64_t mask = d->cap - 1;
    int64_t h = vredrs_dict_hash(k) & mask;
    for (int64_t i = 0; i < d->cap; i++) {
        int64_t s = (h + i) & mask;
        if (d->keys[s] == NULL) {
            d->keys[s] = k;
            d->vals[s] = v;
            d->len++;
            return;
        }
    }
    fputs("vredrs: dict insertion failed (table full)\n", stderr);
    exit(70);
}
vredrs_value vredrs_dict_get(vredrs_dict *d, vredrs_str *k, vredrs_value dflt) {
    int64_t slot = vredrs_dict_lookup(d, k);
    return slot >= 0 ? d->vals[slot] : dflt;
}
int64_t vredrs_dict_has(vredrs_dict *d, vredrs_str *k) {
    return vredrs_dict_lookup(d, k) >= 0 ? 1 : 0;
}
void vredrs_dict_del(vredrs_dict *d, vredrs_str *k) {
    int64_t slot = vredrs_dict_lookup(d, k);
    if (slot < 0) return;
    d->keys[slot] = NULL;
    d->len--;
    int64_t mask = d->cap - 1;
    for (int64_t i = 1; i < d->cap; i++) {
        int64_t s = (slot + i) & mask;
        if (d->keys[s] == NULL) break;
        vredrs_str *kk = d->keys[s];
        vredrs_value vv = d->vals[s];
        d->keys[s] = NULL;
        d->len--;
        vredrs_dict_set(d, kk, vv);
    }
}
vredrs_list *vredrs_dict_keys(vredrs_dict *d) {
    vredrs_list *r = vredrs_list_new();
    if (!d) return r;
    for (int64_t i = 0; i < d->cap; i++) {
        if (d->keys[i]) vredrs_list_push(r, vredrs_value_make_str(d->keys[i]));
    }
    return r;
}
vredrs_list *vredrs_dict_values(vredrs_dict *d) {
    vredrs_list *r = vredrs_list_new();
    if (!d) return r;
    for (int64_t i = 0; i < d->cap; i++) {
        if (d->keys[i]) vredrs_list_push(r, d->vals[i]);
    }
    return r;
}

/* --------------------------------------------------------------------------
 * Tuple operations
 * ------------------------------------------------------------------------ */
vredrs_tuple *vredrs_tuple_new(int64_t n) {
    vredrs_tuple *t = (vredrs_tuple *)malloc(sizeof(vredrs_tuple));
    t->len = n;
    t->data = (vredrs_value *)calloc((size_t)(n > 0 ? n : 1), sizeof(vredrs_value));
    return t;
}
void vredrs_tuple_set_init(vredrs_tuple *t, int64_t i, vredrs_value v) {
    if (i < 0 || i >= t->len) return;
    t->data[i] = v;
}
vredrs_value vredrs_tuple_get(vredrs_tuple *t, int64_t i) {
    if (!t || i < 0 || i >= t->len) { fputs("vredrs: tuple index out of range\n", stderr); exit(70); }
    return t->data[i];
}
int64_t vredrs_tuple_len(vredrs_tuple *t) { return t ? t->len : 0; }
void vredrs_tuple_free(vredrs_tuple *t) { if (!t) return; free(t->data); free(t); }

/* --------------------------------------------------------------------------
 * Set operations (thin wrapper around dict)
 * ------------------------------------------------------------------------ */
vredrs_dict *vredrs_set_new(void) { return vredrs_dict_new(); }
void vredrs_set_add(vredrs_dict *s, vredrs_str *k) { vredrs_dict_set(s, k, vredrs_value_make_bool(1)); }
int64_t vredrs_set_has(vredrs_dict *s, vredrs_str *k) { return vredrs_dict_has(s, k); }
int64_t vredrs_set_len(vredrs_dict *s) { return vredrs_dict_len(s); }

/* --------------------------------------------------------------------------
 * Object operations
 * ------------------------------------------------------------------------ */
vredrs_object *vredrs_object_new(vredrs_vtable *vt) {
    vredrs_object *o = (vredrs_object *)malloc(sizeof(vredrs_object));
    o->vt = vt;
    o->fields = vredrs_dict_new();
    o->rc = 1;
    return o;
}
void vredrs_object_free(vredrs_object *o) {
    if (!o) return;
    vredrs_dict_free(o->fields);
    free(o);
}
void vredrs_object_set_field(vredrs_object *o, vredrs_str *k, vredrs_value v) {
    if (!o || !k) return;
    vredrs_dict_set(o->fields, k, v);
}
vredrs_value vredrs_object_get_field(vredrs_object *o, vredrs_str *k, vredrs_value dflt) {
    if (!o || !k) return dflt;
    return vredrs_dict_get(o->fields, k, dflt);
}
int64_t vredrs_object_has_field(vredrs_object *o, vredrs_str *k) {
    if (!o || !k) return 0;
    return vredrs_dict_has(o->fields, k);
}
void vredrs_object_del_field(vredrs_object *o, vredrs_str *k) {
    if (!o || !k) return;
    vredrs_dict_del(o->fields, k);
}
void *vredrs_object_get_method(vredrs_object *o, int64_t index) {
    if (!o || !o->vt) return NULL;
    vredrs_vtable *vt = o->vt;
    while (vt) {
        if (index < vt->method_count) return vt->methods[index];
        index -= vt->method_count;
        vt = vt->parent;
    }
    return NULL;
}
void vredrs_inc_ref(vredrs_object *o) { if (o) o->rc++; }
void vredrs_dec_ref(vredrs_object *o) {
    if (!o) return;
    o->rc--;
    if (o->rc <= 0) vredrs_object_free(o);
}
vredrs_vtable *vredrs_object_vtable(vredrs_object *o) { return o ? o->vt : NULL; }
vredrs_vtable *vredrs_vtable_parent(vredrs_vtable *vt) { return vt ? vt->parent : NULL; }

/* --------------------------------------------------------------------------
 * Coroutine operations
 * ------------------------------------------------------------------------ */
vredrs_coro *vredrs_coro_alloc(void) {
    vredrs_coro *c = (vredrs_coro *)malloc(sizeof(vredrs_coro));
    c->state = 0; c->result = 0; c->impl = NULL; c->gen_index = 0;
    return c;
}
int64_t vredrs_coro_result(vredrs_coro *c) { return c ? c->result : 0; }
void    vredrs_coro_store_result(vredrs_coro *c, int64_t v) { if (c) c->result = v; }
void    vredrs_coro_set_state(vredrs_coro *c, int64_t s) { if (c) c->state = s; }
int64_t vredrs_coro_state(vredrs_coro *c) { return c ? c->state : 0; }

/* --------------------------------------------------------------------------
 * Exception state (setjmp/longjmp based)
 * ------------------------------------------------------------------------ */
static jmp_buf *vredrs_jmp_top = NULL;
static int64_t  vredrs_exception_i64 = 0;
static vredrs_str *vredrs_exception_str = NULL;

void vredrs_set_jmp_top(void *buf) { vredrs_jmp_top = (jmp_buf *)buf; }
void *vredrs_get_jmp_top(void) { return (void *)vredrs_jmp_top; }
int64_t vredrs_get_exception_i64(void) { return vredrs_exception_i64; }
vredrs_str *vredrs_get_exception_str(void) { return vredrs_exception_str; }
void vredrs_clear_exception(void) {
    vredrs_exception_i64 = 0;
    vredrs_exception_str = NULL;
}

/* Heap-allocated jmp_buf lifecycle. The IR uses these instead of stack
   alloca so the buffer survives across the longjmp and isn't optimised
   away under -O2. */
void *vredrs_try_begin(void) {
    jmp_buf *buf = (jmp_buf *)malloc(sizeof(jmp_buf));
    return (void *)buf;
}
int32_t vredrs_try_setjmp(void *buf) {
    vredrs_jmp_top = (jmp_buf *)buf;
    return (int32_t)setjmp(*(jmp_buf *)buf);
}
void vredrs_try_end(void *buf) {
    if (buf) free(buf);
}

void vredrs_throw_i64(int64_t v) {
    vredrs_exception_i64 = v;
    if (vredrs_jmp_top) longjmp(*vredrs_jmp_top, 1);
    fprintf(stderr, "vredrs: uncaught throw: %lld\n", (long long)v);
    exit(70);
}
void vredrs_throw_str(vredrs_str *s) {
    vredrs_exception_str = s;
    vredrs_exception_i64 = s ? (int64_t)(intptr_t)s : 0;
    if (vredrs_jmp_top) longjmp(*vredrs_jmp_top, 1);
    fprintf(stderr, "vredrs: uncaught throw: %s\n", s ? s->data : "(null)");
    exit(70);
}

int32_t vredrs_setjmp_impl(void *buf) { return (int32_t)setjmp(*(jmp_buf *)buf); }
void vredrs_longjmp_impl(void *buf, int32_t val) { longjmp(*(jmp_buf *)buf, val); }

/* Object freeze support: frozen objects refuse field mutation. */
int64_t vredrs_object_is_frozen(vredrs_object *o) {
    if (!o) return 0;
    return (o->rc & 0x8000000000000000ULL) ? 1 : 0;
}
void vredrs_object_freeze(vredrs_object *o) {
    if (!o) return;
    o->rc |= 0x8000000000000000ULL;
}
void vredrs_object_set_field_checked(vredrs_object *o, vredrs_str *k, vredrs_value v) {
    if (!o || !k) return;
    if (vredrs_object_is_frozen(o)) {
        fputs("vredrs: cannot modify frozen object\n", stderr);
        exit(70);
    }
    vredrs_dict_set(o->fields, k, v);
}

/* --------------------------------------------------------------------------
 * Standard library — builtins callable from generated IR
 * ------------------------------------------------------------------------ */
int64_t vredrs_len_str(vredrs_str *s)    { return vredrs_str_len(s); }
int64_t vredrs_len_list(vredrs_list *l)   { return vredrs_list_len(l); }
int64_t vredrs_len_dict(vredrs_dict *d)   { return vredrs_dict_len(d); }
int64_t vredrs_len_tuple(vredrs_tuple *t) { return vredrs_tuple_len(t); }
int64_t vredrs_len_set(vredrs_dict *s)    { return vredrs_set_len(s); }

vredrs_str *vredrs_str_of_i64(int64_t v)      { return vredrs_str_from_i64(v); }
vredrs_str *vredrs_str_of_f64(double v)       { return vredrs_str_from_f64(v); }
vredrs_str *vredrs_str_of_bool(int64_t b)     { return vredrs_str_from_bool(b); }
vredrs_str *vredrs_str_of_str(vredrs_str *s)  { return s ? s : vredrs_str_from_cstr(""); }
vredrs_str *vredrs_str_of_ptr(void *p) {
    if (!p) return vredrs_str_from_cstr("null");
    char buf[32];
    snprintf(buf, sizeof(buf), "<%p>", p);
    return vredrs_str_from_cstr(buf);
}

int64_t vredrs_int_of_str(vredrs_str *s) {
    if (!s || !s->data) return 0;
    return (int64_t)strtoll(s->data, NULL, 10);
}
int64_t vredrs_int_of_f64(double v) { return (int64_t)v; }
int64_t vredrs_int_of_bool(int64_t b) { return b ? 1 : 0; }
double  vredrs_float_of_str(vredrs_str *s) {
    if (!s || !s->data) return 0.0;
    return strtod(s->data, NULL);
}
double  vredrs_float_of_i64(int64_t v) { return (double)v; }
int64_t vredrs_bool_of_str(vredrs_str *s) { return (s && s->len > 0) ? 1 : 0; }

vredrs_list *vredrs_range(int64_t start, int64_t end) {
    vredrs_list *l = vredrs_list_new();
    if (end > start) {
        vredrs_list_reserve(l, end - start);
        for (int64_t i = start; i < end; i++) vredrs_list_push(l, vredrs_value_make_i64(i));
    }
    return l;
}
vredrs_list *vredrs_range1(int64_t end) { return vredrs_range(0, end); }

vredrs_list *vredrs_enumerate(vredrs_list *l) {
    vredrs_list *r = vredrs_list_new();
    if (!l) return r;
    for (int64_t i = 0; i < l->len; i++) {
        vredrs_tuple *t = vredrs_tuple_new(2);
        vredrs_tuple_set_init(t, 0, vredrs_value_make_i64(i));
        vredrs_tuple_set_init(t, 1, l->data[i]);
        vredrs_list_push(r, vredrs_value_make_tuple(t));
    }
    return r;
}
vredrs_list *vredrs_zip(vredrs_list *a, vredrs_list *b) {
    vredrs_list *r = vredrs_list_new();
    if (!a || !b) return r;
    int64_t n = a->len < b->len ? a->len : b->len;
    for (int64_t i = 0; i < n; i++) {
        vredrs_tuple *t = vredrs_tuple_new(2);
        vredrs_tuple_set_init(t, 0, a->data[i]);
        vredrs_tuple_set_init(t, 1, b->data[i]);
        vredrs_list_push(r, vredrs_value_make_tuple(t));
    }
    return r;
}

int64_t vredrs_sum(vredrs_list *l) {
    int64_t s = 0;
    if (l) for (int64_t i = 0; i < l->len; i++) s += vredrs_value_get_i64(l->data[i]);
    return s;
}
int64_t vredrs_min(vredrs_list *l) {
    if (!l || l->len == 0) return 0;
    int64_t m = vredrs_value_get_i64(l->data[0]);
    for (int64_t i = 1; i < l->len; i++) { int64_t v = vredrs_value_get_i64(l->data[i]); if (v < m) m = v; }
    return m;
}
int64_t vredrs_max(vredrs_list *l) {
    if (!l || l->len == 0) return 0;
    int64_t m = vredrs_value_get_i64(l->data[0]);
    for (int64_t i = 1; i < l->len; i++) { int64_t v = vredrs_value_get_i64(l->data[i]); if (v > m) m = v; }
    return m;
}
static int vredrs_cmp_value_asc(const void *a, const void *b) {
    int64_t x = vredrs_value_get_i64(*(const vredrs_value *)a);
    int64_t y = vredrs_value_get_i64(*(const vredrs_value *)b);
    return (x > y) - (x < y);
}
vredrs_list *vredrs_sorted(vredrs_list *l) {
    vredrs_list *r = vredrs_list_new();
    if (!l) return r;
    vredrs_list_reserve(r, l->len);
    for (int64_t i = 0; i < l->len; i++) vredrs_list_push(r, l->data[i]);
    qsort(r->data, (size_t)r->len, sizeof(vredrs_value), vredrs_cmp_value_asc);
    return r;
}
vredrs_list *vredrs_reversed(vredrs_list *l) {
    vredrs_list *r = vredrs_list_new();
    if (!l) return r;
    vredrs_list_reserve(r, l->len);
    for (int64_t i = l->len - 1; i >= 0; i--) vredrs_list_push(r, l->data[i]);
    return r;
}

/* --------------------------------------------------------------------------
 * print helpers
 * ------------------------------------------------------------------------ */
int64_t vredrs_print_str(vredrs_str *s) { if (s) fputs(s->data, stdout); return 0; }
int64_t vredrs_print_i64(int64_t v) { printf("%lld", (long long)v); return 0; }
int64_t vredrs_print_f64(double v) {
    if (v == (double)(int64_t)v && v >= -1e15 && v <= 1e15) printf("%lld", (long long)v);
    else printf("%g", v);
    return 0;
}
int64_t vredrs_print_bool(int64_t b) { fputs(b ? "true" : "false", stdout); return 0; }
int64_t vredrs_print_cstr(const char *s) { if (s) fputs(s, stdout); return 0; }
int64_t vredrs_println(void) { fputc('\n', stdout); return 0; }
int64_t vredrs_print_space(void) { fputc(' ', stdout); return 0; }
int64_t vredrs_print_value(vredrs_value v) { vredrs_value_print(v); return 0; }

vredrs_str *vredrs_input(vredrs_str *prompt) {
    if (prompt) fputs(prompt->data, stdout);
    fflush(stdout);
    char *line = NULL; size_t cap = 0;
    ssize_t n = getline(&line, &cap, stdin);
    if (n < 0) { free(line); return vredrs_str_from_cstr(""); }
    while (n > 0 && (line[n-1] == '\n' || line[n-1] == '\r')) line[--n] = '\0';
    vredrs_str *r = vredrs_str_new(line, n);
    free(line);
    return r;
}

/* --------------------------------------------------------------------------
 * File I/O
 * ------------------------------------------------------------------------ */
int64_t vredrs_open(vredrs_str *path, vredrs_str *mode) {
    if (!path || !mode) return 0;
    FILE *f = fopen(path->data, mode->data[0] == 'w' ? "wb" : "rb");
    return f ? (int64_t)(intptr_t)f : 0;
}
void vredrs_close(int64_t handle) { if (handle) fclose((FILE *)(intptr_t)handle); }
vredrs_str *vredrs_read(int64_t handle, int64_t n) {
    FILE *f = (FILE *)(intptr_t)handle;
    if (!f) return vredrs_str_from_cstr("");
    if (n < 0) {
        fseek(f, 0, SEEK_END);
        long sz = ftell(f);
        fseek(f, 0, SEEK_SET);
        if (sz < 0) sz = 0;
        n = (int64_t)sz;
    }
    char *buf = (char *)malloc((size_t)n + 1);
    int64_t got = (int64_t)fread(buf, 1, (size_t)n, f);
    buf[got] = '\0';
    vredrs_str *r = vredrs_str_new(buf, got);
    free(buf);
    return r;
}
int64_t vredrs_write(int64_t handle, vredrs_str *s) {
    FILE *f = (FILE *)(intptr_t)handle;
    if (!f || !s) return 0;
    return (int64_t)fwrite(s->data, 1, (size_t)s->len, f);
}
vredrs_str *vredrs_read_file(vredrs_str *path) {
    if (!path) return vredrs_str_from_cstr("");
    FILE *f = fopen(path->data, "rb");
    if (!f) return vredrs_str_from_cstr("");
    fseek(f, 0, SEEK_END);
    long sz = ftell(f);
    fseek(f, 0, SEEK_SET);
    char *buf = (char *)malloc((size_t)sz + 1);
    int64_t got = (int64_t)fread(buf, 1, (size_t)sz, f);
    buf[got] = '\0';
    fclose(f);
    vredrs_str *r = vredrs_str_new(buf, got);
    free(buf);
    return r;
}
int64_t vredrs_write_file(vredrs_str *path, vredrs_str *content) {
    if (!path || !content) return 0;
    FILE *f = fopen(path->data, "wb");
    if (!f) return 0;
    int64_t n = (int64_t)fwrite(content->data, 1, (size_t)content->len, f);
    fclose(f);
    return n;
}
int64_t vredrs_file_exists(vredrs_str *path) {
    if (!path) return 0;
    FILE *f = fopen(path->data, "rb");
    if (!f) return 0;
    fclose(f);
    return 1;
}

void vredrs_exit(int64_t code) { exit((int)code); }
void vredrs_abort(const char *msg) { fputs(msg ? msg : "vredrs: abort\n", stderr); exit(70); }

/* -----------------------------------------------------------------------
 * Eager generator support: a generator runs to completion on the first
 * poll, collecting all yielded values into a list. The list and index
 * are stored in the coro's impl field.
 * --------------------------------------------------------------------- */
vredrs_list *vredrs_coro_get_list(vredrs_coro *c) {
    if (!c || !c->impl) return NULL;
    return (vredrs_list *)c->impl;
}

void vredrs_coro_set_list(vredrs_coro *c, vredrs_list *l) {
    if (c) c->impl = (void *)l;
}

int64_t vredrs_coro_get_index(vredrs_coro *c) {
    return c ? c->gen_index : 0;
}

void vredrs_coro_set_index(vredrs_coro *c, int64_t idx) {
    if (c) c->gen_index = idx;
}


/* -----------------------------------------------------------------------
 * Memory management: value cleanup functions.
 *
 * These are called by the generated IR when a local variable goes out of
 * scope, a container is destroyed, or an object's refcount hits zero.
 * For 1.0, strings and lists use simple ownership (no shared refs), so
 * free is a deep release.
 * --------------------------------------------------------------------- */

/* Deep-free a vredrs_value. For container types, recursively frees
   contained values. For str/obj, frees the pointer. For i64/f64/bool,
   no-op. */
void vredrs_value_free(vredrs_value v) {
    switch (v.tag) {
        case 4: { /* str */
            vredrs_str_free((vredrs_str *)(intptr_t)v.payload);
            break;
        }
        case 5: { /* list */
            vredrs_list *l = (vredrs_list *)(intptr_t)v.payload;
            if (l) {
                for (int64_t i = 0; i < l->len; i++) {
                    vredrs_value_free(l->data[i]);
                }
                vredrs_list_free(l);
            }
            break;
        }
        case 6: { /* dict */
            vredrs_dict *d = (vredrs_dict *)(intptr_t)v.payload;
            /* Keys are interned (not owned by dict), but values are freed. */
            if (d) {
                for (int64_t i = 0; i < d->cap; i++) {
                    if (d->keys[i]) {
                        vredrs_value_free(d->vals[i]);
                    }
                }
                vredrs_dict_free(d);
            }
            break;
        }
        case 7: { /* tuple */
            vredrs_tuple *t = (vredrs_tuple *)(intptr_t)v.payload;
            if (t) {
                for (int64_t i = 0; i < t->len; i++) {
                    vredrs_value_free(t->data[i]);
                }
                vredrs_tuple_free(t);
            }
            break;
        }
        case 8: { /* object */
            vredrs_dec_ref((vredrs_object *)(intptr_t)v.payload);
            break;
        }
        case 9: { /* coroutine */
            vredrs_coro *c = (vredrs_coro *)(intptr_t)v.payload;
            if (c) {
                if (c->impl) vredrs_list_free((vredrs_list *)c->impl);
                free(c);
            }
            break;
        }
        default:
            /* nil, i64, f64, bool: no allocation to free. */
            break;
    }
}

/* Print a value to a FILE* stream (for error messages). */
void vredrs_value_fprint(FILE *f, vredrs_value v) {
    vredrs_str *s = vredrs_value_to_str(v);
    if (s) {
        fputs(s->data, f);
        vredrs_str_free(s);
    }
}

/* Get the class name from a tagged value (if it wraps an object). */
const char *vredrs_value_class_name(vredrs_value v) {
    if (v.tag != 8) return NULL;
    vredrs_object *o = (vredrs_object *)(intptr_t)v.payload;
    if (!o || !o->vt) return NULL;
    return o->vt->class_name;
}

/* Runtime helper: index a tagged value (dict/list/str/tuple). */
vredrs_value vredrs_value_index(vredrs_value container, vredrs_value idx) {
    switch (container.tag) {
        case 4: { /* str */
            vredrs_str *s = (vredrs_str *)(intptr_t)container.payload;
            int64_t i = idx.payload;
            if (!s || i < 0 || i >= s->len) return vredrs_value_make_i64(0);
            return vredrs_value_make_i64((int64_t)(unsigned char)s->data[i]);
        }
        case 5: { /* list */
            vredrs_list *l = (vredrs_list *)(intptr_t)container.payload;
            int64_t i = idx.payload;
            if (!l || i < 0 || i >= l->len) return vredrs_value_nil();
            return vredrs_list_get(l, i);
        }
        case 6: { /* dict */
            vredrs_dict *d = (vredrs_dict *)(intptr_t)container.payload;
            /* idx should be a str */
            if (idx.tag != 4) return vredrs_value_nil();
            vredrs_str *k = (vredrs_str *)(intptr_t)idx.payload;
            return vredrs_dict_get(d, k, vredrs_value_nil());
        }
        case 7: { /* tuple */
            vredrs_tuple *t = (vredrs_tuple *)(intptr_t)container.payload;
            int64_t i = idx.payload;
            if (!t || i < 0 || i >= t->len) return vredrs_value_nil();
            return vredrs_tuple_get(t, i);
        }
        default:
            return vredrs_value_nil();
    }
}

// List concatenation: creates a new list with elements from both lists.
vredrs_list *vredrs_list_concat(vredrs_list *a, vredrs_list *b) {
    vredrs_list *result = vredrs_list_new();
    if (a) {
        for (int64_t i = 0; i < a->len; i++) {
            vredrs_list_append(result, vredrs_list_get(a, i));
        }
    }
    if (b) {
        for (int64_t i = 0; i < b->len; i++) {
            vredrs_list_append(result, vredrs_list_get(b, i));
        }
    }
    return result;
}

// Sorted descending: returns a new list sorted in descending order.
vredrs_list *vredrs_sorted_desc(vredrs_list *l) {
    if (!l) return vredrs_list_new();
    vredrs_list *result = vredrs_list_new();
    for (int64_t i = 0; i < l->len; i++) {
        vredrs_list_append(result, vredrs_list_get(l, i));
    }
    // Simple bubble sort (descending)
    for (int64_t i = 0; i < result->len - 1; i++) {
        for (int64_t j = 0; j < result->len - 1 - i; j++) {
            vredrs_value a = vredrs_list_get(result, j);
            vredrs_value b = vredrs_list_get(result, j + 1);
            // Compare as i64 (simplified)
            if (a.tag == 1 && b.tag == 1 && a.payload < b.payload) {
                vredrs_list_set(result, j, b);
                vredrs_list_set(result, j + 1, a);
            }
        }
    }
    return result;
}

// Math helpers for radians/degrees conversion.
double vredrs_math_radians(double deg) { return deg * 3.14159265358979323846 / 180.0; }
double vredrs_math_degrees(double rad) { return rad * 180.0 / 3.14159265358979323846; }

// Sleep helper.
void vredrs_sleep(int64_t ms) {
    struct timespec ts;
    ts.tv_sec = ms / 1000;
    ts.tv_nsec = (ms % 1000) * 1000000;
    nanosleep(&ts, NULL);
}

// Assert failure handler.
void vredrs_assert_fail(void) {
    fprintf(stderr, "Assertion failed\n");
    exit(1);
}

// OS helpers.
vredrs_str *vredrs_os_cwd(void) {
    char buf[4096];
    if (getcwd(buf, sizeof(buf))) {
        return vredrs_str_from_cstr(buf);
    }
    return vredrs_str_from_cstr("");
}

vredrs_str *vredrs_os_get_env(vredrs_str *name) {
    char *val = getenv(name->data);
    if (val) {
        return vredrs_str_from_cstr(val);
    }
    return vredrs_str_from_cstr("");
}

int64_t vredrs_is_file(vredrs_str *path) {
    struct stat st;
    if (stat(path->data, &st) == 0) {
        return S_ISREG(st.st_mode) ? 1 : 0;
    }
    return 0;
}

int64_t vredrs_is_dir(vredrs_str *path) {
    struct stat st;
    if (stat(path->data, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 1 : 0;
    }
    return 0;
}

int64_t vredrs_os_mkdir(vredrs_str *path) {
    return mkdir(path->data, 0755) == 0 ? 1 : 0;
}

vredrs_list *vredrs_os_args(void) {
    // This is a stub — actual arg access requires main() to save argv.
    return vredrs_list_new();
}

// String helpers.
vredrs_str *vredrs_split_helper(vredrs_str *s, vredrs_str *sep) {
    // This is declared but the actual split is handled by vredrs_split in the header.
    return vredrs_str_from_cstr("");
}

vredrs_str *vredrs_trim(vredrs_str *s) {
    if (!s || s->len == 0) return vredrs_str_from_cstr("");
    char *start = s->data;
    char *end = s->data + s->len - 1;
    while (start <= end && (*start == ' ' || *start == '\t' || *start == '\n' || *start == '\r')) start++;
    while (end >= start && (*end == ' ' || *end == '\t' || *end == '\n' || *end == '\r')) end--;
    int64_t len = end - start + 1;
    if (len <= 0) return vredrs_str_from_cstr("");
    vredrs_str *result = (vredrs_str *)malloc(sizeof(vredrs_str));
    result->len = len;
    result->data = (char *)malloc(len + 1);
    memcpy(result->data, start, len);
    result->data[len] = '\0';
    return result;
}

vredrs_str *vredrs_upper(vredrs_str *s) {
    if (!s) return vredrs_str_from_cstr("");
    vredrs_str *result = (vredrs_str *)malloc(sizeof(vredrs_str));
    result->len = s->len;
    result->data = (char *)malloc(s->len + 1);
    for (int64_t i = 0; i < s->len; i++) {
        result->data[i] = toupper((unsigned char)s->data[i]);
    }
    result->data[s->len] = '\0';
    return result;
}

vredrs_str *vredrs_lower(vredrs_str *s) {
    if (!s) return vredrs_str_from_cstr("");
    vredrs_str *result = (vredrs_str *)malloc(sizeof(vredrs_str));
    result->len = s->len;
    result->data = (char *)malloc(s->len + 1);
    for (int64_t i = 0; i < s->len; i++) {
        result->data[i] = tolower((unsigned char)s->data[i]);
    }
    result->data[s->len] = '\0';
    return result;
}

int64_t vredrs_contains(vredrs_str *haystack, vredrs_str *needle) {
    if (!haystack || !needle) return 0;
    if (needle->len == 0) return 1;
    if (haystack->len < needle->len) return 0;
    return strstr(haystack->data, needle->data) != NULL ? 1 : 0;
}

// Path helpers.
vredrs_str *vredrs_path_join(vredrs_str *a, vredrs_str *b) {
    if (!a || a->len == 0) return vredrs_str_from_cstr(b ? b->data : "");
    if (!b || b->len == 0) return vredrs_str_from_cstr(a->data);
    int64_t len = a->len + 1 + b->len;
    char *buf = (char *)malloc(len + 1);
    snprintf(buf, len + 1, "%s/%s", a->data, b->data);
    vredrs_str *result = (vredrs_str *)malloc(sizeof(vredrs_str));
    result->len = len;
    result->data = buf;
    return result;
}

vredrs_str *vredrs_path_dirname(vredrs_str *p) {
    if (!p) return vredrs_str_from_cstr("");
    char *slash = strrchr(p->data, '/');
    if (!slash) return vredrs_str_from_cstr(".");
    int64_t len = slash - p->data;
    if (len == 0) return vredrs_str_from_cstr("/");
    vredrs_str *result = (vredrs_str *)malloc(sizeof(vredrs_str));
    result->len = len;
    result->data = (char *)malloc(len + 1);
    memcpy(result->data, p->data, len);
    result->data[len] = '\0';
    return result;
}

vredrs_str *vredrs_path_basename(vredrs_str *p) {
    if (!p) return vredrs_str_from_cstr("");
    char *slash = strrchr(p->data, '/');
    if (!slash) return vredrs_str_from_cstr(p->data);
    return vredrs_str_from_cstr(slash + 1);
}

vredrs_str *vredrs_path_ext(vredrs_str *p) {
    if (!p) return vredrs_str_from_cstr("");
    char *dot = strrchr(p->data, '.');
    if (!dot) return vredrs_str_from_cstr("");
    return vredrs_str_from_cstr(dot);
}

int64_t vredrs_path_exists(vredrs_str *p) {
    struct stat st;
    return stat(p->data, &st) == 0 ? 1 : 0;
}

int64_t vredrs_path_is_abs(vredrs_str *p) {
    if (!p || p->len == 0) return 0;
    return p->data[0] == '/' ? 1 : 0;
}

vredrs_str *vredrs_path_abs(vredrs_str *p) {
    char buf[4096];
    if (realpath(p->data, buf)) {
        return vredrs_str_from_cstr(buf);
    }
    return vredrs_str_from_cstr(p->data);
}
