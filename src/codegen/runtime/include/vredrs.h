/* ============================================================================
 * Vredrs runtime library — public header.
 *
 * Declares all types and functions used by the generated LLVM IR.
 * The implementation lives in vredrs_runtime.c (which may later be split
 * further into per-type .c files).
 * ==========================================================================*/

#ifndef VREDRS_H
#define VREDRS_H

#include <stdint.h>
#include <stdio.h>

/* Forward declarations */
typedef struct vredrs_str     vredrs_str;
typedef struct vredrs_list    vredrs_list;
typedef struct vredrs_dict    vredrs_dict;
typedef struct vredrs_tuple   vredrs_tuple;
typedef struct vredrs_object  vredrs_object;
typedef struct vredrs_vtable  vredrs_vtable;
typedef struct vredrs_coro    vredrs_coro;

/* Tagged dynamic value: { i8 tag, i64 payload }.
   Tag: 0=nil, 1=i64, 2=f64, 3=bool, 4=str, 5=list, 6=dict, 7=tuple, 8=obj, 9=coro. */
typedef struct vredrs_value { int8_t tag; int64_t payload; } vredrs_value;

/* String: { len, data } — NUL-terminated for printf. */
struct vredrs_str { int64_t len; char *data; };

/* List: dynamic array of vredrs_value. */
struct vredrs_list { int64_t len; int64_t cap; vredrs_value *data; };

/* Dict: linear-probing hash map keyed by vredrs_str*. */
struct vredrs_dict { int64_t len; int64_t cap; vredrs_str **keys; vredrs_value *vals; };

/* Tuple: immutable array. */
struct vredrs_tuple { int64_t len; vredrs_value *data; };

/* Vtable: method dispatch table with parent chain. */
struct vredrs_vtable {
    int64_t method_count;
    void  **methods;
    vredrs_vtable *parent;
    const char *class_name;
};

/* Object: vtable + dynamic field table + refcount. */
struct vredrs_object { vredrs_vtable *vt; vredrs_dict *fields; int64_t rc; };

/* Coroutine: state machine for generators and async fns. */
struct vredrs_coro { int64_t state; int64_t result; void *impl; int64_t gen_index; };

/* ---- String operations ---- */
vredrs_str *vredrs_str_new(const char *src, int64_t len);
vredrs_str *vredrs_str_from_cstr(const char *src);
void vredrs_str_free(vredrs_str *s);
const char *vredrs_str_data(vredrs_str *s);
int64_t vredrs_str_len(vredrs_str *s);
vredrs_str *vredrs_str_concat(vredrs_str *a, vredrs_str *b);
int64_t vredrs_str_eq(vredrs_str *a, vredrs_str *b);
vredrs_str *vredrs_str_from_i64(int64_t v);
vredrs_str *vredrs_str_from_f64(double v);
vredrs_str *vredrs_str_from_bool(int64_t b);

/* ---- Tagged value constructors and accessors ---- */
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
vredrs_str *vredrs_value_get_str(vredrs_value v);
vredrs_list *vredrs_value_get_list(vredrs_value v);
vredrs_dict *vredrs_value_get_dict(vredrs_value v);
vredrs_tuple *vredrs_value_get_tuple(vredrs_value v);
vredrs_object *vredrs_value_get_obj(vredrs_value v);
vredrs_str *vredrs_value_to_str(vredrs_value v);
void vredrs_value_print(vredrs_value v);
void vredrs_value_print_fast(vredrs_value v);
int64_t vredrs_value_truthy(vredrs_value v);
int64_t vredrs_value_eq(vredrs_value a, vredrs_value b);
int64_t vredrs_value_ne(vredrs_value a, vredrs_value b);
int64_t vredrs_value_lt(vredrs_value a, vredrs_value b);
int64_t vredrs_value_gt(vredrs_value a, vredrs_value b);
int64_t vredrs_value_le(vredrs_value a, vredrs_value b);
int64_t vredrs_value_ge(vredrs_value a, vredrs_value b);
vredrs_value vredrs_value_add(vredrs_value a, vredrs_value b);
vredrs_value vredrs_value_sub(vredrs_value a, vredrs_value b);
vredrs_value vredrs_value_mul(vredrs_value a, vredrs_value b);
vredrs_value vredrs_value_div(vredrs_value a, vredrs_value b);
vredrs_value vredrs_value_mod(vredrs_value a, vredrs_value b);

