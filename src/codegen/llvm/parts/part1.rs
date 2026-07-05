impl FullLlvmGen {
    pub fn new() -> Self {
        let builtins: std::collections::HashSet<&'static str> = [
            "len",
            "str",
            "int",
            "float",
            "bool",
            "type_of",
            "range",
            "enumerate",
            "zip",
            "map",
            "filter",
            "sum",
            "min",
            "max",
            "sorted",
            "reversed",
            "print",
            "println",
            "paste",
            "input",
            "open",
            "read",
            "write",
            "close",
            "exit",
            "dict",
            "dict_get",
            "dict_set",
            "dict_keys",
            "dict_values",
            "dict_has",
            "read_file",
            "write_file",
            "file_exists",
            "annotations",
        ]
        .into_iter()
        .collect();
        FullLlvmGen {
            buf: String::new(),
            var_n: 0,
            lbl_n: 0,
            local_n: 0,
            str_n: 0,
            strs: HashMap::new(),
            functions: HashMap::new(),
            classes: HashMap::new(),
            loop_stack: Vec::new(),
            builtins,
            imports: Vec::new(),
            external_sigs: HashMap::new(),
            external_classes: HashMap::new(),
            emit_main: true,
            lambdas: Vec::new(),
            lambda_n: 0,
            fn_annotations: HashMap::new(),
            lambda_bodies: Vec::new(),
            closure_sigs: HashMap::new(),
        }
    }

    /// Register function signatures from other modules so this module can
    /// call them. Functions that are ALSO defined in this module will be
    /// overwritten by `register_fn_sig` during `generate()`.
    pub fn register_external_sigs(&mut self, sigs: HashMap<String, Sig>) {
        self.external_sigs = sigs;
    }

    /// Register class metadata from other modules so this module can
    /// construct objects of classes defined in imported modules.
    pub fn register_external_classes(
        &mut self,
        classes: HashMap<String, (Option<String>, Vec<(String, Sig)>)>,
    ) {
        self.external_classes = classes;
    }

    pub fn generate(&mut self, program: &Program) -> Result<String> {
        self.buf.clear();
        self.var_n = 0;
        self.lbl_n = 0;
        self.local_n = 0;
        self.str_n = 0;
        self.strs.clear();
        self.functions.clear();
        self.classes.clear();
        self.loop_stack.clear();
        self.lambdas.clear();
        self.lambda_n = 0;
        self.fn_annotations.clear();
        self.lambda_bodies.clear();
        self.closure_sigs.clear();
        self.collect_imports(program);
        // Track which classes are locally defined (defined in THIS module's
        // source) so we only emit vtables for those.
        let mut local_class_names: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for d in &program.declarations {
            if let TopLevel::ClassDef(c) = d {
                local_class_names.insert(c.name.name.clone());
            }
        }
        self.collect_classes(program)?;
        self.collect_function_sigs(program)?;
        // Register external (cross-module) signatures AFTER this module's own
        // signatures, so locally-defined functions take precedence but
        // imported ones are still callable.
        for (name, sig) in &self.external_sigs {
            self.functions
                .entry(name.clone())
                .or_insert_with(|| sig.clone());
        }
        // Register external (cross-module) classes as stubs. Locally-defined
        // classes (collected by collect_classes below) take precedence.
        for (name, (parent, methods)) in &self.external_classes {
            if self.classes.contains_key(name) {
                continue;
            }
            let mut method_infos = Vec::new();
            let mut method_idx = HashMap::new();
            for (i, (mname, msig)) in methods.iter().enumerate() {
                let llvm_name = format!("{}__{}", self.safe(name), self.safe(mname));
                method_idx.insert(mname.clone(), i);
                method_infos.push(MethodInfo {
                    name: mname.clone(),
                    index: i,
                    llvm_name,
                    sig: msig.clone(),
                    fn_def: FnDef {
                        annotations: Vec::new(),
                        name: Identifier {
                            name: mname.clone(),
                            span: crate::error::Span::dummy(),
                        },
                        params: Vec::new(),
                        return_type: None,
                        body: Vec::new(),
                        is_constexpr: false,
                        is_lazy: false,
                        is_async: false,
                        is_extern: false,
                        extern_link: None,
                        type_constraints: std::collections::HashMap::new(),
                        span: crate::error::Span::dummy(),
                    },
                    inherited: false,
                });
            }
            self.classes.insert(
                name.clone(),
                ClassInfo {
                    name: name.clone(),
                    parent: parent.clone(),
                    fields: Vec::new(),
                    methods: method_infos,
                    method_idx,
                },
            );
        }
        self.collect_strings(program)?;
        for s in [
            "<object>",
            "<list>",
            "<dict>",
            "<coroutine>",
            "true",
            "false",
            "",
            "int",
            "float",
            "bool",
            "str",
            "dict",
            "tuple",
            "set",
            "object",
            "coroutine",
            "any",
            "nil",
        ] {
            self.intern(s);
        }
        self.emit_header();
        self.emit_external_decls(program);
        self.emit_class_vtables(&local_class_names);
        let classes: Vec<ClassInfo> = self.classes.values().cloned().collect();
        for c in &classes {
            // Only generate method bodies for classes defined in THIS module.
            if !local_class_names.contains(&c.name) {
                continue;
            }
            for m in &c.methods {
                if m.inherited {
                    continue;
                }
                self.gen_method_function(c, m)?;
            }
        }
        for d in &program.declarations {
            match d {
                TopLevel::FnDef(f) if f.is_async => self.gen_async_function(f)?,
                TopLevel::FnDef(f) => self.gen_function(f)?,
                TopLevel::LazyFnDef(l) if l.fn_def.is_async => {
                    self.gen_async_function(&l.fn_def)?
                }
                TopLevel::LazyFnDef(l) => self.gen_function(&l.fn_def)?,
                _ => {}
            }
        }
        if self.emit_main {
            self.gen_main(program)?;
        }
        // Append all lifted lambda function bodies at module scope.
        if !self.lambda_bodies.is_empty() {
            self.buf.push_str("\n; --- Lifted lambda functions ---\n");
            for body in std::mem::take(&mut self.lambda_bodies) {
                self.buf.push_str(&body);
            }
        }
        // Now that all codegen is done, emit the string globals by replacing
        // the placeholder in the buffer.
        let mut globals = String::new();
        if !self.strs.is_empty() {
            globals.push_str("; --- String literals ---\n");
            let mut entries: Vec<(&String, &String)> = self.strs.iter().collect();
            entries.sort_by_key(|(_, n)| *n);
            for (text, name) in entries {
                let bytes = text.as_bytes();
                let len = bytes.len() + 1;
                let mut esc = String::new();
                for &b in bytes {
                    match b {
                        b'\n' => esc.push_str("\\0A"),
                        b'\r' => esc.push_str("\\0D"),
                        b'\t' => esc.push_str("\\09"),
                        b'\\' => esc.push_str("\\5C"),
                        b'"' => esc.push_str("\\22"),
                        0x20..=0x7e => esc.push(b as char),
                        b => esc.push_str(&format!("\\{:02X}", b)),
                    }
                }
                esc.push_str("\\00");
                globals.push_str(&format!(
                    "{} = private unnamed_addr constant [{} x i8] c\"{}\", align 1\n",
                    name, len, esc
                ));
            }
            globals.push('\n');
        }
        self.buf = self
            .buf
            .replace("; __STRING_GLOBALS_PLACEHOLDER__\n", &globals);
        Ok(self.buf.clone())
    }

    /// Emit `declare` statements for functions that are called but not
    /// defined in this module (i.e. they come from imported modules).
    fn emit_external_decls(&mut self, program: &Program) {
        // Collect names of functions defined in THIS module.
        let mut locally_defined: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut locally_defined_classes: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        for d in &program.declarations {
            if let TopLevel::FnDef(f) = d {
                locally_defined.insert(f.name.name.clone());
            }
            if let TopLevel::LazyFnDef(l) = d {
                locally_defined.insert(l.fn_def.name.name.clone());
            }
            if let TopLevel::ClassDef(c) = d {
                locally_defined_classes.insert(c.name.name.clone());
            }
        }
        let mut emitted: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut decls = String::new();
        for (name, sig) in &self.functions {
            if locally_defined.contains(name) {
                continue;
            }
            if emitted.contains(name) {
                continue;
            }
            if self.builtins.contains(name.as_str()) {
                continue;
            }
            if self.classes.contains_key(name) {
                continue;
            }
            emitted.insert(name.clone());
            let mut params = Vec::new();
            for p in &sig.params {
                params.push(p.ir().to_string());
            }
            decls.push_str(&format!(
                "declare {} @{}({})\n",
                sig.ret.ir(),
                self.safe(name),
                params.join(", ")
            ));
        }
        // External class vtables: declare them as external globals so this
        // module can reference @ClassName.vtable without defining it.
        for name in self.classes.keys() {
            if locally_defined_classes.contains(name) {
                continue;
            }
            decls.push_str(&format!(
                "@{}.vtable = external global %vredrs.vtable\n",
                self.safe(name)
            ));
        }
        if !decls.is_empty() {
            self.buf.push_str("; --- Cross-module declarations ---\n");
            self.buf.push_str(&decls);
            self.buf.push('\n');
        }
    }

    fn collect_imports(&mut self, program: &Program) {
        for d in &program.declarations {
            if let TopLevel::Import(imp) = d {
                self.imports.push(imp.clone());
            }
        }
    }

    fn collect_classes(&mut self, program: &Program) -> Result<()> {
        for d in &program.declarations {
            if let TopLevel::ClassDef(c) = d {
                let parent = match &c.extends {
                    Some(TypeExpr::Named(id, _)) => Some(id.name.clone()),
                    _ => None,
                };
                self.classes.insert(
                    c.name.name.clone(),
                    ClassInfo {
                        name: c.name.name.clone(),
                        parent,
                        fields: Vec::new(),
                        methods: Vec::new(),
                        method_idx: HashMap::new(),
                    },
                );
            }
        }
        for d in &program.declarations {
            if let TopLevel::ClassDef(c) = d {
                let class_name = c.name.name.clone();
                let parent = self.classes[&class_name].parent.clone();
                let mut methods: Vec<MethodInfo> = Vec::new();
                let mut method_idx: HashMap<String, usize> = HashMap::new();
                if let Some(pn) = &parent {
                    if !self.classes.contains_key(pn) {
                        return Err(CompilerError::codegen_error(format!(
                            "class '{}' extends unknown class '{}'",
                            class_name, pn
                        )));
                    }
                    for m in self.classes[pn].methods.clone() {
                        let idx = methods.len();
                        method_idx.insert(m.name.clone(), idx);
                        methods.push(MethodInfo {
                            inherited: true,
                            ..m
                        });
                    }
                }
                let mut fields: Vec<FieldInfo> = Vec::new();
                if let Some(pn) = &parent {
                    fields.extend(self.classes[pn].fields.iter().cloned());
                }
                for f in &c.fields {
                    fields.push(FieldInfo {
                        name: f.name.name.clone(),
                        ty: self.ty_of_annot(f.type_annotation.as_ref())?,
                        default: f.default_value.clone(),
                    });
                }
                for m in &c.methods {
                    let idx = if method_idx.contains_key(&m.name.name) {
                        method_idx[&m.name.name]
                    } else {
                        let i = methods.len();
                        methods.push(MethodInfo {
                            name: String::new(),
                            index: 0,
                            llvm_name: String::new(),
                            sig: Sig {
                                params: Vec::new(),
                                ret: Ty::I64,
                            },
                            fn_def: m.clone(),
                            inherited: false,
                        });
                        i
                    };
                    let mut params: Vec<Ty> = Vec::new();
                    for p in &m.params {
                        params.push(self.ty_of_annot(p.type_annotation.as_ref())?);
                    }
                    let ret = match &m.return_type {
                        Some(t) => self.ty_of_te(t)?,
                        None => Ty::Value,
                    };
                    let llvm_name =
                        format!("{}__{}", self.safe(&class_name), self.safe(&m.name.name));
                    methods[idx] = MethodInfo {
                        name: m.name.name.clone(),
                        index: idx,
                        llvm_name,
                        sig: Sig { params, ret },
                        fn_def: m.clone(),
                        inherited: false,
                    };
                    method_idx.insert(m.name.name.clone(), idx);
                }
                if let Some(c) = self.classes.get_mut(&class_name) {
                    c.fields = fields;
                    let methods_clone = methods.clone();
                    c.methods = methods;
                    c.method_idx = method_idx;
                    // Pre-intern the class name and all method names so they
                    // appear in the string globals section emitted by
                    // emit_header().
                    self.intern(&class_name);
                    for m in &methods_clone {
                        self.intern(&m.name);
                    }
                }
            }
        }
        Ok(())
    }

    fn collect_function_sigs(&mut self, program: &Program) -> Result<()> {
        for d in &program.declarations {
            match d {
                TopLevel::FnDef(f) => self.register_fn_sig(f)?,
                TopLevel::LazyFnDef(l) => self.register_fn_sig(&l.fn_def)?,
                _ => {}
            }
        }
        Ok(())
    }

    /// Detect if a function body contains a yield statement (making it a generator).
    fn body_has_yield(&self, body: &[Stmt]) -> bool {
        for s in body {
            if self.stmt_has_yield(s) {
                return true;
            }
        }
        false
    }

    fn stmt_has_yield(&self, s: &Stmt) -> bool {
        match s {
            Stmt::Yield(_) => true,
            Stmt::If(i) => {
                self.body_has_yield(&i.then_body)
                    || i.elif_chain.iter().any(|(_, b)| self.body_has_yield(b))
                    || i.else_body
                        .as_ref()
                        .map(|b| self.body_has_yield(b))
                        .unwrap_or(false)
            }
            Stmt::While(w) => {
                self.body_has_yield(&w.body)
                    || w.else_body
                        .as_ref()
                        .map(|b| self.body_has_yield(b))
                        .unwrap_or(false)
            }
            Stmt::ForIn(f) => {
                self.body_has_yield(&f.body)
                    || f.else_body
                        .as_ref()
                        .map(|b| self.body_has_yield(b))
                        .unwrap_or(false)
            }
            Stmt::ForRange(f) => {
                self.body_has_yield(&f.body)
                    || f.else_body
                        .as_ref()
                        .map(|b| self.body_has_yield(b))
                        .unwrap_or(false)
            }
            Stmt::Loop(l) => {
                self.body_has_yield(&l.body)
                    || l.else_body
                        .as_ref()
                        .map(|b| self.body_has_yield(b))
                        .unwrap_or(false)
            }
            Stmt::Try(t) => {
                self.body_has_yield(&t.try_body)
                    || t.catch_body
                        .as_ref()
                        .map(|b| self.body_has_yield(b))
                        .unwrap_or(false)
                    || t.finally_body
                        .as_ref()
                        .map(|b| self.body_has_yield(b))
                        .unwrap_or(false)
            }
            Stmt::With(w) => self.body_has_yield(&w.body),
            Stmt::Match(m) => {
                m.cases.iter().any(|c| self.body_has_yield(&c.body))
                    || m.else_case
                        .as_ref()
                        .map(|b| self.body_has_yield(b))
                        .unwrap_or(false)
            }
            Stmt::Defer(d) => self.stmt_has_yield(&d.stmt),
            _ => false,
        }
    }

    fn register_fn_sig(&mut self, f: &FnDef) -> Result<()> {
        // Stash annotation metadata (encoded as a JSON string) so the
        // `annotations(fn)` builtin can materialize a dict at runtime.
        if !f.annotations.is_empty() {
            self.fn_annotations
                .insert(f.name.name.clone(), encode_annotations_json(&f.annotations));
        }
        if f.is_async {
            self.functions.insert(
                f.name.name.clone(),
                Sig {
                    params: Vec::new(),
                    ret: Ty::Coro,
                },
            );
            return Ok(());
        }
        // A function containing yield is a generator: calling it returns a coro.
        // Generators take no parameters in 1.0 (parameters would require closure
        // capture, which is not yet supported).
        let is_generator = self.body_has_yield(&f.body);
        if is_generator {
            self.functions.insert(
                f.name.name.clone(),
                Sig {
                    params: Vec::new(),
                    ret: Ty::Coro,
                },
            );
            return Ok(());
        }
        let mut params = Vec::new();
        for p in &f.params {
            params.push(self.ty_of_annot(p.type_annotation.as_ref())?);
        }
        let ret = match &f.return_type {
            Some(t) => self.ty_of_te(t)?,
            None => Ty::Value,
        };
        self.functions
            .insert(f.name.name.clone(), Sig { params, ret });
        Ok(())
    }


}
