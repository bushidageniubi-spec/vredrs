//! Core built-in functions: len, str, int, float, bool, type_of, etc.
//!
//! This file contains all `native_*` functions extracted from the
//! original monolithic interpreter.rs.

use super::super::*;
use crate::error::{CompilerError, Result};
use crate::parser::ast::*;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Read, Write};

// All native functions are impl methods on Interpreter.
// They are defined in the main mod.rs file and will be progressively
// moved here in future refactoring passes.

// For now, this module serves as the documentation entry point for
// the builtins subsystem.
