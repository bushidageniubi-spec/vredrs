impl FullLlvmGen {
    /// Emit a lifted lambda as a top-level LLVM function. All params and the
    /// return value use the tagged `%vredrs.value` type.
    fn emit_lambda_function(
        &mut self,
        name: &str,
        params: &[FnParam],
        body: &Expr,
    ) -> Result<()> {
        let mut params_str = String::new();
        for (i, p) in params.iter().enumerate() {
            if i > 0 {
                params_str.push_str(", ");
            }
            params_str.push_str(&format!("%vredrs.value %{}", p.name.name));
        }
        if !params.is_empty() {
            params_str.push_str(", ");
        }
        params_str.push_str("ptr %__env");
        self.buf.push_str(&format!(
            "define %vredrs.value @{}({}) {{\nentry:\n",
            self.safe(name),
            params_str
        ));
        let mut vars: HashMap<String, LocalSlot> = HashMap::new();
        for p in params {
            let ptr = self.new_local(&p.name.name);
            self.buf.push_str(&format!(
                "  {} = alloca %vredrs.value, align 8\n",
                ptr
            ));
            self.buf.push_str(&format!(
                "  store %vredrs.value %{}, %vredrs.value* {}, align 8\n",
                p.name.name, ptr
            ));
            vars.insert(
                p.name.name.clone(),
                LocalSlot {
                    ty: Ty::Value,
                    ptr,
                    class: None,
                    async_fn: None,
                    closure_sig: None,
                },
            );
        }
        let v = self.gen_expr(body, &mut vars)?;
        let wv = self.wrap_value(v)?;
        self.buf
            .push_str(&format!("  ret %vredrs.value {}\n", wv.repr));
        self.buf.push_str("}\n\n");
        Ok(())
    }

    fn gen_member(
        &mut self,
        m: &MemberAccessExpr,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let recv = self.gen_expr(&m.target, vars)?;
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
            return Err(CompilerError::codegen_error("member access needs object"));
        }
        let class = recv
            .class
            .clone()
            .ok_or_else(|| CompilerError::codegen_error("member access needs known class"))?;
        // Try __getattr__ magic method first if defined.
        if self.class_has_method(&class, "__getattr__") {
            let key = self.intern_str_for(&m.member.name)?;
            return self.emit_vtable_call(
                recv,
                &class,
                "__getattr__",
                vec![Val::new(Ty::Str, key)],
            );
        }
        // Otherwise load from dynamic field table. We don't know the field's
        // declared type at member access (it could be a dynamically-added
        // field), so we return i64. The caller can cast with `as` if needed.
        let key = self.intern_str_for(&m.member.name)?;
        let v = self.new_var();
        self.buf.push_str(&format!("  {} = call %vredrs.value @vredrs_object_get_field(%vredrs.object* {}, %vredrs.str* {}, %vredrs.value zeroinitializer)\n", v, recv.repr, key));
        Ok(Val::new(Ty::Value, v))
    }

    fn gen_await(&mut self, a: &AwaitExpr, vars: &mut HashMap<String, LocalSlot>) -> Result<Val> {
        let coro = self.gen_expr(&a.expr, vars)?;
        if coro.ty != Ty::Coro {
            return Err(CompilerError::codegen_error("await needs coroutine"));
        }
        let async_name = coro
            .async_fn
            .clone()
            .ok_or_else(|| CompilerError::codegen_error("await needs coroutine origin"))?;
        let poll_name = format!("{}_poll", self.safe(&async_name));
        let loop_lbl = self.new_lbl();
        let done_lbl = self.new_lbl();
        self.buf
            .push_str(&format!("  br label %L{}\nL{}:\n", loop_lbl, loop_lbl));
        let done = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call i1 @{}(%vredrs.coro* {})\n",
            done, poll_name, coro.repr
        ));
        self.buf.push_str(&format!(
            "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
            done, done_lbl, loop_lbl, done_lbl
        ));
        let result = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call i64 @vredrs_coro_result(%vredrs.coro* {})\n",
            result, coro.repr
        ));
        Ok(Val::new(Ty::I64, result))
    }

    fn gen_spawn_expr(
        &mut self,
        sp: &SpawnExpr,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        let _ = self.gen_expr(&sp.call, vars)?;
        Ok(Val::new(Ty::Void, ""))
    }

    fn gen_coro_expr(
        &mut self,
        c: &CoroExpr,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        // coro fn(args...) — call the async fn constructor.
        let fn_name = match c.function.as_ref() {
            Expr::Identifier(id) => id.name.clone(),
            _ => return Err(CompilerError::codegen_error("coro() needs function name")),
        };
        let sig = self.functions.get(&fn_name).cloned().ok_or_else(|| {
            CompilerError::codegen_error(format!("unknown async fn '{}'", fn_name))
        })?;
        let _ = sig;
        // Call the constructor (no args; async fn state is set up by the
        // first poll). For simplicity we ignore args.
        let t = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call %vredrs.coro* @{}()\n",
            t,
            self.safe(&fn_name)
        ));
        Ok(Val::new(Ty::Coro, t).with_async(fn_name))
    }

    fn gen_resume_expr(
        &mut self,
        r: &ResumeExpr,
        vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        // resume(coro, ...) — poll the coroutine once.
        let coro = self.gen_expr(&r.handle, vars)?;
        if coro.ty != Ty::Coro {
            return Err(CompilerError::codegen_error("resume needs coroutine"));
        }
        let async_name = coro
            .async_fn
            .clone()
            .ok_or_else(|| CompilerError::codegen_error("resume needs coroutine origin"))?;
        let poll_name = format!("{}_poll", self.safe(&async_name));
        let done = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call i1 @{}(%vredrs.coro* {})\n",
            done, poll_name, coro.repr
        ));
        let result = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call i64 @vredrs_coro_result(%vredrs.coro* {})\n",
            result, coro.repr
        ));
        // If the coroutine is done (exhausted), return 0 instead of the
        // stale result from the last yield.
        let safe_result = self.new_var();
        self.buf.push_str(&format!(
            "  {} = select i1 {}, i64 0, i64 {}\n",
            safe_result, done, result
        ));
        Ok(Val::new(Ty::I64, safe_result))
    }

    fn gen_binary(&mut self, l: Val, r: Val, op: &BinaryOp) -> Result<Val> {
        // Object operator dispatch via magic methods.
        if l.ty == Ty::Obj {
            let class = l
                .class
                .clone()
                .ok_or_else(|| CompilerError::codegen_error("obj op needs class"))?;
            let magic = match op {
                BinaryOp::Add => "__add__",
                BinaryOp::Sub => "__sub__",
                BinaryOp::Mul => "__mul__",
                BinaryOp::Div => "__truediv__",
                BinaryOp::Mod => "__mod__",
                BinaryOp::Eq => "__eq__",
                BinaryOp::Ne => "__ne__",
                BinaryOp::Lt => "__lt__",
                BinaryOp::Gt => "__gt__",
                BinaryOp::Le => "__le__",
                BinaryOp::Ge => "__ge__",
                BinaryOp::In => "__contains__",
                _ => {
                    return Err(CompilerError::codegen_error(
                        "obj operator has no magic mapping",
                    ))
                }
            };
            return self.emit_vtable_call(l, &class, magic, vec![r]);
        }
        // Tagged value dispatch via runtime.
        if l.ty == Ty::Value || r.ty == Ty::Value {
            let lw = self.wrap_value(l)?;
            let rw = self.wrap_value(r)?;
            match op {
                BinaryOp::Add => {
                    let t = self.new_var();
                    self.buf.push_str(&format!("  {} = call %vredrs.value @vredrs_value_add(%vredrs.value {}, %vredrs.value {})\n", t, lw.repr, rw.repr));
                    return Ok(Val::new(Ty::Value, t));
                }
                BinaryOp::Sub => {
                    let t = self.new_var();
                    self.buf.push_str(&format!("  {} = call %vredrs.value @vredrs_value_sub(%vredrs.value {}, %vredrs.value {})\n", t, lw.repr, rw.repr));
                    return Ok(Val::new(Ty::Value, t));
                }
                BinaryOp::Mul => {
                    let t = self.new_var();
                    self.buf.push_str(&format!("  {} = call %vredrs.value @vredrs_value_mul(%vredrs.value {}, %vredrs.value {})\n", t, lw.repr, rw.repr));
                    return Ok(Val::new(Ty::Value, t));
                }
                BinaryOp::Div | BinaryOp::FloorDiv => {
                    let t = self.new_var();
                    self.buf.push_str(&format!("  {} = call %vredrs.value @vredrs_value_div(%vredrs.value {}, %vredrs.value {})\n", t, lw.repr, rw.repr));
                    return Ok(Val::new(Ty::Value, t));
                }
                BinaryOp::Mod => {
                    let t = self.new_var();
                    self.buf.push_str(&format!("  {} = call %vredrs.value @vredrs_value_mod(%vredrs.value {}, %vredrs.value {})\n", t, lw.repr, rw.repr));
                    return Ok(Val::new(Ty::Value, t));
                }
                BinaryOp::Eq => {
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_value_eq(%vredrs.value {}, %vredrs.value {})\n",
                        t, lw.repr, rw.repr
                    ));
                    let b = self.new_var();
                    self.buf
                        .push_str(&format!("  {} = icmp ne i64 {}, 0\n", b, t));
                    return Ok(Val::new(Ty::Bool, b));
                }
                BinaryOp::Ne => {
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_value_ne(%vredrs.value {}, %vredrs.value {})\n",
                        t, lw.repr, rw.repr
                    ));
                    let b = self.new_var();
                    self.buf
                        .push_str(&format!("  {} = icmp ne i64 {}, 0\n", b, t));
                    return Ok(Val::new(Ty::Bool, b));
                }
                BinaryOp::Lt => {
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_value_lt(%vredrs.value {}, %vredrs.value {})\n",
                        t, lw.repr, rw.repr
                    ));
                    let b = self.new_var();
                    self.buf
                        .push_str(&format!("  {} = icmp ne i64 {}, 0\n", b, t));
                    return Ok(Val::new(Ty::Bool, b));
                }
                BinaryOp::Gt => {
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_value_gt(%vredrs.value {}, %vredrs.value {})\n",
                        t, lw.repr, rw.repr
                    ));
                    let b = self.new_var();
                    self.buf
                        .push_str(&format!("  {} = icmp ne i64 {}, 0\n", b, t));
                    return Ok(Val::new(Ty::Bool, b));
                }
                BinaryOp::Le => {
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_value_le(%vredrs.value {}, %vredrs.value {})\n",
                        t, lw.repr, rw.repr
                    ));
                    let b = self.new_var();
                    self.buf
                        .push_str(&format!("  {} = icmp ne i64 {}, 0\n", b, t));
                    return Ok(Val::new(Ty::Bool, b));
                }
                BinaryOp::Ge => {
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_value_ge(%vredrs.value {}, %vredrs.value {})\n",
                        t, lw.repr, rw.repr
                    ));
                    let b = self.new_var();
                    self.buf
                        .push_str(&format!("  {} = icmp ne i64 {}, 0\n", b, t));
                    return Ok(Val::new(Ty::Bool, b));
                }
                _ => {
                    return Err(CompilerError::codegen_error(
                        "binary op not supported on tagged values",
                    ))
                }
            }
        }
        // String concatenation / comparison.
        if l.ty == Ty::Str {
            match op {
                BinaryOp::Add => {
                    let t = self.new_var();
                    self.buf.push_str(&format!("  {} = call %vredrs.str* @vredrs_str_concat(%vredrs.str* {}, %vredrs.str* {})\n", t, l.repr, r.repr));
                    return Ok(Val::new(Ty::Str, t));
                }
                BinaryOp::Eq => {
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_str_eq(%vredrs.str* {}, %vredrs.str* {})\n",
                        t, l.repr, r.repr
                    ));
                    let b = self.new_var();
                    self.buf.push_str(&format!("  {} = icmp ne i64 {}, 0\n", b, t));
                    return Ok(Val::new(Ty::Bool, b));
                }
                BinaryOp::Ne => {
                    let t = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_str_eq(%vredrs.str* {}, %vredrs.str* {})\n",
                        t, l.repr, r.repr
                    ));
                    let not_ = self.new_var();
                    self.buf
                        .push_str(&format!("  {} = icmp eq i64 {}, 0\n", not_, t));
                    return Ok(Val::new(Ty::Bool, not_));
                }
                _ => {}
            }
        }
        // List `in` — check if element is in list.
        if l.ty != Ty::Obj && op == &BinaryOp::In && r.ty == Ty::List {
            let raw = self.value_to_i64_slot(l)?;
            let t = self.new_var();
            self.buf.push_str(&format!(
                "  {} = call i64 @vredrs_list_contains(%vredrs.list* {}, i64 {})\n",
                t, r.repr, raw
            ));
            return Ok(Val::new(Ty::Bool, t));
        }
        // Dict `in` — check key membership.
        if op == &BinaryOp::In && (r.ty == Ty::Dict || r.ty == Ty::Set) {
            let key = self.to_str(l, &mut HashMap::new())?;
            let t = self.new_var();
            self.buf.push_str(&format!(
                "  {} = call i64 @vredrs_dict_has(%vredrs.dict* {}, %vredrs.str* {})\n",
                t, r.repr, key.repr
            ));
            return Ok(Val::new(Ty::Bool, t));
        }
        match op {
            BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::FloorDiv
            | BinaryOp::Mod => {
                if !l.ty.is_numeric() || !r.ty.is_numeric() {
                    return Err(CompilerError::codegen_error(
                        "arithmetic needs numeric operands",
                    ));
                }
                let target = if l.ty == Ty::F64 || r.ty == Ty::F64 {
                    Ty::F64
                } else {
                    Ty::I64
                };
                let li = self.cast(l, &target)?;
                let ri = self.cast(r, &target)?;
                let instr = match (&target, op) {
                    (Ty::I64, BinaryOp::Add) => "add",
                    (Ty::I64, BinaryOp::Sub) => "sub",
                    (Ty::I64, BinaryOp::Mul) => "mul",
                    (Ty::I64, BinaryOp::Div) | (Ty::I64, BinaryOp::FloorDiv) => "sdiv",
                    (Ty::I64, BinaryOp::Mod) => "srem",
                    (Ty::F64, BinaryOp::Add) => "fadd",
                    (Ty::F64, BinaryOp::Sub) => "fsub",
                    (Ty::F64, BinaryOp::Mul) => "fmul",
                    (Ty::F64, BinaryOp::Div) => "fdiv",
                    _ => return Err(CompilerError::codegen_error("unsupported arith op")),
                };
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = {} {} {}, {}\n",
                    t,
                    instr,
                    target.ir(),
                    li.repr,
                    ri.repr
                ));
                Ok(Val::new(target, t))
            }
            BinaryOp::Power => {
                if !l.ty.is_numeric() || !r.ty.is_numeric() {
                    return Err(CompilerError::codegen_error("** needs numeric"));
                }
                // Approximate: use repeated multiplication for small integer exponents.
                Err(CompilerError::codegen_error(
                    "** operator not yet implemented; use a helper fn",
                ))
            }
            BinaryOp::Eq
            | BinaryOp::Ne
            | BinaryOp::Lt
            | BinaryOp::Gt
            | BinaryOp::Le
            | BinaryOp::Ge => self.gen_compare(l, r, op),
            BinaryOp::And | BinaryOp::Or => {
                let lb = self.to_bool(l)?;
                let rb = self.to_bool(r)?;
                let instr = if *op == BinaryOp::And { "and" } else { "or" };
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = {} i1 {}, {}\n",
                    t, instr, lb.repr, rb.repr
                ));
                Ok(Val::new(Ty::Bool, t))
            }
            BinaryOp::BitAnd
            | BinaryOp::BitOr
            | BinaryOp::BitXor
            | BinaryOp::Shl
            | BinaryOp::Shr => {
                let li = self.cast(l, &Ty::I64)?;
                let ri = self.cast(r, &Ty::I64)?;
                let instr = match op {
                    BinaryOp::BitAnd => "and",
                    BinaryOp::BitOr => "or",
                    BinaryOp::BitXor => "xor",
                    BinaryOp::Shl => "shl",
                    BinaryOp::Shr => "ashr",
                    _ => unreachable!(),
                };
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = {} i64 {}, {}\n",
                    t, instr, li.repr, ri.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            BinaryOp::Is => {
                let li = self.cast(l, &Ty::I64)?;
                let ri = self.cast(r, &Ty::I64)?;
                let t = self.new_var();
                self.buf
                    .push_str(&format!("  {} = icmp eq i64 {}, {}\n", t, li.repr, ri.repr));
                Ok(Val::new(Ty::Bool, t))
            }
            BinaryOp::In => Err(CompilerError::codegen_error(
                "'in' needs list/dict/object rhs",
            )),
            BinaryOp::Repeated => Err(CompilerError::codegen_error(
                "repeated operator not supported",
            )),
        }
    }

    fn gen_compare(&mut self, l: Val, r: Val, op: &BinaryOp) -> Result<Val> {
        if !l.ty.is_numeric() || !r.ty.is_numeric() {
            return Err(CompilerError::codegen_error(
                "comparison needs numeric operands",
            ));
        }
        let target = if l.ty == Ty::F64 || r.ty == Ty::F64 {
            Ty::F64
        } else {
            Ty::I64
        };
        let li = self.cast(l, &target)?;
        let ri = self.cast(r, &target)?;
        let t = self.new_var();
        if target == Ty::F64 {
            let pred = match op {
                BinaryOp::Eq => "oeq",
                BinaryOp::Ne => "one",
                BinaryOp::Lt => "olt",
                BinaryOp::Gt => "ogt",
                BinaryOp::Le => "ole",
                BinaryOp::Ge => "oge",
                _ => unreachable!(),
            };
            self.buf.push_str(&format!(
                "  {} = fcmp {} double {}, {}\n",
                t, pred, li.repr, ri.repr
            ));
        } else {
            let pred = match op {
                BinaryOp::Eq => "eq",
                BinaryOp::Ne => "ne",
                BinaryOp::Lt => "slt",
                BinaryOp::Gt => "sgt",
                BinaryOp::Le => "sle",
                BinaryOp::Ge => "sge",
                _ => unreachable!(),
            };
            self.buf.push_str(&format!(
                "  {} = icmp {} i64 {}, {}\n",
                t, pred, li.repr, ri.repr
            ));
        }
        Ok(Val::new(Ty::Bool, t))
    }

    /* ===================================================================
     * Value conversions
     * ================================================================= */

    fn cast(&mut self, v: Val, target: &Ty) -> Result<Val> {
        if v.ty == *target {
            return Ok(v);
        }
        match (&v.ty, target) {
            (Ty::Bool, Ty::I64) => {
                let t = self.new_var();
                self.buf
                    .push_str(&format!("  {} = zext i1 {} to i64\n", t, v.repr));
                Ok(Val::new(Ty::I64, t))
            }
            (Ty::I64, Ty::Bool)
            | (Ty::F64, Ty::Bool)
            | (Ty::Str, Ty::Bool)
            | (Ty::Obj, Ty::Bool) => self.to_bool(v),
            (Ty::I64, Ty::F64) => {
                let t = self.new_var();
                self.buf
                    .push_str(&format!("  {} = sitofp i64 {} to double\n", t, v.repr));
                Ok(Val::new(Ty::F64, t))
            }
            (Ty::Bool, Ty::F64) => {
                let i = self.cast(v, &Ty::I64)?;
                self.cast(i, &Ty::F64)
            }
            (Ty::Obj, Ty::I64) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = ptrtoint %vredrs.object* {} to i64\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            (Ty::I64, Ty::Obj) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = inttoptr i64 {} to %vredrs.object*\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Obj, t).with_class(v.class.clone().unwrap_or_default()))
            }
            (Ty::Str, Ty::I64) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = ptrtoint %vredrs.str* {} to i64\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            (Ty::I64, Ty::Str) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_of_i64(i64 {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Str, t))
            }
            (Ty::List, Ty::I64)
            | (Ty::Dict, Ty::I64)
            | (Ty::Tuple, Ty::I64)
            | (Ty::Coro, Ty::I64) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = ptrtoint {} {} to i64\n",
                    t,
                    v.ty.ir(),
                    v.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            (Ty::I64, Ty::List) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = inttoptr i64 {} to %vredrs.list*\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::List, t))
            }
            (Ty::I64, Ty::Dict) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = inttoptr i64 {} to %vredrs.dict*\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Dict, t))
            }
            (Ty::I64, Ty::Tuple) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = inttoptr i64 {} to %vredrs.tuple*\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Tuple, t))
            }
            (Ty::Void, _) => Ok(Val::new(target.clone(), "0")),
            (Ty::Value, Ty::I64) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_value_get_i64(%vredrs.value {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            (Ty::Value, Ty::Str) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_value_get_str(%vredrs.value {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Str, t))
            }
            (Ty::Value, Ty::F64) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call double @vredrs_value_get_f64(%vredrs.value {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::F64, t))
            }
            (Ty::Value, Ty::Bool) => self.to_bool(v),
            (Ty::I64, Ty::Value) => self.wrap_value(v),
            (Ty::F64, Ty::Value) => self.wrap_value(v),
            (Ty::Bool, Ty::Value) => self.wrap_value(v),
            (Ty::Str, Ty::Value) => self.wrap_value(v),
            (Ty::List, Ty::Value) => self.wrap_value(v),
            (Ty::Dict, Ty::Value) => self.wrap_value(v),
            (Ty::Tuple, Ty::Value) => self.wrap_value(v),
            (Ty::Obj, Ty::Value) => self.wrap_value(v),
            (Ty::Value, Ty::Obj) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.object* @vredrs_value_get_obj(%vredrs.value {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Obj, t))
            }
            (Ty::Value, Ty::List) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_value_get_list(%vredrs.value {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::List, t))
            }
            (Ty::Value, Ty::Dict) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.dict* @vredrs_value_get_dict(%vredrs.value {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Dict, t))
            }
            (Ty::Value, Ty::Tuple) => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.tuple* @vredrs_value_get_tuple(%vredrs.value {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Tuple, t))
            }
            _ => Err(CompilerError::codegen_error(format!(
                "cannot cast {} to {}",
                v.ty.ir(),
                target.ir()
            ))),
        }
    }


}
