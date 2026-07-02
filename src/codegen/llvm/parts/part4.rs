impl FullLlvmGen {
    fn gen_while(
        &mut self,
        ws: &WhileStmt,
        vars: &mut HashMap<String, LocalSlot>,
        ret_ty: &Ty,
    ) -> Result<()> {
        let cond_lbl = self.new_lbl();
        let body_lbl = self.new_lbl();
        let end_lbl = self.new_lbl();
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", cond_lbl, cond_lbl));
        let c_raw = self.gen_expr(&ws.condition, vars)?;
        let cond = self.to_bool(c_raw)?;
        self.buf.push_str(&format!(
            "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
            cond.repr, body_lbl, end_lbl, body_lbl
        ));
        self.loop_stack.push(LoopCtx {
            cont_lbl: cond_lbl,
            brk_lbl: end_lbl,
        });
        let term = self.gen_block(&ws.body, vars, ret_ty)?;
        self.loop_stack.pop();
        if !term {
            self.buf.push_str(&format!("  br label %L{}\n", cond_lbl));
        }
        self.buf.push_str(&format!("L{}:\n", end_lbl));
        if let Some(eb) = &ws.else_body {
            let _ = self.gen_block(eb, vars, ret_ty)?;
        }
        Ok(())
    }

    fn gen_for_range(
        &mut self,
        fs: &ForRangeStmt,
        vars: &mut HashMap<String, LocalSlot>,
        ret_ty: &Ty,
    ) -> Result<()> {
        let start_raw = self.gen_expr(&fs.from, vars)?;
        let start = self.cast(start_raw, &Ty::I64)?;
        let end_raw = self.gen_expr(&fs.to, vars)?;
        let end = self.cast(end_raw, &Ty::I64)?;
        self.store_local(&fs.var.name, start, vars)?;
        let cond_lbl = self.new_lbl();
        let body_lbl = self.new_lbl();
        let step_lbl = self.new_lbl();
        let end_lbl = self.new_lbl();
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", cond_lbl, cond_lbl));
        let cur = self.load_local(&fs.var.name, vars)?;
        let cmp = self.new_var();
        self.buf.push_str(&format!(
            "  {} = icmp slt i64 {}, {}\n",
            cmp, cur.repr, end.repr
        ));
        self.buf.push_str(&format!(
            "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
            cmp, body_lbl, end_lbl, body_lbl
        ));
        self.loop_stack.push(LoopCtx {
            cont_lbl: step_lbl,
            brk_lbl: end_lbl,
        });
        let term = self.gen_block(&fs.body, vars, ret_ty)?;
        self.loop_stack.pop();
        if !term {
            self.buf.push_str(&format!("  br label %L{}\n", step_lbl));
        }
        self.buf.push_str(&format!("L{}:\n", step_lbl));
        let cur2 = self.load_local(&fs.var.name, vars)?;
        let next = self.new_var();
        self.buf
            .push_str(&format!("  {} = add i64 {}, 1\n", next, cur2.repr));
        self.store_local(&fs.var.name, Val::new(Ty::I64, next), vars)?;
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", cond_lbl, end_lbl));
        if let Some(eb) = &fs.else_body {
            let _ = self.gen_block(eb, vars, ret_ty)?;
        }
        Ok(())
    }

    fn gen_for_in(
        &mut self,
        fs: &ForInStmt,
        vars: &mut HashMap<String, LocalSlot>,
        ret_ty: &Ty,
    ) -> Result<()> {
        // for x in range(a, b) -> for-range
        // for x in list/tuple/dict/str -> iterate by index
        let it = self.gen_expr(&fs.iterable, vars)?;
        match &it.ty {
            Ty::List | Ty::Tuple => {
                let len_fn = if it.ty == Ty::List {
                    "@vredrs_list_len"
                } else {
                    "@vredrs_tuple_len"
                };
                let len_var = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 {}({} {})\n",
                    len_var,
                    len_fn,
                    it.ty.ir(),
                    it.repr
                ));
                let idx_ptr = self.new_local("__for_idx");
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
                let get_fn = if it.ty == Ty::List {
                    "@vredrs_list_get"
                } else {
                    "@vredrs_tuple_get"
                };
                let item = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.value {}({} {}, i64 {})\n",
                    item,
                    get_fn,
                    it.ty.ir(),
                    it.repr,
                    cur_idx
                ));
                self.store_local(&fs.var.name, Val::new(Ty::Value, item), vars)?;
                self.loop_stack.push(LoopCtx {
                    cont_lbl: step_lbl,
                    brk_lbl: end_lbl,
                });
                let term = self.gen_block(&fs.body, vars, ret_ty)?;
                self.loop_stack.pop();
                if !term {
                    self.buf.push_str(&format!("  br label %L{}\n", step_lbl));
                }
                self.buf.push_str(&format!("L{}:\n", step_lbl));
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
                if let Some(eb) = &fs.else_body {
                    let _ = self.gen_block(eb, vars, ret_ty)?;
                }
                Ok(())
            }
            Ty::Dict => {
                let keys = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_dict_keys(%vredrs.dict* {})\n",
                    keys, it.repr
                ));
                let len_var = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_list_len(%vredrs.list* {})\n",
                    len_var, keys
                ));
                let idx_ptr = self.new_local("__for_idx");
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
                let key_ptr = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.value @vredrs_list_get(%vredrs.list* {}, i64 {})\n",
                    key_ptr, keys, cur_idx
                ));
                self.store_local(&fs.var.name, Val::new(Ty::Value, key_ptr), vars)?;
                self.loop_stack.push(LoopCtx {
                    cont_lbl: step_lbl,
                    brk_lbl: end_lbl,
                });
                let term = self.gen_block(&fs.body, vars, ret_ty)?;
                self.loop_stack.pop();
                if !term {
                    self.buf.push_str(&format!("  br label %L{}\n", step_lbl));
                }
                self.buf.push_str(&format!("L{}:\n", step_lbl));
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
                Ok(())
            }
            Ty::Str => {
                let len_var = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_str_len(%vredrs.str* {})\n",
                    len_var, it.repr
                ));
                let idx_ptr = self.new_local("__for_idx");
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
                // Get byte at index — for simplicity, store as i64.
                let data_ptr = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i8* @vredrs_str_data(%vredrs.str* {})\n",
                    data_ptr, it.repr
                ));
                let byte_ptr = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = getelementptr inbounds i8, ptr {}, i64 {}\n",
                    byte_ptr, data_ptr, cur_idx
                ));
                let byte = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = load i8, i8* {}, align 1\n",
                    byte, byte_ptr
                ));
                let byte_i64 = self.new_var();
                self.buf
                    .push_str(&format!("  {} = sext i8 {} to i64\n", byte_i64, byte));
                self.store_local(&fs.var.name, Val::new(Ty::I64, byte_i64), vars)?;
                self.loop_stack.push(LoopCtx {
                    cont_lbl: step_lbl,
                    brk_lbl: end_lbl,
                });
                let term = self.gen_block(&fs.body, vars, ret_ty)?;
                self.loop_stack.pop();
                if !term {
                    self.buf.push_str(&format!("  br label %L{}\n", step_lbl));
                }
                self.buf.push_str(&format!("L{}:\n", step_lbl));
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
                Ok(())
            }
            _ => Err(CompilerError::codegen_error(format!(
                "for-in over type {} not supported",
                it.ty.ir()
            ))),
        }
    }

    fn gen_loop(
        &mut self,
        ls: &LoopStmt,
        vars: &mut HashMap<String, LocalSlot>,
        ret_ty: &Ty,
    ) -> Result<()> {
        let body_lbl = self.new_lbl();
        let end_lbl = self.new_lbl();
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", body_lbl, body_lbl));
        self.loop_stack.push(LoopCtx {
            cont_lbl: body_lbl,
            brk_lbl: end_lbl,
        });
        let term = self.gen_block(&ls.body, vars, ret_ty)?;
        self.loop_stack.pop();
        if !term {
            self.buf.push_str(&format!("  br label %L{}\n", body_lbl));
        }
        self.buf.push_str(&format!("L{}:\n", end_lbl));
        if let Some(eb) = &ls.else_body {
            let _ = self.gen_block(eb, vars, ret_ty)?;
        }
        Ok(())
    }

    fn gen_match(
        &mut self,
        ms: &MatchStmt,
        vars: &mut HashMap<String, LocalSlot>,
        ret_ty: &Ty,
    ) -> Result<()> {
        let subj = self.gen_expr(&ms.expr, vars)?;
        let subj = self.cast(subj, &Ty::I64)?;
        let end_lbl = self.new_lbl();
        let mut next_lbl = self.new_lbl();
        for case in &ms.cases {
            self.buf.push_str(&format!("L{}:\n", next_lbl));
            let case_body = self.new_lbl();
            let following = self.new_lbl();
            let cond = self.pattern_cond(&subj, &case.pattern, vars)?;
            let cond = if let Some(guard) = &case.guard {
                let g_raw = self.gen_expr(guard, vars)?;
                let g = self.to_bool(g_raw)?;
                let both = self.new_var();
                self.buf
                    .push_str(&format!("  {} = and i1 {}, {}\n", both, cond.repr, g.repr));
                Val::new(Ty::Bool, both)
            } else {
                cond
            };
            self.buf.push_str(&format!(
                "  br i1 {}, label %L{}, label %L{}\n",
                cond.repr, case_body, following
            ));
            self.buf.push_str(&format!("L{}:\n", case_body));
            let term = self.gen_block(&case.body, vars, ret_ty)?;
            if !term {
                self.buf.push_str(&format!("  br label %L{}\n", end_lbl));
            }
            next_lbl = following;
        }
        self.buf.push_str(&format!("L{}:\n", next_lbl));
        if let Some(body) = &ms.else_case {
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

    fn pattern_cond(
        &mut self,
        subj: &Val,
        pat: &Pattern,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        match pat {
            Pattern::Literal(lit) => {
                let v_raw = self.gen_expr(&lit.literal, vars)?;
                let v = self.cast(v_raw, &Ty::I64)?;
                let tmp = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = icmp eq i64 {}, {}\n",
                    tmp, subj.repr, v.repr
                ));
                Ok(Val::new(Ty::Bool, tmp))
            }
            Pattern::Binding(b) => {
                self.store_local(&b.name.name, subj.clone(), vars)?;
                Ok(Val::new(Ty::Bool, "1"))
            }
            Pattern::Wildcard(_) => Ok(Val::new(Ty::Bool, "1")),
            _ => Err(CompilerError::codegen_error(
                "match pattern kind not supported",
            )),
        }
    }

    fn gen_try(
        &mut self,
        ts: &TryStmt,
        vars: &mut HashMap<String, LocalSlot>,
        ret_ty: &Ty,
    ) -> Result<()> {
        let try_lbl = self.new_lbl();
        let catch_lbl = self.new_lbl();
        let finally_lbl = self.new_lbl();
        let end_lbl = self.new_lbl();
        self.buf
            .push_str("  ; try/catch (heap-allocated jmp_buf, returns_twice setjmp)\n");
        // Pre-allocate the catch variable slot in the entry block so it's
        // always valid (LLVM requires allocas to dominate all uses, and the
        // catch block is only reached via longjmp).
        if let Some(cv) = &ts.catch_var {
            if !vars.contains_key(&cv.name) {
                let ptr = self.new_local(&cv.name);
                self.buf.push_str(&format!("  ; catch var pre-alloca\n"));
                self.buf
                    .push_str(&format!("  {} = alloca %vredrs.str*, align 8\n", ptr));
                self.buf.push_str(&format!(
                    "  store %vredrs.str* null, %vredrs.str** {}, align 8\n",
                    ptr
                ));
                vars.insert(
                    cv.name.clone(),
                    LocalSlot {
                        ty: Ty::Str,
                        ptr,
                        class: None,
                        async_fn: None,
                    closure_sig: None,
                    },
                );
            }
        }
        // Allocate jmp_buf on the heap (survives -O2).
        let buf = self.new_var();
        self.buf
            .push_str(&format!("  {} = call ptr @vredrs_try_begin()\n", buf));
        // Save current jmp_top so we can restore it on the way out.
        let old = self.new_var();
        self.buf
            .push_str(&format!("  {} = call ptr @vredrs_get_jmp_top()\n", old));
        // Install this buffer as the current jmp target.
        self.buf
            .push_str(&format!("  call void @vredrs_set_jmp_top(ptr {})\n", buf));
        // Call setjmp directly in IR so the saved frame is THIS function.
        let code = self.new_var();
        self.buf
            .push_str(&format!("  {} = call i32 @setjmp(ptr {}) #0\n", code, buf));
        let is_zero = self.new_var();
        self.buf
            .push_str(&format!("  {} = icmp eq i32 {}, 0\n", is_zero, code));
        self.buf.push_str(&format!(
            "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
            is_zero, try_lbl, catch_lbl, try_lbl
        ));
        let try_term = self.gen_block(&ts.try_body, vars, ret_ty)?;
        if !try_term {
            self.buf
                .push_str(&format!("  br label %L{}\n", finally_lbl));
        }
        // catch block: reached via longjmp
        self.buf.push_str(&format!("L{}:\n", catch_lbl));
        if let Some(cv) = &ts.catch_var {
            // Get the exception. If a str was thrown, use it directly;
            // otherwise convert the i64 payload to a str for display.
            let ex_str = self.new_var();
            self.buf.push_str(&format!(
                "  {} = call ptr @vredrs_get_exception_str()\n",
                ex_str
            ));
            let is_null = self.new_var();
            self.buf
                .push_str(&format!("  {} = icmp eq ptr {}, null\n", is_null, ex_str));
            let str_lbl = self.new_lbl();
            let i64_lbl = self.new_lbl();
            let merge_lbl = self.new_lbl();
            self.buf.push_str(&format!(
                "  br i1 {}, label %L{}, label %L{}\n",
                is_null, i64_lbl, str_lbl
            ));
            self.buf.push_str(&format!("L{}:\n", str_lbl));
            // String exception — store into the pre-allocated slot.
            if let Some(slot) = vars.get(&cv.name) {
                self.buf.push_str(&format!(
                    "  store %vredrs.str* {}, %vredrs.str** {}, align 8\n",
                    ex_str, slot.ptr
                ));
            }
            self.buf.push_str(&format!("  br label %L{}\n", merge_lbl));
            self.buf.push_str(&format!("L{}:\n", i64_lbl));
            // i64 exception — convert to str and store.
            let ex_i64 = self.new_var();
            self.buf.push_str(&format!(
                "  {} = call i64 @vredrs_get_exception_i64()\n",
                ex_i64
            ));
            let ex_i64_str = self.new_var();
            self.buf.push_str(&format!(
                "  {} = call %vredrs.str* @vredrs_str_of_i64(i64 {})\n",
                ex_i64_str, ex_i64
            ));
            if let Some(slot) = vars.get(&cv.name) {
                self.buf.push_str(&format!(
                    "  store %vredrs.str* {}, %vredrs.str** {}, align 8\n",
                    ex_i64_str, slot.ptr
                ));
            }
            self.buf.push_str(&format!("  br label %L{}\n", merge_lbl));
            self.buf.push_str(&format!("L{}:\n", merge_lbl));
        }
        if let Some(body) = &ts.catch_body {
            let term = self.gen_block(body, vars, ret_ty)?;
            if term {
                self.buf
                    .push_str(&format!("  call void @vredrs_set_jmp_top(ptr {})\n", old));
                self.buf
                    .push_str(&format!("  call void @vredrs_try_end(ptr {})\n", buf));
                return Ok(());
            }
        }
        self.buf.push_str(&format!(
            "  br label %L{}\nL{}:\n",
            finally_lbl, finally_lbl
        ));
        if let Some(body) = &ts.finally_body {
            let term = self.gen_block(body, vars, ret_ty)?;
            if term {
                self.buf
                    .push_str(&format!("  call void @vredrs_set_jmp_top(ptr {})\n", old));
                self.buf
                    .push_str(&format!("  call void @vredrs_try_end(ptr {})\n", buf));
                return Ok(());
            }
        }
        // Restore the previous jmp_top and free our buffer.
        self.buf
            .push_str(&format!("  call void @vredrs_set_jmp_top(ptr {})\n", old));
        self.buf
            .push_str(&format!("  call void @vredrs_try_end(ptr {})\n", buf));
        // If there was no catch body and we reached here via longjmp
        // (exception path), re-throw the exception so it propagates to
        // the outer try/catch. We check if the catch_body was None.
        if ts.catch_body.is_none() && ts.catch_var.is_none() {
            // Re-throw: get the current exception and throw it again.
            self.buf.push_str(
                "  ; re-throw: no catch, propagate exception after finally\n",
            );
            self.buf.push_str(
                "  %rethrow.ex = call ptr @vredrs_get_exception_str()\n",
            );
            self.buf.push_str("  call void @vredrs_throw_str(ptr %rethrow.ex)\n");
            self.buf.push_str("  unreachable\n");
        } else {
            self.buf
                .push_str(&format!("  br label %L{}\nL{}:\n", end_lbl, end_lbl));
        }
        Ok(())
    }


}
