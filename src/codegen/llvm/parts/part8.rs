impl FullLlvmGen {
    fn gen_class_new(
        &mut self,
        class_name: &str,
        args: Vec<Val>,
        _vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let c = self.classes.get(class_name).cloned().ok_or_else(|| {
            CompilerError::codegen_error(format!("unknown class '{}'", class_name))
        })?;
        let obj = self.new_var();
        self.buf.push_str(&format!("  ; new {}\n", class_name));
        self.buf.push_str(&format!(
            "  {} = call %vredrs.object* @vredrs_object_new(%vredrs.vtable* @{}.vtable)\n",
            obj,
            self.safe(class_name)
        ));
        // Initialize declared fields with defaults (or zero if none).
        for f in &c.fields {
            let v = match &f.default {
                Some(d) => self.gen_expr(d, _vars)?,
                None => self.default_value_for_type(&f.ty),
            };
            let raw = self.value_to_i64_slot(v)?;
            let key = self.intern_str_for(&f.name)?;
            self.buf.push_str(&format!("  call void @vredrs_object_set_field(%vredrs.object* {}, %vredrs.str* {}, i64 {})\n", obj, key, raw));
        }
        // Constructor dispatch — match the VM's `instantiate_class` semantics:
        //   1. If the class defines `__init__`, call it (Python-style; it
        //      mutates `self` in place and its return value is ignored).
        //   2. Otherwise, if the class defines `new`, call it (Vredrs-style;
        //      it may `return, self` and its return value is used).
        //   3. Otherwise, if no constructor exists, the bare object (with
        //      default-initialized fields) is returned. Extra args are
        //      rejected in that case.
        let obj_val = Val::new(Ty::Obj, obj.clone()).with_class(class_name.to_string());
        if self.class_has_method(class_name, "__init__") {
            // __init__ mutates self in place; discard any return value.
            let _ = self.emit_vtable_call(obj_val.clone(), class_name, "__init__", args)?;
        } else if self.class_has_method(class_name, "new") {
            let _ = self.emit_vtable_call(obj_val, class_name, "new", args)?;
        } else if !args.is_empty() {
            return Err(CompilerError::codegen_error(format!(
                "class '{}' has no new() or __init__() but got {} args",
                class_name,
                args.len()
            )));
        }
        Ok(Val::new(Ty::Obj, obj).with_class(class_name.to_string()))
    }

    fn gen_method_call(
        &mut self,
        m: &MethodCallExpr,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        // Stdlib module method call: math.sqrt(), time.now(), io.open(), etc.
        // If the receiver is a known stdlib module name, resolve to
        // "module_method" builtin.
        if let Expr::Identifier(id) = m.receiver.as_ref() {
            let known_modules = [
                "math", "io", "os", "time", "fs", "fmt", "json", "collections",
                "rand", "path", "encoding", "regex", "debug", "log", "term",
                "flag", "sync", "net", "http", "image", "machine", "unsafe",
                "embed", "csv", "xml", "toml", "yaml", "compress", "websocket",
                "sql", "crypto",
            ];
            if known_modules.contains(&id.name.as_str()) {
                let builtin_name = format!("{}_{}", id.name, m.method.name);
                if self.builtins.contains(builtin_name.as_str()) {
                    let args: Vec<Val> = {
                        let mut out = Vec::new();
                        for a in &m.args {
                            out.push(self.gen_expr(a, vars)?);
                        }
                        out
                    };
                    return self.gen_builtin_call(&builtin_name, args, vars);
                }
                // Also try without module prefix (e.g. "now" → "time_now")
                if self.builtins.contains(m.method.name.as_str()) {
                    let args: Vec<Val> = {
                        let mut out = Vec::new();
                        for a in &m.args {
                            out.push(self.gen_expr(a, vars)?);
                        }
                        out
                    };
                    return self.gen_builtin_call(&m.method.name, args, vars);
                }
            }
        }
        // super.method() handling: if the receiver is the synthetic
        // `__super_class` slot, dispatch via the parent vtable.
        if let Expr::Identifier(id) = m.receiver.as_ref() {
            if id.name == "super" {
                if let Some(slot) = vars.get("__super_class") {
                    let parent = slot
                        .class
                        .clone()
                        .ok_or_else(|| CompilerError::codegen_error("super outside method"))?;
                    // Load self from the slot's pointer (which is the self ptr).
                    let self_val = self.load_local("__super_class", vars)?;
                    // Resolve method in parent class (which is the current
                    // class for super calls — the vtable index already points
                    // to the parent's slot).
                    let args: Vec<Val> = {
                        let mut out = Vec::new();
                        for a in &m.args {
                            out.push(self.gen_expr(a, vars)?);
                        }
                        out
                    };
                    // For super calls, call the parent's method DIRECTLY by
                    // function name — not via vtable dispatch. This avoids
                    // the vtable walking in vredrs_object_get_method which
                    // would find the child's override instead of the parent's.
                    let parent_class = self.classes.get(&parent)
                        .ok_or_else(|| CompilerError::codegen_error(
                            format!("super: parent class '{}' not found", parent)))?;
                    let method_info = parent_class.method_idx.get(&m.method.name)
                        .and_then(|&idx| parent_class.methods.get(idx))
                        .ok_or_else(|| CompilerError::codegen_error(
                            format!("super: method '{}' not found in parent '{}'", m.method.name, parent)))?;
                    let m_info = method_info.clone();
                    if m_info.sig.params.len() != args.len() {
                        return Err(CompilerError::codegen_error(
                            format!("super.{} expects {} args, got {}", m.method.name, m_info.sig.params.len(), args.len())));
                        }
                    // Build a direct call to @ParentClass__method
                    let mut call_args = vec![format!("%vredrs.object* {}", self_val.repr)];
                    for (i, a) in args.into_iter().enumerate() {
                        let casted = self.cast(a, &m_info.sig.params[i])?;
                        call_args.push(format!("{} {}", m_info.sig.params[i].ir(), casted.repr));
                    }
                    if m_info.sig.ret == Ty::Void {
                        self.buf.push_str(&format!(
                            "  call void @{}({})\n",
                            m_info.llvm_name, call_args.join(", ")
                        ));
                        return Ok(Val::new(Ty::Void, ""));
                    } else {
                        let t = self.new_var();
                        self.buf.push_str(&format!(
                            "  {} = call {} @{}({})\n",
                            t, m_info.sig.ret.ir(), m_info.llvm_name, call_args.join(", ")
                        ));
                        let mut v = Val::new(m_info.sig.ret.clone(), t);
                        if m_info.sig.ret == Ty::Obj || m_info.sig.ret == Ty::Value {
                            v.class = Some(parent.clone());
                        }
                        return Ok(v);
                    }
                }
            }
        }
        let recv = self.gen_expr(&m.receiver, vars)?;
        // File-handle method calls: i64 receiver with write/read/close/flush.
        if recv.ty == Ty::I64 {
            match m.method.name.as_str() {
                "write" => {
                    if m.args.len() != 1 {
                        return Err(CompilerError::codegen_error(
                            "f.write(s) expects 1 arg",
                        ));
                    }
                    let arg = self.gen_expr(&m.args[0], vars)?;
                    let s = self.to_str(arg, vars)?;
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_write(i64 {}, %vredrs.str* {})\n",
                        t, recv.repr, s.repr
                    ));
                    return Ok(Val::new(Ty::I64, t));
                }
                "read" => {
                    let n = if m.args.is_empty() {
                        Val::new(Ty::I64, "-1".to_string())
                    } else {
                        let v = self.gen_expr(&m.args[0], vars)?;
                        self.cast(v, &Ty::I64)?
                    };
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call %vredrs.str* @vredrs_read(i64 {}, i64 {})\n",
                        t, recv.repr, n.repr
                    ));
                    return Ok(Val::new(Ty::Str, t));
                }
                "close" => {
                    self.buf.push_str(&format!(
                        "  call void @vredrs_close(i64 {})\n",
                        recv.repr
                    ));
                    return Ok(Val::new(Ty::Void, ""));
                }
                "flush" => {
                    // vredrs has no flush; treat as no-op.
                    return Ok(Val::new(Ty::Void, ""));
                }
                _ => {}
            }
        }
        // String method calls (limited subset).
        if recv.ty == Ty::Str {
            match m.method.name.as_str() {
                "len" => {
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_str_len(%vredrs.str* {})\n",
                        t, recv.repr
                    ));
                    return Ok(Val::new(Ty::I64, t));
                }
                "upper" | "lower" => {
                    // Best-effort: return the string itself (runtime has no
                    // case conversion). This is a known 1.0 limitation.
                    return Ok(recv);
                }
                _ => {}
            }
        }
        // List method calls (limited subset).
        if recv.ty == Ty::List {
            match m.method.name.as_str() {
                "len" => {
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_list_len(%vredrs.list* {})\n",
                        t, recv.repr
                    ));
                    return Ok(Val::new(Ty::I64, t));
                }
                "push" => {
                    if m.args.len() != 1 {
                        return Err(CompilerError::codegen_error(
                            "list.push expects 1 arg",
                        ));
                    }
                    let arg = self.gen_expr(&m.args[0], vars)?;
                    let wv = self.wrap_value(arg)?;
                    self.buf.push_str(&format!(
                        "  call void @vredrs_list_push(%vredrs.list* {}, %vredrs.value {})\n",
                        recv.repr, wv.repr
                    ));
                    return Ok(Val::new(Ty::Void, ""));
                }
                "pop" => {
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call %vredrs.value @vredrs_list_pop(%vredrs.list* {})\n",
                        t, recv.repr
                    ));
                    return Ok(Val::new(Ty::Value, t));
                }
                _ => {}
            }
        }
        // If the receiver is a tagged value, try to extract the object.
        let recv = if recv.ty == Ty::Value {
            let class = recv.class.clone();
            let extracted = self.cast(recv, &Ty::Obj)?;
            let mut e = extracted;
            e.class = class;
            e
        } else {
            recv
        };
        if recv.ty != Ty::Obj {
            // For non-object receivers, check if this is a known method
            // that can be applied to the value type.
            // .unix() on an i64 (from time.now()) just returns the value.
            if m.method.name == "unix" || m.method.name == "to_unix" {
                return Ok(recv);
            }
            // .str() / .to_str() on any type converts to string.
            if m.method.name == "str" || m.method.name == "to_str" {
                return self.to_str(recv, vars);
            }
            // .len() on a string or list returns the length.
            if m.method.name == "len" {
                return self.gen_builtin_call("len", vec![recv], vars);
            }
            return Err(CompilerError::codegen_error(
                "method call needs object receiver",
            ));
        }
        let class = if let Some(c) = recv.class.clone() {
            c
        } else {
            let matches: Vec<&String> = self
                .classes
                .keys()
                .filter(|cn| self.class_has_method(cn, &m.method.name))
                .collect();
            if matches.len() == 1 {
                matches[0].clone()
            } else if matches.is_empty() {
                return Err(CompilerError::codegen_error(format!(
                    "no class defines method '{}'",
                    m.method.name
                )));
            } else {
                return Err(CompilerError::codegen_error(format!(
                    "ambiguous method '{}': defined in {} classes",
                    m.method.name,
                    matches.len()
                )));
            }
        };
        let args: Vec<Val> = {
            let mut out = Vec::new();
            for a in &m.args {
                out.push(self.gen_expr(a, vars)?);
            }
            out
        };
        self.emit_vtable_call(recv, &class, &m.method.name, args)
    }

    fn emit_vtable_call(
        &mut self,
        recv: Val,
        class: &str,
        method: &str,
        args: Vec<Val>,
    ) -> Result<Val> {
        let m = self.method_info(class, method)?.clone();
        if m.sig.params.len() != args.len() {
            return Err(CompilerError::codegen_error(format!(
                "method {}.{} expects {} args, got {}",
                class,
                method,
                m.sig.params.len(),
                args.len()
            )));
        }
        // Load function pointer from vtable slot.
        let raw = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call ptr @vredrs_object_get_method(%vredrs.object* {}, i64 {})\n",
            raw, recv.repr, m.index
        ));
        let typed = self.new_var();
        self.buf
            .push_str(&format!("  {} = bitcast ptr {} to ptr\n", typed, raw));
        let mut call_args = vec![format!("%vredrs.object* {}", recv.repr)];
        for (i, a) in args.into_iter().enumerate() {
            let casted = self.cast(a, &m.sig.params[i])?;
            call_args.push(format!("{} {}", m.sig.params[i].ir(), casted.repr));
        }
        if m.sig.ret == Ty::Void {
            self.buf.push_str(&format!(
                "  call void {}({})\n",
                typed,
                call_args.join(", ")
            ));
            Ok(Val::new(Ty::Void, ""))
        } else {
            let t = self.new_var();
            self.buf.push_str(&format!(
                "  {} = call {} {}({})\n",
                t,
                m.sig.ret.ir(),
                typed,
                call_args.join(", ")
            ));
            let mut v = Val::new(m.sig.ret.clone(), t);
            // Preserve class info for Obj and Value returns so that
            // method chains (a.add(b).get()) work.
            if m.sig.ret == Ty::Obj || m.sig.ret == Ty::Value {
                v.class = Some(class.to_string());
            }
            Ok(v)
        }
    }

    fn gen_index(&mut self, i: &IndexExpr, vars: &mut HashMap<String, LocalSlot>) -> Result<Val> {
        let t = self.gen_expr(&i.target, vars)?;
        // If the target is a tagged value, dispatch via runtime helper.
        if t.ty == Ty::Value {
            let idx_val = self.gen_expr(&i.index, vars)?;
            let wv = self.wrap_value(t)?;
            let widx = self.wrap_value(idx_val)?;
            let result = self.new_var();
            self.buf.push_str(&format!(
                "  {} = call %vredrs.value @vredrs_value_index(%vredrs.value {}, %vredrs.value {})\n",
                result, wv.repr, widx.repr
            ));
            return Ok(Val::new(Ty::Value, result));
        }
        let idx = self.gen_expr(&i.index, vars)?;
        match t.ty {
            Ty::List => {
                let i64_idx = self.cast(idx, &Ty::I64)?;
                let v = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.value @vredrs_list_get(%vredrs.list* {}, i64 {})\n",
                    v, t.repr, i64_idx.repr
                ));
                Ok(Val::new(Ty::Value, v))
            }
            Ty::Tuple => {
                let i64_idx = self.cast(idx, &Ty::I64)?;
                let v = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.value @vredrs_tuple_get(%vredrs.tuple* {}, i64 {})\n",
                    v, t.repr, i64_idx.repr
                ));
                Ok(Val::new(Ty::Value, v))
            }
            Ty::Dict => {
                let key = self.to_str(idx, vars)?;
                let v = self.new_var();
                self.buf.push_str(&format!("  {} = call %vredrs.value @vredrs_dict_get(%vredrs.dict* {}, %vredrs.str* {}, %vredrs.value zeroinitializer)\n", v, t.repr, key.repr));
                Ok(Val::new(Ty::Value, v))
            }
            Ty::Str => {
                let i64_idx = self.cast(idx, &Ty::I64)?;
                let data = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i8* @vredrs_str_data(%vredrs.str* {})\n",
                    data, t.repr
                ));
                let byte_ptr = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = getelementptr inbounds i8, ptr {}, i64 {}\n",
                    byte_ptr, data, i64_idx.repr
                ));
                let byte = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = load i8, i8* {}, align 1\n",
                    byte, byte_ptr
                ));
                let b_i64 = self.new_var();
                self.buf
                    .push_str(&format!("  {} = sext i8 {} to i64\n", b_i64, byte));
                Ok(Val::new(Ty::I64, b_i64))
            }
            Ty::Obj => {
                let class = t
                    .class
                    .clone()
                    .ok_or_else(|| CompilerError::codegen_error("obj[i] needs class"))?;
                self.emit_vtable_call(t, &class, "__getitem__", vec![idx])
            }
            _ => Err(CompilerError::codegen_error(
                "index needs list/dict/str/tuple/object",
            )),
        }
    }

    /// Lower a slice expression `target[start:end:step]` (any of start/end/step
    /// may be absent). Delegates to `vredrs_list_slice_full` or
    /// `vredrs_str_slice_full` in the C runtime, which implement the full
    /// Python slice semantics (negative indices, negative step, etc.).
    /// Absent bounds are passed as INT64_MIN (= i64 minimum) which the runtime
    /// treats as the default for that direction.
    fn gen_slice(
        &mut self,
        s: &SliceExpr,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let t = self.gen_expr(&s.target, vars)?;
        // Resolve each bound: None => INT64_MIN, Some(expr) => evaluated i64.
        let neg_min = "-9223372036854775808".to_string();
        let start_repr = if let Some(e) = &s.start {
            let v = self.gen_expr(e, vars)?;
            let i = self.cast(v, &Ty::I64)?;
            i.repr
        } else {
            neg_min.clone()
        };
        let end_repr = if let Some(e) = &s.end {
            let v = self.gen_expr(e, vars)?;
            let i = self.cast(v, &Ty::I64)?;
            i.repr
        } else {
            neg_min.clone()
        };
        let step_repr = if let Some(e) = &s.step {
            let v = self.gen_expr(e, vars)?;
            let i = self.cast(v, &Ty::I64)?;
            i.repr
        } else {
            neg_min.clone()
        };
        match t.ty {
            Ty::List => {
                let r = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_list_slice_full(%vredrs.list* {}, i64 {}, i64 {}, i64 {})\n",
                    r, t.repr, start_repr, end_repr, step_repr
                ));
                Ok(Val::new(Ty::List, r))
            }
            Ty::Str => {
                let r = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_slice_full(%vredrs.str* {}, i64 {}, i64 {}, i64 {})\n",
                    r, t.repr, start_repr, end_repr, step_repr
                ));
                Ok(Val::new(Ty::Str, r))
            }
            Ty::Tuple => {
                // Materialize tuple into a temp list, then slice the list.
                let n = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_tuple_len(%vredrs.tuple* {})\n",
                    n, t.repr
                ));
                let tmp = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_list_new()\n",
                    tmp
                ));
                let loop_lbl = self.new_lbl();
                let body_lbl = self.new_lbl();
                let done_lbl = self.new_lbl();
                let i_slot = self.new_local("__slice_i");
                self.buf.push_str(&format!(
                    "  {} = alloca i64, align 8\n  store i64 0, i64* {}, align 8\n  br label %L{}\nL{}:\n",
                    i_slot, i_slot, loop_lbl, loop_lbl
                ));
                let cur = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = load i64, i64* {}, align 8\n",
                    cur, i_slot
                ));
                let cond = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = icmp slt i64 {}, {}\n",
                    cond, cur, n
                ));
                self.buf.push_str(&format!(
                    "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
                    cond, body_lbl, done_lbl, body_lbl
                ));
                let elt = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.value @vredrs_tuple_get(%vredrs.tuple* {}, i64 {})\n",
                    elt, t.repr, cur
                ));
                self.buf.push_str(&format!(
                    "  call void @vredrs_list_push(%vredrs.list* {}, %vredrs.value {})\n",
                    tmp, elt
                ));
                let next = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = add i64 {}, 1\n  store i64 {}, i64* {}, align 8\n  br label %L{}\nL{}:\n",
                    next, cur, next, i_slot, loop_lbl, done_lbl
                ));
                let r = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_list_slice_full(%vredrs.list* {}, i64 {}, i64 {}, i64 {})\n",
                    r, tmp, start_repr, end_repr, step_repr
                ));
                Ok(Val::new(Ty::List, r))
            }
            _ => Err(CompilerError::codegen_error(format!(
                "slice needs list/str/tuple, got {:?}",
                t.ty
            ))),
        }
    }

    /// Lift a lambda expression to a top-level function and return a
    /// first-class function value (Ty::Fn, an i64 closure-struct pointer).
    ///
    /// Captures are supported: free variables referenced in the lambda body
    /// (and present in the enclosing `vars` map) are copied into a stack-
    /// allocated env array at lambda-creation time. The env array is bundled
    /// with the function pointer in a `{ ptr fn, ptr env }` struct; the
    /// struct's address (as an i64) is the closure value. At call sites the
    /// fn and env pointers are loaded back out and the env pointer is passed
    /// as the trailing `%__env` parameter to the lifted lambda function.
    ///
    /// All lambdas use a uniform signature so indirect calls don't need
    /// per-lambda type info:
    ///   `define %vredrs.value @__lambda_N(%vredrs.value %p1, ..., ptr %__env)`
    /// Inside, parameters and captures are stored as Ty::Value slots; binary
    /// ops on Ty::Value dispatch to runtime helpers (vredrs_value_add etc.),
    /// so the body expression works without explicit untagging.
    ///
    /// Limitation: the env array is `alloca`'d in the enclosing function, so
    /// a closure that outlives its enclosing function (e.g., returned from
    /// it) will reference dangling memory. Heap allocation would be needed
    /// for full correctness — tracked as a known 1.0 limitation.
    fn gen_lambda(
        &mut self,
        l: &LambdaExpr,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let name = format!("__lambda_{}", self.lambda_n);
        self.lambda_n += 1;
        let n_params = l.params.len();
        // Detect free variables in the lambda body that are present in the
        // enclosing scope. These become the closure's captures.
        let bound: std::collections::HashSet<String> =
            l.params.iter().map(|p| p.name.name.clone()).collect();
        let free_vars = self.collect_free_vars(&l.body, &bound);
        let captures: Vec<String> = free_vars
            .into_iter()
            .filter(|name| vars.contains_key(name))
            .collect();
        // Stash info for the closure slot at the call site.
        let sig = Sig {
            params: vec![Ty::Value; n_params],
            ret: Ty::Value,
        };
        self.functions.insert(name.clone(), sig.clone());
        self.lambda_captures
            .insert(name.clone(), captures.clone());
        // Emit the lambda body to a side buffer so it ends up at module scope
        // (not nested inside the enclosing function).
        let saved_buf = std::mem::take(&mut self.buf);
        self.emit_lambda_function(&name, &l.params, &l.body)?;
        let body = std::mem::take(&mut self.buf);
        self.buf = saved_buf;
        self.lambda_bodies.push(body);
        // Build the closure struct { ptr fn, ptr env } on the stack.
        self.buf
            .push_str(&format!("  ; build closure {} ({} captures)\n", name, captures.len()));
        // Allocate the env array [num_captures x %vredrs.value] (if any).
        let env_ptr = if captures.is_empty() {
            "null".to_string()
        } else {
            let env_alloca = self.new_local(&format!("__{}_env", name));
            self.buf.push_str(&format!(
                "  {} = alloca [{} x %vredrs.value], align 8\n",
                env_alloca,
                captures.len()
            ));
            // Store each captured value (wrapped as %vredrs.value) into env[i].
            for (i, cap_name) in captures.iter().enumerate() {
                let cap_val = self.gen_expr(&Expr::Identifier(Identifier {
                    name: cap_name.clone(),
                    span: crate::error::Span::dummy(),
                }), vars)?;
                let wv = self.wrap_value(cap_val)?;
                let slot_addr = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = getelementptr inbounds [{} x %vredrs.value], ptr {}, i64 0, i64 {}\n",
                    slot_addr,
                    captures.len(),
                    env_alloca,
                    i
                ));
                self.buf.push_str(&format!(
                    "  store %vredrs.value {}, %vredrs.value* {}, align 8\n",
                    wv.repr, slot_addr
                ));
            }
            env_alloca
        };
        // Allocate the closure struct { ptr fn, ptr env }.
        let closure_alloca = self.new_local(&format!("__{}_closure", name));
        self.buf.push_str(&format!(
            "  {} = alloca {{ ptr, ptr }}, align 8\n",
            closure_alloca
        ));
        // Store fn pointer at offset 0.
        let fn_slot = self.new_var();
        self.buf.push_str(&format!(
            "  {} = getelementptr inbounds {{ ptr, ptr }}, ptr {}, i64 0, i32 0\n",
            fn_slot, closure_alloca
        ));
        self.buf.push_str(&format!(
            "  store ptr @{}, ptr {}, align 8\n",
            self.safe(&name),
            fn_slot
        ));
        // Store env pointer at offset 1.
        let env_slot = self.new_var();
        self.buf.push_str(&format!(
            "  {} = getelementptr inbounds {{ ptr, ptr }}, ptr {}, i64 0, i32 1\n",
            env_slot, closure_alloca
        ));
        if env_ptr == "null" {
            self.buf.push_str(&format!(
                "  store ptr null, ptr {}, align 8\n",
                env_slot
            ));
        } else {
            self.buf.push_str(&format!(
                "  store ptr {}, ptr {}, align 8\n",
                env_ptr, env_slot
            ));
        }
        // The closure value is the struct pointer cast to i64.
        let ptr = self.new_var();
        self.buf.push_str(&format!(
            "  {} = ptrtoint ptr {} to i64\n",
            ptr, closure_alloca
        ));
        // Remember the signature so call sites can form the right indirect call.
        // We use a side map keyed by the SSA register name.
        self.closure_sigs.insert(ptr.clone(), sig);
        Ok(Val::new(Ty::Fn, ptr))
    }
    /// Load the fn pointer and env pointer from a closure value.
    /// The closure value is an i64 holding the address of a
    /// `{ ptr fn, ptr env }` struct. Returns `(fn_ptr_typed_ssa, env_ssa_repr)`
    /// where `fn_ptr_typed_ssa` is the fn pointer bitcast to `fn_ty`, and
    /// `env_ssa_repr` is the SSA register name (or `null`) holding the env ptr.
    fn load_closure_pointers(
        &mut self,
        closure_repr: &str,
        fn_ty: &str,
    ) -> Result<(String, String)> {
        let closure_ptr = self.new_var();
        self.buf.push_str(&format!(
            "  {} = inttoptr i64 {} to ptr\n",
            closure_ptr, closure_repr
        ));
        let fn_ptr_raw = self.new_var();
        self.buf.push_str(&format!(
            "  {} = load ptr, ptr {}, align 8\n",
            fn_ptr_raw, closure_ptr
        ));
        let env_ptr_addr = self.new_var();
        self.buf.push_str(&format!(
            "  {} = getelementptr inbounds {{ ptr, ptr }}, ptr {}, i64 0, i32 1\n",
            env_ptr_addr, closure_ptr
        ));
        let env_ptr = self.new_var();
        self.buf.push_str(&format!(
            "  {} = load ptr, ptr {}, align 8\n",
            env_ptr, env_ptr_addr
        ));
        let fn_ptr_typed = self.new_var();
        self.buf.push_str(&format!(
            "  {} = bitcast ptr {} to {}\n",
            fn_ptr_typed, fn_ptr_raw, fn_ty
        ));
        Ok((fn_ptr_typed, env_ptr))
    }


}