/* ---- List operations ---- */
vredrs_list *vredrs_list_new(void);
void vredrs_list_free(vredrs_list *l);
int64_t vredrs_list_len(vredrs_list *l);
void vredrs_list_push(vredrs_list *l, vredrs_value v);
vredrs_value vredrs_list_get(vredrs_list *l, int64_t i);
void vredrs_list_set(vredrs_list *l, int64_t i, vredrs_value v);
vredrs_value vredrs_list_pop(vredrs_list *l);
int64_t vredrs_list_contains(vredrs_list *l, vredrs_value v);
vredrs_list *vredrs_list_slice(vredrs_list *l, int64_t start, int64_t end);
vredrs_list *vredrs_list_slice_full(vredrs_list *l, int64_t start, int64_t end, int64_t step);
vredrs_str  *vredrs_str_slice_full(vredrs_str *s, int64_t start, int64_t end, int64_t step);

/* ---- Dict operations ---- */
vredrs_dict *vredrs_dict_new(void);
void vredrs_dict_free(vredrs_dict *d);
int64_t vredrs_dict_len(vredrs_dict *d);
void vredrs_dict_set(vredrs_dict *d, vredrs_str *k, vredrs_value v);
vredrs_value vredrs_dict_get(vredrs_dict *d, vredrs_str *k, vredrs_value dflt);
int64_t vredrs_dict_has(vredrs_dict *d, vredrs_str *k);
void vredrs_dict_del(vredrs_dict *d, vredrs_str *k);
vredrs_list *vredrs_dict_keys(vredrs_dict *d);
vredrs_list *vredrs_dict_values(vredrs_dict *d);

/* ---- Tuple operations ---- */
vredrs_tuple *vredrs_tuple_new(int64_t n);
void vredrs_tuple_free(vredrs_tuple *t);
void vredrs_tuple_set_init(vredrs_tuple *t, int64_t i, vredrs_value v);
vredrs_value vredrs_tuple_get(vredrs_tuple *t, int64_t i);
int64_t vredrs_tuple_len(vredrs_tuple *t);

/* ---- Set operations ---- */
vredrs_dict *vredrs_set_new(void);
void vredrs_set_add(vredrs_dict *s, vredrs_str *k);
int64_t vredrs_set_has(vredrs_dict *s, vredrs_str *k);
int64_t vredrs_set_len(vredrs_dict *s);

/* ---- Object operations ---- */
vredrs_object *vredrs_object_new(vredrs_vtable *vt);
void vredrs_object_free(vredrs_object *o);
void vredrs_object_set_field(vredrs_object *o, vredrs_str *k, vredrs_value v);
vredrs_value vredrs_object_get_field(vredrs_object *o, vredrs_str *k, vredrs_value dflt);
int64_t vredrs_object_has_field(vredrs_object *o, vredrs_str *k);
void vredrs_object_del_field(vredrs_object *o, vredrs_str *k);
void *vredrs_object_get_method(vredrs_object *o, int64_t index);
void vredrs_inc_ref(vredrs_object *o);
void vredrs_dec_ref(vredrs_object *o);
vredrs_vtable *vredrs_object_vtable(vredrs_object *o);
vredrs_vtable *vredrs_vtable_parent(vredrs_vtable *vt);
int64_t vredrs_object_is_frozen(vredrs_object *o);
void vredrs_object_freeze(vredrs_object *o);

