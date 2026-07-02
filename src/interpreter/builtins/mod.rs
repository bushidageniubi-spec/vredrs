//! Built-in function registration for the interpreter.
//!
//! All native functions are registered here via `install_builtins`.

use super::*;
use crate::error::Result;

impl Interpreter {
    /// Register all built-in functions in the global scope.
    pub(crate) fn install_builtins(&mut self) {
        let builtins: &[(&str, NativeFn)] = &[
            ("len", Self::native_len),
            ("str", Self::native_str),
            ("int", Self::native_int),
            ("float", Self::native_float),
            ("bool", Self::native_bool),
            ("type_of", Self::native_type_of),
            ("range", Self::native_range),
            ("enumerate", Self::native_enumerate),
            ("zip", Self::native_zip),
            ("map", Self::native_map),
            ("filter", Self::native_filter),
            ("sum", Self::native_sum),
            ("min", Self::native_min),
            ("max", Self::native_max),
            ("sorted", Self::native_sorted),
            ("reversed", Self::native_reversed),
            ("print", Self::native_print),
            ("println", Self::native_println),
            ("paste", Self::native_print),
            ("input", Self::native_input),
            ("open", Self::native_open),
            ("read", Self::native_read),
            ("write", Self::native_write),
            ("close", Self::native_close),
            ("exit", Self::native_exit),
            ("dict", Self::native_dict),
            ("dict_get", Self::native_dict_get),
            ("dict_set", Self::native_dict_set),
            ("dict_keys", Self::native_dict_keys),
            ("dict_values", Self::native_dict_values),
            ("dict_has", Self::native_dict_has),
            ("read_file", Self::native_read_file),
            ("write_file", Self::native_write_file),
            ("file_exists", Self::native_file_exists),
            ("channel", Self::native_channel),
            ("send", Self::native_send),
            ("receive", Self::native_receive),
            ("spawn", Self::native_spawn),
            ("resume", Self::native_resume),
            ("scheduler_run", Self::native_scheduler_run),
            ("iter", Self::native_iter),
            ("next", Self::native_next),
            ("stop", Self::native_stop),
            ("hash", Self::native_hash),
            ("read_dir", Self::native_read_dir),
            ("is_dir", Self::native_is_dir),
            ("is_file", Self::native_is_file),
            ("path_join", Self::native_path_join),
            ("basename", Self::native_basename),
            ("dirname", Self::native_dirname),
            ("set_recursion_limit", Self::native_set_recursion_limit),
            ("annotations", Self::native_annotations),
            ("is_frozen", Self::native_is_frozen),
            ("freeze", Self::native_freeze),
        ];
        for (name, f) in builtins {
            self.global(name, Value::Native(*f));
            self.builtins.insert(name.to_string());
        }
    }
}
