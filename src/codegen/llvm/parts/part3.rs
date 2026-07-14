impl FullLlvmGen {
    fn gen_async_function(&mut self, f: &FnDef) -> Result<()> {
        let ctor = self.safe(&f.name.name);
        let poll = format!("{}_poll", ctor);
        self.buf
            .push_str(&format!("; async fn {} constructor\n", f.name.name));
        self.buf
            .push_str(&format!("define %vredrs.coro* @{}() {{\nentry:\n", ctor));
        self.buf
            .push_str("  %coro = call %vredrs.coro* @vredrs_coro_alloc()\n");
        self.buf.push_str("  ret %vredrs.coro* %coro\n}\n\n");
        self.buf
            .push_str(&format!("; async fn {} poll\n", f.name.name));
        self.buf.push_str(&format!(
            "define i1 @{}(%vredrs.coro* %coro) {{\nentry:\n",
            poll
        ));
        self.buf.push_str(
            "  %state.slot = getelementptr inbounds %vredrs.coro, ptr %coro, i32 0, i32 0\n",
        );
        self.buf
            .push_str("  %state = load i64, i64* %state.slot, align 8\n");
        self.buf.push_str("  %done = icmp eq i64 %state, 2\n");
        let body_lbl = self.new_lbl();
        let end_lbl = self.new_lbl();
        self.buf.push_str(&format!(
            "  br i1 %done, label %L{}, label %L{}\n",
            end_lbl, body_lbl
        ));
        self.buf.push_str(&format!("L{}:\n", body_lbl));
        let mut vars: HashMap<String, LocalSlot> = HashMap::new();
        vars.insert(
            "__coro".to_string(),
            LocalSlot {
                ty: Ty::Coro,
                ptr: "%coro.addr.synthetic".to_string(),
                class: None,
                async_fn: Some(f.name.name.clone()),
                    closure_sig: None,
            },
        );
        let terminated = self.gen_async_block(&f.body, &mut vars)?;
        if !terminated {
            self.buf
                .push_str("  call void @vredrs_coro_store_result(%vredrs.coro* %coro, i64 0)\n");
            self.buf
                .push_str("  call void @vredrs_coro_set_state(%vredrs.coro* %coro, i64 2)\n");
            self.buf.push_str(&format!("  br label %L{}\n", end_lbl));
        }
        self.buf.push_str(&format!("L{}:\n", end_lbl));
        self.buf.push_str("  ret i1 true\n}\n\n");
        Ok(())
    }

    fn gen_async_block(
        &mut self,
        body: &[Stmt],
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<bool> {
        for s in body {
            match s {
                Stmt::Return(r) => {
                    let v = if r.values.is_empty() {
                        Val::new(Ty::Value, "zeroinitializer")
                    } else {
                        let raw = self.gen_expr(&r.values[0], vars)?;
                        self.cast(raw, &Ty::Value)?
                    };
                    self.buf.push_str(&format!(
                        "  call void @vredrs_coro_store_result(%vredrs.coro* %coro, i64 {})\n",
                        v.repr
                    ));
                    self.buf.push_str("  call void @vredrs_coro_set_state(%vredrs.coro* %coro, i64 2)\n  ret i1 true\n");
                    return Ok(true);
                }
                _ => {
                    let t = self.gen_stmt(s, vars, &Ty::I64)?;
                    if t {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }

    fn gen_main(&mut self, program: &Program) -> Result<()> {
        // Check if user already defined a function named "main".
        let user_has_main = program.declarations.iter().any(|d| {
            if let TopLevel::FnDef(f) = d { f.name.name == "main" } else { false }
        });
        if user_has_main {
            // User-defined main exists — don't generate an auto-main.
            // Just generate top-level statements (if any) into a separate
            // init function that main can call, or skip them.
            return Ok(());
        }
        self.buf.push_str("define i32 @main() {\nentry:\n");
        let mut vars: HashMap<String, LocalSlot> = HashMap::new();
        let ret_ty = Ty::Value;
        let mut terminated = false;
        for d in &program.declarations {
            match d {
                TopLevel::Statement(s) if !terminated => {
                    terminated = self.gen_stmt(s, &mut vars, &ret_ty)?;
                }
                TopLevel::Statement(_) => {}
                _ => {}
            }
        }
        if !terminated {
            self.buf.push_str("  ret i32 0\n");
        }
        self.buf.push_str("}\n");
        Ok(())
    }

    fn gen_block(
        &mut self,
        body: &[Stmt],
        vars: &mut HashMap<String, LocalSlot>,
        ret_ty: &Ty,
    ) -> Result<bool> {
        for s in body {
            let t = self.gen_stmt(s, vars, ret_ty)?;
            if t {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn gen_stmt(
        &mut self,
        s: &Stmt,
        vars: &mut HashMap<String, LocalSlot>,
        ret_ty: &Ty,
    ) -> Result<bool> {
        match s {
            Stmt::Assign(a) => {
                self.gen_assign(a, vars)?;
                Ok(false)
            }
            Stmt::Println(p) => {
                for a in &p.args {
                    self.emit_print_arg(a, vars)?;
                }
                self.buf.push_str("  call i64 @vredrs_println()\n");
                Ok(false)
            }
            Stmt::Paste(p) => {
                for a in &p.args {
                    self.emit_print_arg(a, vars)?;
                }
                Ok(false)
            }
            Stmt::Return(r) => {
                if r.values.is_empty() {
                    self.emit_default_ret(ret_ty)?;
                } else {
                    let v = self.gen_expr(&r.values[0], vars)?;
                    let v = self.cast(v, ret_ty)?;
                    if *ret_ty == Ty::Void {
                        self.buf.push_str("  ret void\n");
                    } else {
                        self.emit_rc_inc(&v);
                        self.buf
                            .push_str(&format!("  ret {} {}\n", ret_ty.ir(), v.repr));
                    }
                }
                Ok(true)
            }
            Stmt::Throw(t) => {
                self.gen_throw(t, vars)?;
                Ok(true)
            }
            Stmt::Expr(e) => {
                let _ = self.gen_expr(&e.expr, vars)?;
                Ok(false)
            }
            Stmt::If(i) => {
                self.gen_if(i, vars, ret_ty)?;
                Ok(false)
            }
            Stmt::While(w) => {
                self.gen_while(w, vars, ret_ty)?;
                Ok(false)
            }
            Stmt::ForRange(f) => {
                self.gen_for_range(f, vars, ret_ty)?;
                Ok(false)
            }
            Stmt::ForIn(f) => {
                self.gen_for_in(f, vars, ret_ty)?;
                Ok(false)
            }
            Stmt::Loop(l) => {
                self.gen_loop(l, vars, ret_ty)?;
                Ok(false)
            }
            Stmt::Break(_) => {
                let ctx = self
                    .loop_stack
                    .last()
                    .cloned()
                    .ok_or_else(|| CompilerError::codegen_error("break outside loop"))?;
                self.buf
                    .push_str(&format!("  br label %L{}\n", ctx.brk_lbl));
                Ok(true)
            }
            Stmt::Continue(_) => {
                let ctx = self
                    .loop_stack
                    .last()
                    .cloned()
                    .ok_or_else(|| CompilerError::codegen_error("continue outside loop"))?;
                self.buf
                    .push_str(&format!("  br label %L{}\n", ctx.cont_lbl));
                Ok(true)
            }
            Stmt::Match(m) => {
                self.gen_match(m, vars, ret_ty)?;
                Ok(false)
            }
            Stmt::Try(t) => {
                self.gen_try(t, vars, ret_ty)?;
                Ok(false)
            }
            Stmt::With(w) => {
                self.gen_with(w, vars, ret_ty)?;
                Ok(false)
            }
            Stmt::Assert(a) => {
                self.gen_assert(a, vars)?;
                Ok(false)
            }
            Stmt::Panic(p) => {
                let v = self.gen_expr(&p.message, vars)?;
                let s = self.to_str(v, vars)?;
                self.buf.push_str(&format!(
                    "  call void @vredrs_throw_str(%vredrs.str* {})\n  unreachable\n",
                    s.repr
                ));
                Ok(true)
            }
            Stmt::Yield(y) => {
                let is_gen = vars
                    .get("__coro")
                    .and_then(|s| s.async_fn.as_ref())
                    .is_some();
                self.gen_yield(y, vars)?;
                // For generators (eager mode), yield does NOT terminate the block —
                // execution continues to collect more values. For async fns, yield
                // suspends (terminates the poll).
                if is_gen {
                    Ok(false)
                } else {
                    Ok(true)
                }
            }
            Stmt::Spawn(sp) => {
                self.gen_spawn(sp, vars)?;
                Ok(false)
            }
            Stmt::Defer(d) => {
                let _ = self.gen_stmt(&d.stmt, vars, ret_ty)?;
                Ok(false)
            }
            Stmt::Flush(_) => {
                self.buf.push_str("  call i32 @fflush(i8* null)\n");
                Ok(false)
            }
            Stmt::Input(inp) => {
                let prompt_val = if let Some(p) = &inp.prompt {
                    let pstr = self.intern(p);
                    let len = p.len() + 1;
                    let ptr = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = getelementptr inbounds [{} x i8], ptr {}, i64 0, i64 0\n",
                        ptr, len, pstr
                    ));
                    let s = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call %vredrs.str* @vredrs_str_from_cstr(i8* {})\n",
                        s, ptr
                    ));
                    Some(Val::new(Ty::Str, s))
                } else {
                    None
                };
                let res = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_input(%vredrs.str* {})\n",
                    res,
                    prompt_val
                        .as_ref()
                        .map(|v| v.repr.clone())
                        .unwrap_or_else(|| "null".to_string())
                ));
                let v = Val::new(Ty::Str, res);
                self.store_assignee(&inp.target, v, vars)?;
                Ok(false)
            }
            other => Err(CompilerError::codegen_error(format!(
                "stmt {:?} not lowered",
                self.stmt_kind(other)
            ))),
        }
    }

    fn gen_assign(&mut self, a: &AssignStmt, vars: &mut HashMap<String, LocalSlot>) -> Result<()> {
        if a.targets.is_empty() {
            return Err(CompilerError::codegen_error("assignment with no target"));
        }
        for target in &a.targets {
            if a.operator == AssignOp::Delete {
                self.gen_delete(target, vars)?;
                continue;
            }
            let v = if a.operator == AssignOp::Simple {
                self.gen_expr(&a.value, vars)?
            } else {
                let cur = self.load_assignee(target, vars)?;
                let rhs = self.gen_expr(&a.value, vars)?;
                let op = match a.operator {
                    AssignOp::Plus => BinaryOp::Add,
                    AssignOp::Minus => BinaryOp::Sub,
                    AssignOp::Star => BinaryOp::Mul,
                    AssignOp::Slash => BinaryOp::Div,
                    AssignOp::Percent => BinaryOp::Mod,
                    _ => return Err(CompilerError::codegen_error("unsupported compound assign")),
                };
                self.gen_binary(cur, rhs, &op)?
            };
            self.store_assignee(target, v, vars)?;
        }
        Ok(())
    }

    fn gen_delete(
        &mut self,
        target: &Assignee,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<()> {
        match target {
            Assignee::Index(i) => {
                let t = self.gen_expr(&i.target, vars)?;
                let idx = self.gen_expr(&i.index, vars)?;
                match t.ty {
                    Ty::Dict => {
                        let key = self.to_str(idx, vars)?;
                        self.buf.push_str(&format!(
                            "  call void @vredrs_dict_del(%vredrs.dict* {}, %vredrs.str* {})\n",
                            t.repr, key.repr
                        ));
                    }
                    Ty::Obj => {
                        let class = t.class.clone().ok_or_else(|| {
                            CompilerError::codegen_error("del obj[i] needs class")
                        })?;
                        let _ = self.emit_vtable_call(t, &class, "__delitem__", vec![idx])?;
                    }
                    _ => {
                        return Err(CompilerError::codegen_error(
                            "del index needs dict or object",
                        ))
                    }
                }
            }
            Assignee::Member(m) => {
                let t = self.gen_expr(&m.target, vars)?;
                if t.ty != Ty::Obj {
                    return Err(CompilerError::codegen_error("del member needs object"));
                }
                let key = self.intern_str_for(&m.member.name)?;
                self.buf.push_str(&format!(
                    "  call void @vredrs_object_del_field(%vredrs.object* {}, %vredrs.str* {})\n",
                    t.repr, key
                ));
            }
            _ => {
                return Err(CompilerError::codegen_error(
                    "del only supports index/member targets",
                ))
            }
        }
        Ok(())
    }

    fn load_assignee(
        &mut self,
        target: &Assignee,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        match target {
            Assignee::Identifier(id) => self.load_local(&id.name, vars),
            Assignee::Member(m) => self.gen_member(m, vars),
            Assignee::Index(i) => self.gen_index(i, vars),
            _ => Err(CompilerError::codegen_error(
                "compound assign target not loadable",
            )),
        }
    }

    fn store_assignee(
        &mut self,
        target: &Assignee,
        v: Val,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<()> {
        match target {
            Assignee::Identifier(id) => self.store_local(&id.name, v, vars),
            Assignee::Member(m) => self.store_member(m, v, vars),
            Assignee::Index(i) => self.store_index(i, v, vars),
            Assignee::Qualified(_) => Err(CompilerError::codegen_error(
                "module-qualified assignment not supported in native backend",
            )),
            Assignee::Tuple(items) => {
                let tup = self.to_tuple(v, vars)?;
                for (i, item) in items.iter().enumerate() {
                    let idx_var = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_tuple_get(%vredrs.tuple* {}, i64 {})\n",
                        idx_var, tup.repr, i
                    ));
                    let elem = Val::new(Ty::I64, idx_var);
                    self.store_assignee(item, elem, vars)?;
                }
                Ok(())
            }
        }
    }

    fn store_local(
        &mut self,
        name: &str,
        v: Val,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<()> {
        // If this is a closure value, capture its signature so call sites
        // can form the right indirect call.
        let closure_sig = if v.ty == Ty::Fn {
            self.closure_sigs.get(&v.repr).cloned()
        } else {
            None
        };
        if !vars.contains_key(name) {
            let ptr = self.new_local(name);
            self.buf.push_str(&format!("  ; local {}\n", name));
            self.buf
                .push_str(&format!("  {} = alloca {}, align 8\n", ptr, v.ty.ir()));
            vars.insert(
                name.to_string(),
                LocalSlot {
                    ty: v.ty.clone(),
                    ptr,
                    class: v.class.clone(),
                    async_fn: v.async_fn.clone(),
                    closure_sig: closure_sig.clone(),
                },
            );
        }
        let slot = vars.get(name).cloned().ok_or_else(|| {
            CompilerError::codegen_error(format!("missing local slot for '{}'", name))
        })?;
        // Save class info from the original value before casting, because
        // cast(Obj -> Value) via wrap_value loses the class metadata.
        let orig_class = v.class.clone();
        let stored = self.cast(v, &slot.ty)?;
        if stored.ty == Ty::Obj {
            self.emit_rc_inc(&stored);
        }
        if slot.ty == Ty::Obj {
            self.emit_rc_dec_slot(&slot);
        }
        self.buf.push_str(&format!(
            "  store {} {}, {}* {}, align 8\n",
            slot.ty.ir(),
            stored.repr,
            slot.ty.ir(),
            slot.ptr
        ));
        if let Some(s) = vars.get_mut(name) {
            // Prefer the original class, then the stored class, then the slot's.
            s.class = orig_class.clone().or(stored.class.clone()).or(s.class.clone());
            s.async_fn = stored.async_fn.clone().or(s.async_fn.clone());
            if closure_sig.is_some() {
                s.closure_sig = closure_sig;
            }
        }
        Ok(())
    }

    fn store_member(
        &mut self,
        m: &MemberAccessExpr,
        v: Val,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<()> {
        let recv = self.gen_expr(&m.target, vars)?;
        if recv.ty != Ty::Obj {
            return Err(CompilerError::codegen_error(
                "member assign needs object receiver",
            ));
        }
        let class = recv
            .class
            .clone()
            .ok_or_else(|| CompilerError::codegen_error("member assign needs known class"))?;
        if self.class_has_method(&class, "__setattr__") {
            let key = self.intern_str_for(&m.member.name)?;
            let key_val = Val::new(Ty::Str, key);
            let _ = self.emit_vtable_call(recv, &class, "__setattr__", vec![key_val, v])?;
            return Ok(());
        }
        let key = self.intern_str_for(&m.member.name)?;
        let wv = self.wrap_value(v)?;
        self.buf.push_str(&format!("  call void @vredrs_object_set_field(%vredrs.object* {}, %vredrs.str* {}, %vredrs.value {})\n", recv.repr, key, wv.repr));
        Ok(())
    }

    fn store_index(
        &mut self,
        i: &IndexExpr,
        v: Val,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<()> {
        let t = self.gen_expr(&i.target, vars)?;
        let idx = self.gen_expr(&i.index, vars)?;
        match t.ty {
            Ty::List => {
                let i64_idx = self.cast(idx, &Ty::I64)?;
                let wv = self.wrap_value(v)?;
                self.buf.push_str(&format!(
                    "  call void @vredrs_list_set(%vredrs.list* {}, i64 {}, %vredrs.value {})\n",
                    t.repr, i64_idx.repr, wv.repr
                ));
            }
            Ty::Dict => {
                let key = self.to_str(idx, vars)?;
                let wv = self.wrap_value(v)?;
                self.buf.push_str(&format!("  call void @vredrs_dict_set(%vredrs.dict* {}, %vredrs.str* {}, %vredrs.value {})\n", t.repr, key.repr, wv.repr));
            }
            Ty::Obj => {
                let class = t
                    .class
                    .clone()
                    .ok_or_else(|| CompilerError::codegen_error("obj[i] = needs class"))?;
                let _ = self.emit_vtable_call(t, &class, "__setitem__", vec![idx, v])?;
            }
            _ => {
                return Err(CompilerError::codegen_error(
                    "index assign needs list/dict/object",
                ))
            }
        }
        Ok(())
    }

    fn gen_if(
        &mut self,
        is_: &IfStmt,
        vars: &mut HashMap<String, LocalSlot>,
        ret_ty: &Ty,
    ) -> Result<()> {
        let end_lbl = self.new_lbl();
        let mut next_lbl = self.new_lbl();
        let cond_raw = self.gen_expr(&is_.condition, vars)?;
        let cond = self.to_bool(cond_raw)?;
        self.buf.push_str(&format!("  ; if\n"));
        let then_lbl = self.new_lbl();
        self.buf.push_str(&format!(
            "  br i1 {}, label %L{}, label %L{}\n",
            cond.repr, then_lbl, next_lbl
        ));
        self.buf.push_str(&format!("L{}:\n", then_lbl));
        let then_term = self.gen_block(&is_.then_body, vars, ret_ty)?;
        if !then_term {
            self.buf.push_str(&format!("  br label %L{}\n", end_lbl));
        }
        for (elif_cond, elif_body) in &is_.elif_chain {
            self.buf.push_str(&format!("L{}:\n", next_lbl));
            let elif_then = self.new_lbl();
            let following = self.new_lbl();
            let c_raw = self.gen_expr(elif_cond, vars)?;
            let c = self.to_bool(c_raw)?;
            self.buf.push_str(&format!(
                "  br i1 {}, label %L{}, label %L{}\n",
                c.repr, elif_then, following
            ));
            self.buf.push_str(&format!("L{}:\n", elif_then));
            let term = self.gen_block(elif_body, vars, ret_ty)?;
            if !term {
                self.buf.push_str(&format!("  br label %L{}\n", end_lbl));
            }
            next_lbl = following;
        }
        self.buf.push_str(&format!("L{}:\n", next_lbl));
        if let Some(body) = &is_.else_body {
            let term = self.gen_block(body, vars, ret_ty)?;
            if !term {
                self.buf.push_str(&format!("  br label %L{}\n", end_lbl));
            }
        } else {
            self.buf.push_str(&format!("  br label %L{}\n", end_lbl));
        }
        self.buf.push_str(&format!("L{}:\n", end_lbl));
        Ok(())
    }


}