/* ---- Coroutine operations ---- */
vredrs_coro *vredrs_coro_alloc(void);
int64_t vredrs_coro_result(vredrs_coro *c);
void vredrs_coro_store_result(vredrs_coro *c, int64_t v);
void vredrs_coro_set_state(vredrs_coro *c, int64_t s);
int64_t vredrs_coro_state(vredrs_coro *c);
vredrs_list *vredrs_coro_get_list(vredrs_coro *c);
void vredrs_coro_set_list(vredrs_coro *c, vredrs_list *l);
int64_t vredrs_coro_get_index(vredrs_coro *c);
void vredrs_coro_set_index(vredrs_coro *c, int64_t idx);

/* ---- Exception handling ---- */
void vredrs_set_jmp_top(void *buf);
void *vredrs_get_jmp_top(void);
int64_t vredrs_get_exception_i64(void);
vredrs_str *vredrs_get_exception_str(void);
void vredrs_clear_exception(void);
void *vredrs_try_begin(void);
int32_t vredrs_try_setjmp(void *buf);
void vredrs_try_end(void *buf);
void vredrs_throw_i64(int64_t v);
void vredrs_throw_str(vredrs_str *s);

/* ---- Built-in functions ---- */
int64_t vredrs_len_str(vredrs_str *s);
int64_t vredrs_len_list(vredrs_list *l);
int64_t vredrs_len_dict(vredrs_dict *d);
int64_t vredrs_len_tuple(vredrs_tuple *t);
int64_t vredrs_len_set(vredrs_dict *s);
vredrs_str *vredrs_str_of_i64(int64_t v);
vredrs_str *vredrs_str_of_f64(double v);
vredrs_str *vredrs_str_of_bool(int64_t b);
vredrs_str *vredrs_str_of_str(vredrs_str *s);
vredrs_str *vredrs_str_of_ptr(void *p);
int64_t vredrs_int_of_str(vredrs_str *s);
int64_t vredrs_int_of_f64(double v);
int64_t vredrs_int_of_bool(int64_t b);
double vredrs_float_of_str(vredrs_str *s);
double vredrs_float_of_i64(int64_t v);
int64_t vredrs_bool_of_str(vredrs_str *s);
vredrs_list *vredrs_range(int64_t start, int64_t end);
vredrs_list *vredrs_range1(int64_t end);
vredrs_list *vredrs_enumerate(vredrs_list *l);
vredrs_list *vredrs_zip(vredrs_list *a, vredrs_list *b);
int64_t vredrs_sum(vredrs_list *l);
int64_t vredrs_min(vredrs_list *l);
int64_t vredrs_max(vredrs_list *l);
vredrs_list *vredrs_sorted(vredrs_list *l);
vredrs_list *vredrs_reversed(vredrs_list *l);
int64_t vredrs_print_str(vredrs_str *s);
int64_t vredrs_print_i64(int64_t v);
int64_t vredrs_print_f64(double v);
int64_t vredrs_print_bool(int64_t b);
int64_t vredrs_print_cstr(const char *s);
int64_t vredrs_println(void);
int64_t vredrs_print_space(void);
int64_t vredrs_print_value(vredrs_value v);
vredrs_str *vredrs_input(vredrs_str *prompt);
int64_t vredrs_open(vredrs_str *path, vredrs_str *mode);
void vredrs_close(int64_t handle);
vredrs_str *vredrs_read(int64_t handle, int64_t n);
int64_t vredrs_write(int64_t handle, vredrs_str *s);
vredrs_str *vredrs_read_file(vredrs_str *path);
int64_t vredrs_write_file(vredrs_str *path, vredrs_str *content);
int64_t vredrs_file_exists(vredrs_str *path);
void vredrs_exit(int64_t code);
void vredrs_abort(const char *msg);
int64_t vredrs_eq_str(vredrs_str *a, vredrs_str *b);
int64_t vredrs_ne_str(vredrs_str *a, vredrs_str *b);

#endif /* VREDRS_H */
const char *vredrs_value_class_name(vredrs_value v);
vredrs_value vredrs_value_index(vredrs_value container, vredrs_value idx);
