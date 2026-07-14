impl FullLlvmGen {
    fn gen_set_literal(
        &mut self,
        elements: &[Expr],
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let s = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call %vredrs.dict* @vredrs_dict_new()\n",
            s
        ));
        for e in elements {
            let v = self.gen_expr(e, vars)?;
            let key = self.to_str(v, vars)?;
            self.buf.push_str(&format!(
                "  call void @vredrs_set_add(%vredrs.dict* {}, %vredrs.str* {})\n",
                s, key.repr
            ));
        }
        Ok(Val::new(Ty::Set, s))
    }

    fn gen_list_comprehension(
        &mut self,
        lc: &ListComprehension,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let list = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call %vredrs.list* @vredrs_list_new()\n",
            list
        ));
        // Build iteration: for item in iterable { if cond { push result_expr } }
        let iter = self.gen_expr(&lc.iterable, vars)?;
        let len_fn = match iter.ty {
            Ty::List => "@vredrs_list_len",
            Ty::Tuple => "@vredrs_tuple_len",
            _ => {
                return Err(CompilerError::codegen_error(
                    "list comprehension iterable must be list or tuple",
                ))
            }
        };
        let len_var = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call i64 {}({} {})\n",
            len_var,
            len_fn,
            iter.ty.ir(),
            iter.repr
        ));
        let idx_ptr = self.new_local("__lc_idx");
        self.buf
            .push_str(&format!("  {} = alloca i64, align 8\n", idx_ptr));
        self.buf
            .push_str(&format!("  store i64 0, i64* {}, align 8\n", idx_ptr));
        let cond_lbl = self.new_lbl();
        let body_lbl = self.new_lbl();
        let step_lbl = self.new_lbl();
        let end_lbl = self.new_lbl();
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", cond_lbl, cond_lbl));
        let cur_idx = self.new_var();
        self.buf.push_str(&format!(
            "  {} = load i64, i64* {}, align 8\n",
            cur_idx, idx_ptr
        ));
        let cmp = self.new_var();
        self.buf.push_str(&format!(
            "  {} = icmp slt i64 {}, {}\n",
            cmp, cur_idx, len_var
        ));
        self.buf.push_str(&format!(
            "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
            cmp, body_lbl, end_lbl, body_lbl
        ));
        let get_fn = if iter.ty == Ty::List {
            "@vredrs_list_get"
        } else {
            "@vredrs_tuple_get"
        };
        let item = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call %vredrs.value {}({} {}, i64 {})\n",
            item,
            get_fn,
            iter.ty.ir(),
            iter.repr,
            cur_idx
        ));
        // Bind item to var name. Type is %vredrs.value (tagged).
        self.store_local(&lc.var.name, Val::new(Ty::Value, item), vars)?;
        // Optional condition
        if let Some(cond) = &lc.condition {
            let c_raw = self.gen_expr(cond, vars)?;
            let c = self.to_bool(c_raw)?;
            let push_lbl = self.new_lbl();
            let skip_lbl = self.new_lbl();
            self.buf.push_str(&format!(
                "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
                c.repr, push_lbl, skip_lbl, push_lbl
            ));
            let v = self.gen_expr(&lc.result_expr, vars)?;
            let wv = self.wrap_value(v)?;
            self.buf.push_str(&format!(
                "  call void @vredrs_list_push(%vredrs.list* {}, %vredrs.value {})\n",
                list, wv.repr
            ));
            self.buf
                .push_str(&format!("  br label %L{}\nL{}:\n", skip_lbl, skip_lbl));
        } else {
            let v = self.gen_expr(&lc.result_expr, vars)?;
            let wv = self.wrap_value(v)?;
            self.buf.push_str(&format!(
                "  call void @vredrs_list_push(%vredrs.list* {}, %vredrs.value {})\n",
                list, wv.repr
            ));
        }
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", step_lbl, step_lbl));
        let cur2 = self.new_var();
        self.buf.push_str(&format!(
            "  {} = load i64, i64* {}, align 8\n",
            cur2, idx_ptr
        ));
        let next = self.new_var();
        self.buf
            .push_str(&format!("  {} = add i64 {}, 1\n", next, cur2));
        self.buf.push_str(&format!(
            "  store i64 {}, i64* {}, align 8\n",
            next, idx_ptr
        ));
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", cond_lbl, end_lbl));
        Ok(Val::new(Ty::List, list))
    }

    fn gen_dict_comprehension(
        &mut self,
        dc: &DictComprehension,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let d = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call %vredrs.dict* @vredrs_dict_new()\n",
            d
        ));
        let iter = self.gen_expr(&dc.iterable, vars)?;
        let len_fn = match iter.ty {
            Ty::List => "@vredrs_list_len",
            Ty::Tuple => "@vredrs_tuple_len",
            _ => {
                return Err(CompilerError::codegen_error(
                    "dict comprehension iterable must be list or tuple",
                ))
            }
        };
        let len_var = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call i64 {}({} {})\n",
            len_var,
            len_fn,
            iter.ty.ir(),
            iter.repr
        ));
        let idx_ptr = self.new_local("__dc_idx");
        self.buf
            .push_str(&format!("  {} = alloca i64, align 8\n", idx_ptr));
        self.buf
            .push_str(&format!("  store i64 0, i64* {}, align 8\n", idx_ptr));
        let cond_lbl = self.new_lbl();
        let body_lbl = self.new_lbl();
        let step_lbl = self.new_lbl();
        let end_lbl = self.new_lbl();
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", cond_lbl, cond_lbl));
        let cur_idx = self.new_var();
        self.buf.push_str(&format!(
            "  {} = load i64, i64* {}, align 8\n",
            cur_idx, idx_ptr
        ));
        let cmp = self.new_var();
        self.buf.push_str(&format!(
            "  {} = icmp slt i64 {}, {}\n",
            cmp, cur_idx, len_var
        ));
        self.buf.push_str(&format!(
            "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
            cmp, body_lbl, end_lbl, body_lbl
        ));
        let get_fn = if iter.ty == Ty::List {
            "@vredrs_list_get"
        } else {
            "@vredrs_tuple_get"
        };
        let item = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call %vredrs.value {}({} {}, i64 {})\n",
            item,
            get_fn,
            iter.ty.ir(),
            iter.repr,
            cur_idx
        ));
        self.store_local(&dc.var.name, Val::new(Ty::Value, item), vars)?;
        if let Some(cond) = &dc.condition {
            let c_raw = self.gen_expr(cond, vars)?;
            let c = self.to_bool(c_raw)?;
            let push_lbl = self.new_lbl();
            let skip_lbl = self.new_lbl();
            self.buf.push_str(&format!(
                "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
                c.repr, push_lbl, skip_lbl, push_lbl
            ));
            let k = self.gen_expr(&dc.key_expr, vars)?;
            let key = self.to_str(k, vars)?;
            let v = self.gen_expr(&dc.value_expr, vars)?;
            let wv = self.wrap_value(v)?;
            self.buf.push_str(&format!("  call void @vredrs_dict_set(%vredrs.dict* {}, %vredrs.str* {}, %vredrs.value {})\n", d, key.repr, wv.repr));
            self.buf
                .push_str(&format!("  br label %L{}\nL{}:\n", skip_lbl, skip_lbl));
        } else {
            let k = self.gen_expr(&dc.key_expr, vars)?;
            let key = self.to_str(k, vars)?;
            let v = self.gen_expr(&dc.value_expr, vars)?;
            let wv = self.wrap_value(v)?;
            self.buf.push_str(&format!("  call void @vredrs_dict_set(%vredrs.dict* {}, %vredrs.str* {}, %vredrs.value {})\n", d, key.repr, wv.repr));
        }
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", step_lbl, step_lbl));
        let cur2 = self.new_var();
        self.buf.push_str(&format!(
            "  {} = load i64, i64* {}, align 8\n",
            cur2, idx_ptr
        ));
        let next = self.new_var();
        self.buf
            .push_str(&format!("  {} = add i64 {}, 1\n", next, cur2));
        self.buf.push_str(&format!(
            "  store i64 {}, i64* {}, align 8\n",
            next, idx_ptr
        ));
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", cond_lbl, end_lbl));
        Ok(Val::new(Ty::Dict, d))
    }

    fn gen_range_expr(
        &mut self,
        r: &RangeExpr,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let start = match &r.start {
            Some(s) => {
                let v = self.gen_expr(s, vars)?;
                self.cast(v, &Ty::I64)?
            }
            None => Val::new(Ty::I64, "0"),
        };
        let end = match &r.end {
            Some(e) => {
                let v = self.gen_expr(e, vars)?;
                self.cast(v, &Ty::I64)?
            }
            None => Val::new(Ty::I64, "0"),
        };
        let list = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call %vredrs.list* @vredrs_range(i64 {}, i64 {})\n",
            list, start.repr, end.repr
        ));
        Ok(Val::new(Ty::List, list))
    }

    fn gen_ternary(
        &mut self,
        t: &TernaryExpr,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let c_raw = self.gen_expr(&t.condition, vars)?;
        let c = self.to_bool(c_raw)?;
        let true_lbl = self.new_lbl();
        let false_lbl = self.new_lbl();
        let end_lbl = self.new_lbl();
        self.buf.push_str(&format!(
            "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
            c.repr, true_lbl, false_lbl, true_lbl
        ));
        let tv = self.gen_expr(&t.true_branch, vars)?;
        let slot = self.new_local("__tern");
        self.buf
            .push_str(&format!("  {} = alloca {}, align 8\n", slot, tv.ty.ir()));
        self.buf.push_str(&format!(
            "  store {} {}, {}* {}, align 8\n",
            tv.ty.ir(),
            tv.repr,
            tv.ty.ir(),
            slot
        ));
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", end_lbl, false_lbl));
        let fv = self.gen_expr(&t.false_branch, vars)?;
        let fv_cast = self.cast(fv, &tv.ty)?;
        self.buf.push_str(&format!(
            "  store {} {}, {}* {}, align 8\n",
            tv.ty.ir(),
            fv_cast.repr,
            tv.ty.ir(),
            slot
        ));
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", end_lbl, end_lbl));
        let res = self.new_var();
        self.buf.push_str(&format!(
            "  {} = load {}, {}* {}, align 8\n",
            res,
            tv.ty.ir(),
            tv.ty.ir(),
            slot
        ));
        Ok(Val::new(tv.ty, res))
    }

    fn gen_unary(&mut self, u: &UnaryExpr, vars: &mut HashMap<String, LocalSlot>) -> Result<Val> {
        let operand = self.gen_expr(&u.operand, vars)?;
        match u.operator {
            UnaryOp::Neg => match operand.ty {
                Ty::I64 => {
                    let t = self.new_var();
                    self.buf
                        .push_str(&format!("  {} = sub i64 0, {}\n", t, operand.repr));
                    Ok(Val::new(Ty::I64, t))
                }
                Ty::F64 => {
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = fsub double -0.000000e+00, {}\n",
                        t, operand.repr
                    ));
                    Ok(Val::new(Ty::F64, t))
                }
                _ => Err(CompilerError::codegen_error("unary minus needs numeric")),
            },
            UnaryOp::Not | UnaryOp::Bang => {
                let b = self.to_bool(operand)?;
                let t = self.new_var();
                self.buf
                    .push_str(&format!("  {} = xor i1 {}, true\n", t, b.repr));
                Ok(Val::new(Ty::Bool, t))
            }
        }
    }

    fn gen_call(&mut self, c: &CallExpr, vars: &mut HashMap<String, LocalSlot>) -> Result<Val> {
        // Special case: annotations(fn_name) — we need the function's source
        // name (not its lowered value) to look up compile-time annotation
        // metadata. Intercept before generic arg evaluation.
        if let Expr::Identifier(id) = c.callee.as_ref() {
            if id.name == "annotations" && c.args.len() == 1 {
                if let Expr::Identifier(arg_id) = &c.args[0] {
                    let fname = &arg_id.name;
                    if let Some(json) = self.fn_annotations.get(fname).cloned() {
                        return self.gen_annotations_dict(&json);
                    }
                    // Function exists but has no annotations → return empty dict.
                    if self.functions.contains_key(fname) {
                        return self.gen_annotations_dict("{}");
                    }
                    return Err(CompilerError::codegen_error(format!(
                        "annotations() target '{}' is not a known function",
                        fname
                    )));
                }
            }
        }
        let args: Vec<Val> = {
            let mut out = Vec::new();
            for a in &c.args {
                out.push(self.gen_expr(a, vars)?);
            }
            out
        };
        self.gen_call_with_args(c, args, vars)
    }

    /// Materialize a runtime dict from a JSON-encoded annotations string.
    /// The JSON is parsed at IR runtime by `vredrs_dict_from_json` (added
    /// to the C runtime). For 1.0 we use a simple parser that handles flat
    /// objects of string keys to string/number/bool values.
    fn gen_annotations_dict(&mut self, json: &str) -> Result<Val> {
        let d = self.new_var();
        self.buf
            .push_str(&format!("  {} = call %vredrs.dict* @vredrs_dict_new()\n", d));
        // Parse the JSON inline at compile time and emit direct dict_set
        // calls. This avoids needing a JSON parser in the C runtime.
        let parsed = parse_simple_json(json);
        for (k, v) in &parsed {
            let k_name = self.intern(k);
            let k_len = k.as_bytes().len() + 1;
            let k_ptr = self.new_var();
            self.buf.push_str(&format!(
                "  {} = getelementptr inbounds [{} x i8], ptr {}, i64 0, i64 0\n",
                k_ptr, k_len, k_name
            ));
            let k_str = self.new_var();
            self.buf.push_str(&format!(
                "  {} = call %vredrs.str* @vredrs_str_from_cstr(ptr {})\n",
                k_str, k_ptr
            ));
            // Value: if it's a string literal, intern it; if numeric, make i64.
            let val_repr = if v.starts_with('"') && v.ends_with('"') {
                // String value.
                let inner = &v[1..v.len() - 1];
                let v_name = self.intern(inner);
                let v_len = inner.as_bytes().len() + 1;
                let v_ptr = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = getelementptr inbounds [{} x i8], ptr {}, i64 0, i64 0\n",
                    v_ptr, v_len, v_name
                ));
                let v_str = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_from_cstr(ptr {})\n",
                    v_str, v_ptr
                ));
                let wv = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.value @vredrs_value_make_str(%vredrs.str* {})\n",
                    wv, v_str
                ));
                format!("%vredrs.value {}", wv)
            } else if v == "true" || v == "false" {
                let b: i64 = if v == "true" { 1 } else { 0 };
                let wv = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.value @vredrs_value_make_bool(i64 {})\n",
                    wv, b
                ));
                format!("%vredrs.value {}", wv)
            } else if let Ok(n) = v.parse::<i64>() {
                let wv = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.value @vredrs_value_make_i64(i64 {})\n",
                    wv, n
                ));
                format!("%vredrs.value {}", wv)
            } else {
                // Fallback: treat as string.
                let v_name = self.intern(v);
                let v_len = v.as_bytes().len() + 1;
                let v_ptr = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = getelementptr inbounds [{} x i8], ptr {}, i64 0, i64 0\n",
                    v_ptr, v_len, v_name
                ));
                let v_str = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_from_cstr(ptr {})\n",
                    v_str, v_ptr
                ));
                let wv = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.value @vredrs_value_make_str(%vredrs.str* {})\n",
                    wv, v_str
                ));
                format!("%vredrs.value {}", wv)
            };
            self.buf.push_str(&format!(
                "  call void @vredrs_dict_set(%vredrs.dict* {}, %vredrs.str* {}, {})\n",
                d, k_str, val_repr
            ));
        }
        Ok(Val::new(Ty::Dict, d))
    }

    fn gen_call_with_args(
        &mut self,
        c: &CallExpr,
        args: Vec<Val>,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        // Special case: `recv.method(args)` is parsed as Call { callee: MemberAccess(recv, method) }.
        // Lower it as a method call.
        if let Expr::MemberAccess(m) = c.callee.as_ref() {
            let mc = MethodCallExpr {
                receiver: m.target.clone(),
                method: m.member.clone(),
                args: c.args.clone(),
                span: c.span.clone(),
            };
            // Note: we already evaluated args above, but gen_method_call re-evaluates.
            // That's fine for side-effect-free expressions; for side-effecting
            // ones, we accept the double-evaluation as a known limitation.
            let _ = args; // discard pre-evaluated args
            return self.gen_method_call(&mc, vars);
        }
        let func_name = match c.callee.as_ref() {
            Expr::Identifier(id) => id.name.clone(),
            other => {
                let callee = self.gen_expr(other, vars)?;
                if callee.ty == Ty::Obj {
                    let class = callee.class.clone().ok_or_else(|| {
                        CompilerError::codegen_error("object __call__ needs class")
                    })?;
                    return self.emit_vtable_call(callee, &class, "__call__", args);
                }
                if callee.ty == Ty::Fn {
                    // Indirect call: the closure value is an i64 pointer to a
                    // `{ ptr fn, ptr env }` struct. Load both fields, cast fn
                    // to the uniform closure signature
                    // `%vredrs.value (%vredrs.value, ..., ptr)*`, and call
                    // with each arg wrapped as %vredrs.value plus the env ptr.
                    let sig = self.closure_sigs.get(&callee.repr).cloned().ok_or_else(
                        || CompilerError::codegen_error("closure call without sig"),
                    )?;
                    if sig.params.len() != args.len() {
                        return Err(CompilerError::codegen_error(format!(
                            "closure expects {} args, got {}",
                            sig.params.len(),
                            args.len()
                        )));
                    }
                    // Build the function pointer type string.
                    let mut fn_ty = String::from("%vredrs.value (");
                    for (i, p) in sig.params.iter().enumerate() {
                        if i > 0 {
                            fn_ty.push_str(", ");
                        }
                        // Each param is %vredrs.value (uniform signature).
                        let _ = p;
                        fn_ty.push_str("%vredrs.value");
                    }
                    if !sig.params.is_empty() {
                        fn_ty.push_str(", ");
                    }
                    fn_ty.push_str("ptr)*");
                    // Load fn ptr and env ptr from the closure struct.
                    let (fn_ptr_typed, env_repr) =
                        self.load_closure_pointers(&callee.repr, &fn_ty)?;
                    let mut call_args = Vec::new();
                    for a in args {
                        let wv = self.wrap_value(a)?;
                        call_args.push(format!("%vredrs.value {}", wv.repr));
                    }
                    call_args.push(format!("ptr {}", env_repr));
                    let ret = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call %vredrs.value {}({})\n",
                        ret, fn_ptr_typed, call_args.join(", ")
                    ));
                    return Ok(Val::new(Ty::Value, ret));
                }
                return Err(CompilerError::codegen_error(
                    "only direct fn calls supported",
                ));
            }
        };
        // Builtins
        if self.builtins.contains(func_name.as_str()) {
            return self.gen_builtin_call(&func_name, args, vars);
        }
        // Check if this is an imported stdlib symbol (e.g. "sqrt" from "math"
        // module → "math_sqrt"). Build the set of known module prefixes and
        // check if prepending any of them produces a known builtin.
        if !self.functions.contains_key(&func_name) && !self.classes.contains_key(&func_name) {
            for imp in &self.imports {
                if let Some(symbols) = &imp.symbols {
                    for sym in symbols {
                        if sym.name == func_name {
                            let full_name = format!("{}_{}", imp.module, func_name);
                            if self.builtins.contains(full_name.as_str()) {
                                return self.gen_builtin_call(&full_name, args, vars);
                            }
                        }
                    }
                }
            }
        }
        // Class constructor
        if self.classes.contains_key(&func_name) {
            return self.gen_class_new(&func_name, args, vars);
        }
        // If not a user function, check whether the identifier refers to a
        // local closure variable (Ty::Fn). If so, dispatch via indirect call.
        if !self.functions.contains_key(&func_name) {
            if let Some(slot) = vars.get(&func_name).cloned() {
                if slot.ty == Ty::Fn {
                    let callee = self.load_local(&func_name, vars)?;
                    let sig = slot.closure_sig.ok_or_else(|| {
                        CompilerError::codegen_error("closure call without sig")
                    })?;
                    if sig.params.len() != args.len() {
                        return Err(CompilerError::codegen_error(format!(
                            "closure expects {} args, got {}",
                            sig.params.len(),
                            args.len()
                        )));
                    }
                    let mut fn_ty = String::from("%vredrs.value (");
                    for (i, _p) in sig.params.iter().enumerate() {
                        if i > 0 {
                            fn_ty.push_str(", ");
                        }
                        fn_ty.push_str("%vredrs.value");
                    }
                    if !sig.params.is_empty() {
                        fn_ty.push_str(", ");
                    }
                    fn_ty.push_str("ptr)*");
                    let (fn_ptr_typed, env_repr) =
                        self.load_closure_pointers(&callee.repr, &fn_ty)?;
                    let mut call_args = Vec::new();
                    for a in args {
                        let wv = self.wrap_value(a)?;
                        call_args.push(format!("%vredrs.value {}", wv.repr));
                    }
                    call_args.push(format!("ptr {}", env_repr));
                    let ret = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call %vredrs.value {}({})\n",
                        ret, fn_ptr_typed, call_args.join(", ")
                    ));
                    return Ok(Val::new(Ty::Value, ret));
                }
            }
        }
        // User function
        let sig = self.functions.get(&func_name).cloned().ok_or_else(|| {
            CompilerError::codegen_error(format!("unknown function '{}'", func_name))
        })?;

        if sig.params.len() != args.len() {
            return Err(CompilerError::codegen_error(format!(
                "fn '{}' expects {} args, got {}",
                func_name,
                sig.params.len(),
                args.len()
            )));
        }
        let mut call_args = Vec::new();
        for (i, a) in args.into_iter().enumerate() {
            let casted = self.cast(a, &sig.params[i])?;
            call_args.push(format!("{} {}", sig.params[i].ir(), casted.repr));
        }
        if sig.ret == Ty::Void {
            self.buf.push_str(&format!(
                "  call void @{}({})\n",
                self.safe(&func_name),
                call_args.join(", ")
            ));
            Ok(Val::new(Ty::Void, ""))
        } else {
            let t = self.new_var();
            self.buf.push_str(&format!(
                "  {} = call {} @{}({})\n",
                t,
                sig.ret.ir(),
                self.safe(&func_name),
                call_args.join(", ")
            ));
            let mut v = Val::new(sig.ret.clone(), t);
            if sig.ret == Ty::Coro {
                v = v.with_async(func_name);
            }
            Ok(v)
        }
    }


}
