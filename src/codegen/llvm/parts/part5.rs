impl FullLlvmGen {
    fn gen_throw(&mut self, ts: &ThrowStmt, vars: &mut HashMap<String, LocalSlot>) -> Result<()> {
        let v = self.gen_expr(&ts.value, vars)?;
        match v.ty {
            Ty::Str => {
                self.buf.push_str(&format!(
                    "  call void @vredrs_throw_str(%vredrs.str* {})\n  unreachable\n",
                    v.repr
                ));
            }
            _ => {
                let i = self.cast(v, &Ty::I64)?;
                self.buf.push_str(&format!(
                    "  call void @vredrs_throw_i64(i64 {})\n  unreachable\n",
                    i.repr
                ));
            }
        }
        Ok(())
    }

    fn gen_with(
        &mut self,
        ws: &WithStmt,
        vars: &mut HashMap<String, LocalSlot>,
        ret_ty: &Ty,
    ) -> Result<()> {
        let manager = self.gen_expr(&ws.manager, vars)?;
        // Special case: integer file handle (returned by `open()`).
        // Call `vredrs_close(handle)` on exit; no __enter__/__exit__.
        if manager.ty == Ty::I64 {
            if let Some(var) = &ws.var {
                self.store_local(&var.name, manager.clone(), vars)?;
            }
            let body_lbl = self.new_lbl();
            let cleanup_lbl = self.new_lbl();
            let rethrow_lbl = self.new_lbl();
            let end_lbl = self.new_lbl();
            let buf = self.new_var();
            self.buf
                .push_str(&format!("  {} = call ptr @vredrs_try_begin()\n", buf));
            let old = self.new_var();
            self.buf
                .push_str(&format!("  {} = call ptr @vredrs_get_jmp_top()\n", old));
            self.buf
                .push_str(&format!("  call void @vredrs_set_jmp_top(ptr {})\n", buf));
            let code = self.new_var();
            self.buf
                .push_str(&format!("  {} = call i32 @setjmp(ptr {}) #0\n", code, buf));
            let normal = self.new_var();
            self.buf
                .push_str(&format!("  {} = icmp eq i32 {}, 0\n", normal, code));
            self.buf.push_str(&format!(
                "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
                normal, body_lbl, rethrow_lbl, body_lbl
            ));
            let term = self.gen_block(&ws.body, vars, ret_ty)?;
            if !term {
                self.buf
                    .push_str(&format!("  br label %L{}\n", cleanup_lbl));
            }
            self.buf.push_str(&format!("L{}:\n", cleanup_lbl));
            self.buf.push_str(&format!(
                "  call void @vredrs_close(i64 {})\n",
                manager.repr
            ));
            self.buf
                .push_str(&format!("  call void @vredrs_set_jmp_top(ptr {})\n", old));
            self.buf
                .push_str(&format!("  call void @vredrs_try_end(ptr {})\n", buf));
            self.buf
                .push_str(&format!("  br label %L{}\nL{}:\n", end_lbl, rethrow_lbl));
            self.buf.push_str(&format!(
                "  call void @vredrs_close(i64 {})\n",
                manager.repr
            ));
            self.buf
                .push_str(&format!("  call void @vredrs_set_jmp_top(ptr {})\n", old));
            self.buf
                .push_str(&format!("  call void @vredrs_try_end(ptr {})\n", buf));
            self.buf.push_str("  %with.ex = call ptr @vredrs_get_exception_str()\n  call void @vredrs_throw_str(ptr %with.ex)\n  unreachable\n");
            self.buf.push_str(&format!("L{}:\n", end_lbl));
            return Ok(());
        }
        if manager.ty != Ty::Obj {
            return Err(CompilerError::codegen_error(format!(
                "with manager must be object or file handle, got {:?}",
                manager.ty
            )));
        }
        let class = manager
            .class
            .clone()
            .ok_or_else(|| CompilerError::codegen_error("with manager needs known class"))?;
        let entered = if self.class_has_method(&class, "__enter__") {
            self.emit_vtable_call(manager.clone(), &class, "__enter__", vec![])?
        } else {
            manager.clone()
        };
        if let Some(var) = &ws.var {
            self.store_local(&var.name, entered, vars)?;
        }
        // Call __exit__ / close on normal exit. Exception path: install setjmp.
        let body_lbl = self.new_lbl();
        let cleanup_lbl = self.new_lbl();
        let rethrow_lbl = self.new_lbl();
        let end_lbl = self.new_lbl();
        let buf = self.new_var();
        self.buf
            .push_str(&format!("  {} = call ptr @vredrs_try_begin()\n", buf));
        let old = self.new_var();
        self.buf
            .push_str(&format!("  {} = call ptr @vredrs_get_jmp_top()\n", old));
        self.buf
            .push_str(&format!("  call void @vredrs_set_jmp_top(ptr {})\n", buf));
        let code = self.new_var();
        self.buf
            .push_str(&format!("  {} = call i32 @setjmp(ptr {}) #0\n", code, buf));
        let normal = self.new_var();
        self.buf
            .push_str(&format!("  {} = icmp eq i32 {}, 0\n", normal, code));
        self.buf.push_str(&format!(
            "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
            normal, body_lbl, rethrow_lbl, body_lbl
        ));
        let term = self.gen_block(&ws.body, vars, ret_ty)?;
        if !term {
            self.buf
                .push_str(&format!("  br label %L{}\n", cleanup_lbl));
        }
        self.buf.push_str(&format!("L{}:\n", cleanup_lbl));
        self.emit_exit_or_close(manager.clone(), &class)?;
        self.buf
            .push_str(&format!("  call void @vredrs_set_jmp_top(ptr {})\n", old));
        self.buf
            .push_str(&format!("  call void @vredrs_try_end(ptr {})\n", buf));
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", end_lbl, rethrow_lbl));
        self.emit_exit_or_close(manager, &class)?;
        self.buf
            .push_str(&format!("  call void @vredrs_set_jmp_top(ptr {})\n", old));
        self.buf
            .push_str(&format!("  call void @vredrs_try_end(ptr {})\n", buf));
        self.buf.push_str("  %with.ex = call ptr @vredrs_get_exception_str()\n  call void @vredrs_throw_str(ptr %with.ex)\n  unreachable\n");
        self.buf.push_str(&format!("L{}:\n", end_lbl));
        Ok(())
    }

    fn emit_exit_or_close(&mut self, manager: Val, class: &str) -> Result<()> {
        if self.class_has_method(class, "__exit__") {
            // Match the method's arity: pass a null error to 1-arg __exit__,
            // nothing to 0-arg __exit__.
            let m = self.method_info(class, "__exit__")?.clone();
            let args: Vec<Val> = if m.sig.params.is_empty() {
                vec![]
            } else {
                vec![Val::new(Ty::Obj, "null"); m.sig.params.len()]
            };
            let _ = self.emit_vtable_call(manager, class, "__exit__", args)?;
        } else if self.class_has_method(class, "close") {
            let _ = self.emit_vtable_call(manager, class, "close", vec![])?;
        } else {
            return Err(CompilerError::codegen_error(format!(
                "with manager class '{}' has no __exit__ or close",
                class
            )));
        }
        Ok(())
    }

    fn gen_assert(
        &mut self,
        as_: &AssertStmt,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<()> {
        let c_raw = self.gen_expr(&as_.condition, vars)?;
        let cond = self.to_bool(c_raw)?;
        let ok_lbl = self.new_lbl();
        let bad_lbl = self.new_lbl();
        self.buf.push_str(&format!(
            "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
            cond.repr, ok_lbl, bad_lbl, bad_lbl
        ));
        self.buf
            .push_str("  call void @vredrs_throw_i64(i64 -1)\n  unreachable\n");
        self.buf.push_str(&format!("L{}:\n", ok_lbl));
        Ok(())
    }

    fn gen_yield(&mut self, ys: &YieldStmt, vars: &mut HashMap<String, LocalSlot>) -> Result<()> {
        if !vars.contains_key("__coro") {
            return Err(CompilerError::codegen_error(
                "yield outside async fn or generator",
            ));
        }
        let is_generator = vars
            .get("__coro")
            .and_then(|s| s.async_fn.as_ref())
            .is_some();
        let v = if let Some(v) = &ys.value {
            self.gen_expr(v, vars)?
        } else {
            Val::new(Ty::I64, "0")
        };
        if is_generator {
            let wv = self.wrap_value(v)?;
            let list_var = self.new_var();
            self.buf.push_str(&format!(
                "  {} = call %vredrs.list* @vredrs_coro_get_list(%vredrs.coro* %coro)\n",
                list_var
            ));
            self.buf.push_str(&format!(
                "  call void @vredrs_list_push(%vredrs.list* {}, %vredrs.value {})\n",
                list_var, wv.repr
            ));
        } else {
            let i64_v = self.cast(v, &Ty::I64)?;
            self.buf.push_str(&format!(
                "  call void @vredrs_coro_store_result(%vredrs.coro* %coro, i64 {})\n",
                i64_v.repr
            ));
            self.buf.push_str(
                "  call void @vredrs_coro_set_state(%vredrs.coro* %coro, i64 1)\n  ret i1 false\n",
            );
        }
        Ok(())
    }

    fn gen_spawn(&mut self, sp: &SpawnStmt, vars: &mut HashMap<String, LocalSlot>) -> Result<()> {
        // spawn lowers to calling the async fn constructor and immediately
        // polling once. The result is discarded; full coroutine scheduling
        // is out of scope for the native backend.
        let _ = self.gen_expr(&sp.call, vars)?;
        Ok(())
    }

    fn emit_default_ret(&mut self, ty: &Ty) -> Result<()> {
        match ty {
            Ty::Void => self.buf.push_str("  ret void\n"),
            Ty::I64 => self.buf.push_str("  ret i64 0\n"),
            Ty::F64 => self.buf.push_str("  ret double 0.000000e+00\n"),
            Ty::Bool => self.buf.push_str("  ret i1 0\n"),
            Ty::Str => self.buf.push_str("  ret %vredrs.str* null\n"),
            Ty::List => self.buf.push_str("  ret %vredrs.list* null\n"),
            Ty::Dict | Ty::Set => self.buf.push_str("  ret %vredrs.dict* null\n"),
            Ty::Tuple => self.buf.push_str("  ret %vredrs.tuple* null\n"),
            Ty::Obj => self.buf.push_str("  ret %vredrs.object* null\n"),
            Ty::Coro => self.buf.push_str("  ret %vredrs.coro* null\n"),
            Ty::Ptr => self.buf.push_str("  ret i8* null\n"),
            Ty::Value => self.buf.push_str("  ret %vredrs.value zeroinitializer\n"),
            Ty::Fn => self.buf.push_str("  ret i64 0\n"),
        }
        Ok(())
    }

    fn emit_print_arg(&mut self, arg: &Expr, vars: &mut HashMap<String, LocalSlot>) -> Result<()> {
        let v = self.gen_expr(arg, vars)?;
        match v.ty {
            Ty::I64 => self
                .buf
                .push_str(&format!("  call i64 @vredrs_print_i64(i64 {})\n", v.repr)),
            Ty::F64 => self.buf.push_str(&format!(
                "  call i64 @vredrs_print_f64(double {})\n",
                v.repr
            )),
            Ty::Bool => {
                let i64_v = self.cast(v, &Ty::I64)?;
                self.buf.push_str(&format!(
                    "  call i64 @vredrs_print_bool(i64 {})\n",
                    i64_v.repr
                ));
            }
            Ty::Str => self.buf.push_str(&format!(
                "  call i64 @vredrs_print_str(%vredrs.str* {})\n",
                v.repr
            )),
            Ty::Value => {
                self.buf.push_str(&format!(
                    "  call i64 @vredrs_print_value(%vredrs.value {})\n",
                    v.repr
                ));
            }
            Ty::Obj => {
                let s = self.to_str(v, vars)?;
                self.buf.push_str(&format!(
                    "  call i64 @vredrs_print_str(%vredrs.str* {})\n",
                    s.repr
                ));
            }
            _ => {
                let s = self.to_str(v, vars)?;
                self.buf.push_str(&format!(
                    "  call i64 @vredrs_print_str(%vredrs.str* {})\n",
                    s.repr
                ));
            }
        }
        Ok(())
    }
}

impl FullLlvmGen {
    /* ===================================================================
     * Expression generation
     * ================================================================= */

    fn gen_expr(&mut self, e: &Expr, vars: &mut HashMap<String, LocalSlot>) -> Result<Val> {
        match e {
            Expr::Integer(i) => Ok(Val::new(Ty::I64, i.value.to_string())),
            Expr::Float(f) => Ok(Val::new(Ty::F64, format_float(f.value))),
            Expr::Bool(b) => Ok(Val::new(Ty::Bool, if b.value { "1" } else { "0" })),
            Expr::Null(_) => Ok(Val::new(Ty::Obj, "null")),
            Expr::Identifier(id) => self.load_local(&id.name, vars),
            Expr::String_(s) => {
                if s.parts.iter().any(|p| matches!(p, StringPart::Interpolation(_))) {
                    self.gen_string_literal_interp(&s.parts, vars)
                } else {
                    self.gen_string_literal(&s.parts)
                }
            }
            Expr::MultiLineString(s) => {
                if s.parts.iter().any(|p| matches!(p, StringPart::Interpolation(_))) {
                    self.gen_string_literal_interp(&s.parts, vars)
                } else {
                    self.gen_string_literal(&s.parts)
                }
            }
            Expr::Binary(b) => {
                let l = self.gen_expr(&b.left, vars)?;
                let r = self.gen_expr(&b.right, vars)?;
                self.gen_binary(l, r, &b.operator)
            }
            Expr::Unary(u) => self.gen_unary(u, vars),
            Expr::Call(c) => self.gen_call(c, vars),
            Expr::MethodCall(m) => self.gen_method_call(m, vars),
            Expr::Index(i) => self.gen_index(i, vars),
            Expr::MemberAccess(m) => self.gen_member(m, vars),
            Expr::Await(a) => self.gen_await(a, vars),
            Expr::Cast(c) => self.gen_expr(&c.expr, vars),
            Expr::List(l) => self.gen_list_literal(&l.elements, vars),
            Expr::Tuple(t) => self.gen_tuple_literal(&t.elements, vars),
            Expr::Dict(d) => self.gen_dict_literal(&d.entries, vars),
            Expr::Set(s) => self.gen_set_literal(&s.elements, vars),
            Expr::ListComprehension(lc) => self.gen_list_comprehension(lc, vars),
            Expr::DictComprehension(dc) => self.gen_dict_comprehension(dc, vars),
            Expr::Range(r) => self.gen_range_expr(r, vars),
            Expr::Ternary(t) => self.gen_ternary(t, vars),
            Expr::Slice(s) => self.gen_slice(s, vars),
            Expr::Lambda(l) => self.gen_lambda(l, vars),
            Expr::Spawn(sp) => self.gen_spawn_expr(sp, vars),
            Expr::Coro(c) => self.gen_coro_expr(c, vars),
            Expr::Resume(r) => self.gen_resume_expr(r, vars),
            Expr::Pipe(p) => {
                // pipe: left |> right(args...)  ==>  right(left, args...)
                let left = self.gen_expr(&p.left, vars)?;
                if let Expr::Call(call) = p.right.as_ref() {
                    let mut all_args = vec![left];
                    for a in &call.args {
                        all_args.push(self.gen_expr(a, vars)?);
                    }
                    return self.gen_call_with_args(call, all_args, vars);
                }
                Err(CompilerError::codegen_error("pipe target must be call"))
            }
            Expr::NullCoalesce(nc) => {
                let l = self.gen_expr(&nc.left, vars)?;
                let r_expr = &nc.right;
                let l_bool = self.to_bool(l.clone())?;
                let true_lbl = self.new_lbl();
                let false_lbl = self.new_lbl();
                let end_lbl = self.new_lbl();
                self.buf.push_str(&format!(
                    "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
                    l_bool.repr, end_lbl, false_lbl, false_lbl
                ));
                let r = self.gen_expr(r_expr, vars)?;
                let r_slot = self.new_local("__nc_r");
                self.buf
                    .push_str(&format!("  {} = alloca {}, align 8\n", r_slot, r.ty.ir()));
                self.buf.push_str(&format!(
                    "  store {} {}, {}* {}, align 8\n",
                    r.ty.ir(),
                    r.repr,
                    r.ty.ir(),
                    r_slot
                ));
                self.buf
                    .push_str(&format!("  br label %L{}\nL{}:\n", end_lbl, true_lbl));
                let res = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = load {}, {}* {}, align 8\n",
                    res,
                    r.ty.ir(),
                    r.ty.ir(),
                    r_slot
                ));
                Ok(Val::new(r.ty, res))
            }
            other => Err(CompilerError::codegen_error(format!(
                "expr {:?} not lowered",
                self.expr_kind(other)
            ))),
        }
    }

    fn gen_string_literal(&mut self, parts: &[StringPart]) -> Result<Val> {
        // Fast path: pure-text literal (no interpolation) — intern once.
        if !parts.iter().any(|p| matches!(p, StringPart::Interpolation(_))) {
            let text = self.static_string_text(parts)?;
            let name = self
                .strs
                .get(&text)
                .cloned()
                .ok_or_else(|| CompilerError::codegen_error("string literal not interned"))?;
            let len = text.as_bytes().len() + 1;
            let ptr = self.new_var();
            self.buf.push_str(&format!(
                "  {} = getelementptr inbounds [{} x i8], ptr {}, i64 0, i64 0\n",
                ptr, len, name
            ));
            let s = self.new_var();
            self.buf.push_str(&format!(
                "  {} = call %vredrs.str* @vredrs_str_from_cstr(ptr {})\n",
                s, ptr
            ));
            return Ok(Val::new(Ty::Str, s));
        }
        // Interpolation path: build the string by concatenating each part.
        // Start with empty string, then `vredrs_str_concat` each text part and
        // each interpolated expression (converted to string via to_str).
        // Note: this method needs vars to evaluate interpolations, so we
        // require the caller (gen_expr) to dispatch the interpolation case
        // to gen_string_literal_interp below.
        Err(CompilerError::codegen_error(
            "string interpolation requires vars context — use gen_string_literal_interp",
        ))
    }

    /// Lower a string literal that may contain interpolations. Each
    /// interpolation `{expr}` is converted to its string form via the same
    /// rules as `to_str` (i64/f64/bool/str/etc.) and concatenated in order.
    fn gen_string_literal_interp(
        &mut self,
        parts: &[StringPart],
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        // Start from an empty vredrs_str.
        let empty_name = self.intern("");
        let cur = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call %vredrs.str* @vredrs_str_from_cstr(ptr {})\n",
            cur, empty_name
        ));
        let mut cur = Val::new(Ty::Str, cur);
        for p in parts {
            match p {
                StringPart::Text(t) => {
                    let esc = interpret_escapes(t);
                    if esc.is_empty() {
                        continue;
                    }
                    let name = self.intern(&esc);
                    let len = esc.as_bytes().len() + 1;
                    let ptr = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = getelementptr inbounds [{} x i8], ptr {}, i64 0, i64 0\n",
                        ptr, len, name
                    ));
                    let part = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call %vredrs.str* @vredrs_str_from_cstr(ptr {})\n",
                        part, ptr
                    ));
                    let next = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call %vredrs.str* @vredrs_str_concat(%vredrs.str* {}, %vredrs.str* {})\n",
                        next, cur.repr, part
                    ));
                    cur = Val::new(Ty::Str, next);
                }
                StringPart::Interpolation(e) => {
                    let v = self.gen_expr(e, vars)?;
                    let s = self.to_str(v, vars)?;
                    let next = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call %vredrs.str* @vredrs_str_concat(%vredrs.str* {}, %vredrs.str* {})\n",
                        next, cur.repr, s.repr
                    ));
                    cur = Val::new(Ty::Str, next);
                }
            }
        }
        Ok(cur)
    }

    fn static_string_text(&self, parts: &[StringPart]) -> Result<String> {
        let mut out = String::new();
        for p in parts {
            match p {
                StringPart::Text(t) => out.push_str(&interpret_escapes(t)),
                StringPart::Interpolation(_) => return Err(CompilerError::codegen_error(
                    "string interpolation requires runtime concatenation; use str() + concat explicitly")),
            }
        }
        Ok(out)
    }

    fn gen_list_literal(
        &mut self,
        elements: &[Expr],
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let list = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call %vredrs.list* @vredrs_list_new()\n",
            list
        ));
        for e in elements {
            let v = self.gen_expr(e, vars)?;
            let wv = self.wrap_value(v)?;
            self.buf.push_str(&format!(
                "  call void @vredrs_list_push(%vredrs.list* {}, %vredrs.value {})\n",
                list, wv.repr
            ));
        }
        Ok(Val::new(Ty::List, list))
    }

    fn gen_tuple_literal(
        &mut self,
        elements: &[Expr],
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let n = elements.len() as i64;
        let tup = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call %vredrs.tuple* @vredrs_tuple_new(i64 {})\n",
            tup, n
        ));
        for (i, e) in elements.iter().enumerate() {
            let v = self.gen_expr(e, vars)?;
            let wv = self.wrap_value(v)?;
            self.buf.push_str(&format!(
                "  call void @vredrs_tuple_set_init(%vredrs.tuple* {}, i64 {}, %vredrs.value {})\n",
                tup, i, wv.repr
            ));
        }
        Ok(Val::new(Ty::Tuple, tup))
    }

    fn gen_dict_literal(
        &mut self,
        entries: &[(Expr, Expr)],
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let d = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call %vredrs.dict* @vredrs_dict_new()\n",
            d
        ));
        for (k, v) in entries {
            let kv = self.gen_expr(k, vars)?;
            let key = self.to_str(kv, vars)?;
            let vv = self.gen_expr(v, vars)?;
            let wv = self.wrap_value(vv)?;
            self.buf.push_str(&format!("  call void @vredrs_dict_set(%vredrs.dict* {}, %vredrs.str* {}, %vredrs.value {})\n", d, key.repr, wv.repr));
        }
        Ok(Val::new(Ty::Dict, d))
    }


}
