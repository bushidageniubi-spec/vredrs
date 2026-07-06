impl FullLlvmGen {
    fn to_bool(&mut self, v: Val) -> Result<Val> {
        match v.ty {
            Ty::Bool => Ok(v),
            Ty::I64 => {
                let t = self.new_var();
                self.buf
                    .push_str(&format!("  {} = icmp ne i64 {}, 0\n", t, v.repr));
                Ok(Val::new(Ty::Bool, t))
            }
            Ty::F64 => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = fcmp one double {}, 0.000000e+00\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Bool, t))
            }
            Ty::Str => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = icmp ne %vredrs.str* {}, null\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Bool, t))
            }
            Ty::Obj => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = icmp ne %vredrs.object* {}, null\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Bool, t))
            }
            Ty::List => {
                let len = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_list_len(%vredrs.list* {})\n",
                    len, v.repr
                ));
                let t = self.new_var();
                self.buf
                    .push_str(&format!("  {} = icmp ne i64 {}, 0\n", t, len));
                Ok(Val::new(Ty::Bool, t))
            }
            Ty::Value => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_value_truthy(%vredrs.value {})\n",
                    t, v.repr
                ));
                let b = self.new_var();
                self.buf
                    .push_str(&format!("  {} = trunc i64 {} to i1\n", b, t));
                Ok(Val::new(Ty::Bool, b))
            }
            _ => Err(CompilerError::codegen_error(format!(
                "cannot coerce {} to bool",
                v.ty.ir()
            ))),
        }
    }

    fn to_str(&mut self, v: Val, _vars: &mut HashMap<String, LocalSlot>) -> Result<Val> {
        match v.ty {
            Ty::Str => Ok(v),
            Ty::I64 => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_of_i64(i64 {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Str, t))
            }
            Ty::F64 => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_of_f64(double {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Str, t))
            }
            Ty::Bool => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_of_bool(i64 {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Str, t))
            }
            Ty::Obj => {
                let t = self.new_var();
                let ptr_i8 = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = bitcast %vredrs.object* {} to ptr\n",
                    ptr_i8, v.repr
                ));
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_of_ptr(ptr {})\n",
                    t, ptr_i8
                ));
                Ok(Val::new(Ty::Str, t))
            }
            Ty::Coro => {
                let label = self.intern("<coroutine>");
                let t = self.new_var();
                let cptr = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = getelementptr inbounds [12 x i8], ptr {}, i64 0, i64 0\n",
                    cptr, label
                ));
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_from_cstr(ptr {})\n",
                    t, cptr
                ));
                Ok(Val::new(Ty::Str, t))
            }
            Ty::List => {
                let label = self.intern("<list>");
                let t = self.new_var();
                let cptr = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = getelementptr inbounds [7 x i8], ptr {}, i64 0, i64 0\n",
                    cptr, label
                ));
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_from_cstr(ptr {})\n",
                    t, cptr
                ));
                Ok(Val::new(Ty::Str, t))
            }
            Ty::Dict | Ty::Set => {
                let label = self.intern("<dict>");
                let t = self.new_var();
                let cptr = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = getelementptr inbounds [7 x i8], ptr {}, i64 0, i64 0\n",
                    cptr, label
                ));
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_from_cstr(ptr {})\n",
                    t, cptr
                ));
                Ok(Val::new(Ty::Str, t))
            }
            Ty::Tuple => {
                let label = self.intern("<tuple>");
                let t = self.new_var();
                let cptr = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = getelementptr inbounds [8 x i8], ptr {}, i64 0, i64 0\n",
                    cptr, label
                ));
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_from_cstr(ptr {})\n",
                    t, cptr
                ));
                Ok(Val::new(Ty::Str, t))
            }
            _ => {
                // For other types, fall back to a placeholder string.
                let label = self.intern("<value>");
                let t = self.new_var();
                let cptr = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = getelementptr inbounds [8 x i8], ptr {}, i64 0, i64 0\n",
                    cptr, label
                ));
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_str_from_cstr(ptr {})\n",
                    t, cptr
                ));
                Ok(Val::new(Ty::Str, t))
            }
        }
    }

    fn to_tuple(&mut self, v: Val, _vars: &mut HashMap<String, LocalSlot>) -> Result<Val> {
        match v.ty {
            Ty::Tuple => Ok(v),
            Ty::I64 => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = inttoptr i64 {} to %vredrs.tuple*\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::Tuple, t))
            }
            _ => Err(CompilerError::codegen_error("to_tuple needs tuple or i64")),
        }
    }

    fn value_to_i64_slot(&mut self, v: Val) -> Result<String> {
        match v.ty {
            Ty::I64 => Ok(v.repr),
            Ty::Bool => {
                let t = self.new_var();
                self.buf
                    .push_str(&format!("  {} = zext i1 {} to i64\n", t, v.repr));
                Ok(t)
            }
            Ty::F64 => {
                let t = self.new_var();
                self.buf
                    .push_str(&format!("  {} = bitcast double {} to i64\n", t, v.repr));
                Ok(t)
            }
            Ty::Str | Ty::List | Ty::Dict | Ty::Tuple | Ty::Set | Ty::Obj | Ty::Coro | Ty::Ptr => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = ptrtoint {} {} to i64\n",
                    t,
                    v.ty.ir(),
                    v.repr
                ));
                Ok(t)
            }
            Ty::Void => Ok("0".to_string()),
            Ty::Value => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_value_get_i64(%vredrs.value {})\n",
                    t, v.repr
                ));
                Ok(t)
            }
            Ty::Fn => Ok(v.repr),
        }
    }

    /// Wrap any IR value into a tagged %vredrs.value based on its IR type.
    /// Used when storing into a container (list/dict/tuple) or object field.
    fn wrap_value(&mut self, v: Val) -> Result<Val> {
        let make_fn = match v.ty {
            Ty::I64 => "@vredrs_value_make_i64",
            Ty::F64 => "@vredrs_value_make_f64",
            Ty::Bool => "@vredrs_value_make_bool",
            Ty::Str => "@vredrs_value_make_str",
            Ty::List => "@vredrs_value_make_list",
            Ty::Dict | Ty::Set => "@vredrs_value_make_dict",
            Ty::Tuple => "@vredrs_value_make_tuple",
            Ty::Obj => "@vredrs_value_make_obj",
            Ty::Coro => "@vredrs_value_make_coro",
            Ty::Fn => "@vredrs_value_make_i64",
            Ty::Void | Ty::Ptr | Ty::Value => return Ok(v),
        };
        let t = self.new_var();
        self.buf.push_str(&format!(
            "  {} = call %vredrs.value {}({} {})\n",
            t,
            make_fn,
            v.ty.ir(),
            v.repr
        ));
        Ok(Val::new(Ty::Value, t))
    }

    fn i64_slot_to_value(&mut self, raw: String, ty: &Ty) -> Result<Val> {
        match ty {
            Ty::I64 => Ok(Val::new(Ty::I64, raw)),
            Ty::Bool => {
                let t = self.new_var();
                self.buf
                    .push_str(&format!("  {} = trunc i64 {} to i1\n", t, raw));
                Ok(Val::new(Ty::Bool, t))
            }
            Ty::F64 => {
                let t = self.new_var();
                self.buf
                    .push_str(&format!("  {} = bitcast i64 {} to double\n", t, raw));
                Ok(Val::new(Ty::F64, t))
            }
            Ty::Str => {
                let t = self.new_var();
                self.buf
                    .push_str(&format!("  {} = inttoptr i64 {} to %vredrs.str*\n", t, raw));
                Ok(Val::new(Ty::Str, t))
            }
            Ty::List => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = inttoptr i64 {} to %vredrs.list*\n",
                    t, raw
                ));
                Ok(Val::new(Ty::List, t))
            }
            Ty::Dict | Ty::Set => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = inttoptr i64 {} to %vredrs.dict*\n",
                    t, raw
                ));
                Ok(Val::new(Ty::Dict, t))
            }
            Ty::Tuple => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = inttoptr i64 {} to %vredrs.tuple*\n",
                    t, raw
                ));
                Ok(Val::new(Ty::Tuple, t))
            }
            Ty::Obj => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = inttoptr i64 {} to %vredrs.object*\n",
                    t, raw
                ));
                Ok(Val::new(Ty::Obj, t))
            }
            _ => Err(CompilerError::codegen_error(format!(
                "cannot decode i64 slot to {}",
                ty.ir()
            ))),
        }
    }

    fn default_value_for_type(&self, ty: &Ty) -> Val {
        match ty {
            Ty::I64 => Val::new(Ty::I64, "0"),
            Ty::F64 => Val::new(Ty::F64, "0.000000e+00"),
            Ty::Bool => Val::new(Ty::Bool, "0"),
            Ty::Str => Val::new(Ty::Str, "null"),
            Ty::Obj => Val::new(Ty::Obj, "null"),
            _ => Val::new(Ty::I64, "0"),
        }
    }

    /* ===================================================================
     * Variable & class helpers
     * ================================================================= */

    fn load_local(&mut self, name: &str, vars: &mut HashMap<String, LocalSlot>) -> Result<Val> {
        let slot = vars
            .get(name)
            .ok_or_else(|| CompilerError::codegen_error(format!("unknown local '{}'", name)))?;
        if slot.ptr == "%coro.addr.synthetic" {
            return Ok(
                Val::new(Ty::Coro, "%coro").with_async(slot.async_fn.clone().unwrap_or_default())
            );
        }
        let t = self.new_var();
        self.buf.push_str(&format!(
            "  {} = load {}, {}* {}, align 8\n",
            t,
            slot.ty.ir(),
            slot.ty.ir(),
            slot.ptr
        ));
        let mut v = Val::new(slot.ty.clone(), t);
        v.class = slot.class.clone();
        v.async_fn = slot.async_fn.clone();
        // If this slot holds a closure, register the loaded SSA register
        // in closure_sigs so call sites can look up the signature.
        if v.ty == Ty::Fn {
            if let Some(sig) = &slot.closure_sig {
                self.closure_sigs.insert(v.repr.clone(), sig.clone());
            }
        }
        Ok(v)
    }

    fn emit_rc_inc(&mut self, v: &Val) {
        if v.ty == Ty::Obj {
            self.buf.push_str(&format!(
                "  call void @vredrs_inc_ref(%vredrs.object* {})\n",
                v.repr
            ));
        }
    }

    fn emit_rc_dec_slot(&mut self, slot: &LocalSlot) {
        if slot.ty == Ty::Obj {
            let old = self.new_var();
            self.buf.push_str(&format!(
                "  {} = load %vredrs.object*, %vredrs.object** {}, align 8\n",
                old, slot.ptr
            ));
            self.buf.push_str(&format!(
                "  call void @vredrs_dec_ref(%vredrs.object* {})\n",
                old
            ));
        }
    }

    fn class_has_method(&self, class: &str, method: &str) -> bool {
        self.classes
            .get(class)
            .map(|c| c.method_idx.contains_key(method))
            .unwrap_or(false)
    }

    fn method_info(&self, class: &str, method: &str) -> Result<&MethodInfo> {
        let c = self
            .classes
            .get(class)
            .ok_or_else(|| CompilerError::codegen_error(format!("unknown class '{}'", class)))?;
        let idx = c.method_idx.get(method).ok_or_else(|| {
            CompilerError::codegen_error(format!("class '{}' has no method '{}'", class, method))
        })?;
        Ok(&c.methods[*idx])
    }

    fn find_field(&self, class: &str, name: &str) -> Option<&FieldInfo> {
        self.classes
            .get(class)
            .and_then(|c| c.fields.iter().find(|f| f.name == name))
    }

    /* ===================================================================
     * String collection (walks AST to intern all string literals)
     * ================================================================= */

    fn collect_strings(&mut self, program: &Program) -> Result<()> {
        for d in &program.declarations {
            self.collect_strings_top(d)?;
        }
        Ok(())
    }

    fn collect_strings_top(&mut self, d: &TopLevel) -> Result<()> {
        match d {
            TopLevel::Statement(s) => self.collect_strings_stmt(s),
            TopLevel::FnDef(f) => self.collect_strings_fn(f),
            TopLevel::LazyFnDef(l) => self.collect_strings_fn(&l.fn_def),
            TopLevel::ClassDef(c) => {
                for f in &c.fields {
                    self.intern(&f.name.name);
                    if let Some(d) = &f.default_value {
                        self.collect_strings_expr(d)?;
                    }
                }
                for m in &c.methods {
                    self.intern(&m.name.name);
                    self.collect_strings_fn(m)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn collect_strings_fn(&mut self, f: &FnDef) -> Result<()> {
        for s in &f.body {
            self.collect_strings_stmt(s)?;
        }
        Ok(())
    }

    fn collect_strings_stmt(&mut self, s: &Stmt) -> Result<()> {
        match s {
            Stmt::Println(p) => {
                for a in &p.args {
                    self.collect_strings_expr(a)?;
                }
            }
            Stmt::Paste(p) => {
                for a in &p.args {
                    self.collect_strings_expr(a)?;
                }
            }
            Stmt::Assign(a) => {
                for t in &a.targets {
                    self.collect_strings_assignee(t)?;
                }
                self.collect_strings_expr(&a.value)?;
            }
            Stmt::Return(r) => {
                for v in &r.values {
                    self.collect_strings_expr(v)?;
                }
            }
            Stmt::Throw(t) => self.collect_strings_expr(&t.value)?,
            Stmt::Assert(a) => self.collect_strings_expr(&a.condition)?,
            Stmt::Panic(p) => self.collect_strings_expr(&p.message)?,
            Stmt::Spawn(sp) => self.collect_strings_expr(&sp.call)?,
            Stmt::SpawnThread(sp) => self.collect_strings_expr(&sp.call)?,
            Stmt::Yield(y) => {
                if let Some(v) = &y.value {
                    self.collect_strings_expr(v)?;
                }
            }
            Stmt::If(i) => {
                self.collect_strings_expr(&i.condition)?;
                for s in &i.then_body {
                    self.collect_strings_stmt(s)?;
                }
                for (c, b) in &i.elif_chain {
                    self.collect_strings_expr(c)?;
                    for s in b {
                        self.collect_strings_stmt(s)?;
                    }
                }
                if let Some(b) = &i.else_body {
                    for s in b {
                        self.collect_strings_stmt(s)?;
                    }
                }
            }
            Stmt::While(w) => {
                self.collect_strings_expr(&w.condition)?;
                for s in &w.body {
                    self.collect_strings_stmt(s)?;
                }
            }
            Stmt::ForIn(f) => {
                self.collect_strings_expr(&f.iterable)?;
                for s in &f.body {
                    self.collect_strings_stmt(s)?;
                }
            }
            Stmt::ForRange(f) => {
                self.collect_strings_expr(&f.from)?;
                self.collect_strings_expr(&f.to)?;
                for s in &f.body {
                    self.collect_strings_stmt(s)?;
                }
            }
            Stmt::Loop(l) => {
                for s in &l.body {
                    self.collect_strings_stmt(s)?;
                }
            }
            Stmt::Match(m) => {
                self.collect_strings_expr(&m.expr)?;
                for c in &m.cases {
                    if let Some(g) = &c.guard {
                        self.collect_strings_expr(g)?;
                    }
                    for s in &c.body {
                        self.collect_strings_stmt(s)?;
                    }
                }
            }
            Stmt::Try(t) => {
                for s in &t.try_body {
                    self.collect_strings_stmt(s)?;
                }
                if let Some(b) = &t.catch_body {
                    for s in b {
                        self.collect_strings_stmt(s)?;
                    }
                }
                if let Some(b) = &t.finally_body {
                    for s in b {
                        self.collect_strings_stmt(s)?;
                    }
                }
            }
            Stmt::With(w) => {
                self.collect_strings_expr(&w.manager)?;
                for s in &w.body {
                    self.collect_strings_stmt(s)?;
                }
            }
            Stmt::Defer(d) => self.collect_strings_stmt(&d.stmt)?,
            Stmt::Expr(e) => self.collect_strings_expr(&e.expr)?,
            Stmt::Input(i) => {
                if let Some(p) = &i.prompt {
                    self.intern(p);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn collect_strings_assignee(&mut self, t: &Assignee) -> Result<()> {
        match t {
            Assignee::Identifier(_) | Assignee::Qualified(_) => {}
            Assignee::Index(i) => {
                self.collect_strings_expr(&i.target)?;
                self.collect_strings_expr(&i.index)?;
            }
            Assignee::Member(m) => {
                self.intern(&m.member.name);
                self.collect_strings_expr(&m.target)?;
            }
            Assignee::Tuple(items) => {
                for i in items {
                    self.collect_strings_assignee(i)?;
                }
            }
        }
        Ok(())
    }

    fn collect_strings_expr(&mut self, e: &Expr) -> Result<()> {
        match e {
            Expr::String_(s) => {
                // For pure-text strings, intern the literal. For interpolated
                // strings, intern each text part individually (the
                // interpolation runtime path uses these).
                for p in &s.parts {
                    if let StringPart::Text(t) = p {
                        self.intern(&interpret_escapes(t));
                    } else if let StringPart::Interpolation(inner) = p {
                        self.collect_strings_expr(inner)?;
                    }
                }
            }
            Expr::MultiLineString(s) => {
                for p in &s.parts {
                    if let StringPart::Text(t) = p {
                        self.intern(&interpret_escapes(t));
                    } else if let StringPart::Interpolation(inner) = p {
                        self.collect_strings_expr(inner)?;
                    }
                }
            }
            Expr::Binary(b) => {
                self.collect_strings_expr(&b.left)?;
                self.collect_strings_expr(&b.right)?;
            }
            Expr::Unary(u) => self.collect_strings_expr(&u.operand)?,
            Expr::Call(c) => {
                self.collect_strings_expr(&c.callee)?;
                for a in &c.args {
                    self.collect_strings_expr(a)?;
                }
            }
            Expr::MethodCall(m) => {
                self.intern(&m.method.name);
                self.collect_strings_expr(&m.receiver)?;
                for a in &m.args {
                    self.collect_strings_expr(a)?;
                }
            }
            Expr::Index(i) => {
                self.collect_strings_expr(&i.target)?;
                self.collect_strings_expr(&i.index)?;
            }
            Expr::MemberAccess(m) => {
                self.intern(&m.member.name);
                self.collect_strings_expr(&m.target)?;
            }
            Expr::Ternary(t) => {
                self.collect_strings_expr(&t.condition)?;
                self.collect_strings_expr(&t.true_branch)?;
                self.collect_strings_expr(&t.false_branch)?;
            }
            Expr::Cast(c) => self.collect_strings_expr(&c.expr)?,
            Expr::Range(r) => {
                if let Some(s) = &r.start {
                    self.collect_strings_expr(s)?;
                }
                if let Some(e) = &r.end {
                    self.collect_strings_expr(e)?;
                }
            }
            Expr::List(l) => {
                for e in &l.elements {
                    self.collect_strings_expr(e)?;
                }
            }
            Expr::Tuple(t) => {
                for e in &t.elements {
                    self.collect_strings_expr(e)?;
                }
            }
            Expr::Dict(d) => {
                for (k, v) in &d.entries {
                    self.collect_strings_expr(k)?;
                    self.collect_strings_expr(v)?;
                }
            }
            Expr::Set(s) => {
                for e in &s.elements {
                    self.collect_strings_expr(e)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /* ===================================================================
     * Misc helpers
     * ================================================================= */

    fn safe(&self, name: &str) -> String {
        name.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    }

    fn new_local(&mut self, name: &str) -> String {
        let safe = self.safe(name);
        let n = self.local_n;
        self.local_n += 1;
        format!("%{}.addr{}", safe, n)
    }

    fn new_var(&mut self) -> String {
        let v = format!("%t{}", self.var_n);
        self.var_n += 1;
        v
    }

    fn new_lbl(&mut self) -> usize {
        let l = self.lbl_n;
        self.lbl_n += 1;
        l
    }

    fn stmt_kind(&self, s: &Stmt) -> &'static str {
        match s {
            Stmt::Assign(_) => "Assign",
            Stmt::Pon(_) => "Pon",
            Stmt::TableAssign(_) => "TableAssign",
            Stmt::Paste(_) => "Paste",
            Stmt::Println(_) => "Println",
            Stmt::Flush(_) => "Flush",
            Stmt::Input(_) => "Input",
            Stmt::Return(_) => "Return",
            Stmt::Throw(_) => "Throw",
            Stmt::Defer(_) => "Defer",
            Stmt::Assert(_) => "Assert",
            Stmt::Panic(_) => "Panic",
            Stmt::Spawn(_) => "Spawn",
            Stmt::SpawnThread(_) => "SpawnThread",
            Stmt::Yield(_) => "Yield",
            Stmt::If(_) => "If",
            Stmt::While(_) => "While",
            Stmt::ForIn(_) => "ForIn",
            Stmt::ForRange(_) => "ForRange",
            Stmt::Loop(_) => "Loop",
            Stmt::Break(_) => "Break",
            Stmt::Continue(_) => "Continue",
            Stmt::Match(_) => "Match",
            Stmt::Try(_) => "Try",
            Stmt::With(_) => "With",
            Stmt::Select(_) => "Select",
            Stmt::UnsafeBlock(_) => "UnsafeBlock",
            Stmt::Asm(_) => "Asm",
            Stmt::DirectiveBlock(_) => "DirectiveBlock",
            Stmt::ScopeBlock(_) => "ScopeBlock",
            Stmt::Expr(_) => "Expr",
        }
    }

    fn expr_kind(&self, e: &Expr) -> &'static str {
        match e {
            Expr::Pipe(_) => "Pipe",
            Expr::NullCoalesce(_) => "NullCoalesce",
            Expr::Binary(_) => "Binary",
            Expr::Unary(_) => "Unary",
            Expr::Postfix(_) => "Postfix",
            Expr::Call(_) => "Call",
            Expr::MethodCall(_) => "MethodCall",
            Expr::Index(_) => "Index",
            Expr::Slice(_) => "Slice",
            Expr::MemberAccess(_) => "MemberAccess",
            Expr::OptionalChain(_) => "OptionalChain",
            Expr::Spread(_) => "Spread",
            Expr::Ternary(_) => "Ternary",
            Expr::Lambda(_) => "Lambda",
            Expr::Spawn(_) => "Spawn",
            Expr::Coro(_) => "Coro",
            Expr::Resume(_) => "Resume",
            Expr::Await(_) => "Await",
            Expr::Cast(_) => "Cast",
            Expr::TryPropagate(_) => "TryPropagate",
            Expr::Range(_) => "Range",
            Expr::Integer(_) => "Integer",
            Expr::Float(_) => "Float",
            Expr::String_(_) => "String",
            Expr::MultiLineString(_) => "MultiLineString",
            Expr::Bool(_) => "Bool",
            Expr::Null(_) => "Null",
            Expr::List(_) => "List",
            Expr::ListComprehension(_) => "ListComprehension",
            Expr::Dict(_) => "Dict",
            Expr::DictComprehension(_) => "DictComprehension",
            Expr::Set(_) => "Set",
            Expr::SetComprehension(_) => "SetComprehension",
            Expr::Tuple(_) => "Tuple",
            Expr::Identifier(_) => "Identifier",
            Expr::Qualified(_) => "Qualified",
        }
    }

}
