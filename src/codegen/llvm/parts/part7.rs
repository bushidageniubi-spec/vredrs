impl FullLlvmGen {
    fn gen_builtin_call(
        &mut self,
        name: &str,
        args: Vec<Val>,
        _vars: &mut HashMap<String, LocalSlot>,
    ) -> Result<Val> {
        macro_rules! arg0 {
            () => {
                args.get(0).cloned().unwrap_or(Val::new(Ty::I64, "0"))
            };
        }
        macro_rules! arg1 {
            () => {
                args.get(1).cloned().unwrap_or(Val::new(Ty::I64, "0"))
            };
        }

        match name {
            "len" => {
                let v = arg0!();
                let len_fn = match v.ty {
                    Ty::Str => "@vredrs_len_str",
                    Ty::List => "@vredrs_len_list",
                    Ty::Dict | Ty::Set => "@vredrs_len_dict",
                    Ty::Tuple => "@vredrs_len_tuple",
                    _ => {
                        return Err(CompilerError::codegen_error(
                            "len() needs str/list/dict/tuple",
                        ))
                    }
                };
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 {}({} {})\n",
                    t,
                    len_fn,
                    v.ty.ir(),
                    v.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            "str" => {
                let v = arg0!();
                let s = self.to_str(v, _vars)?;
                Ok(s)
            }
            "int" => {
                let v = arg0!();
                let t = self.new_var();
                match v.ty {
                    Ty::Str => self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_int_of_str(%vredrs.str* {})\n",
                        t, v.repr
                    )),
                    Ty::F64 => self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_int_of_f64(double {})\n",
                        t, v.repr
                    )),
                    Ty::Bool => {
                        let i = self.cast(v, &Ty::I64)?;
                        return Ok(i);
                    }
                    _ => return Err(CompilerError::codegen_error("int() needs str/f64/bool")),
                }
                Ok(Val::new(Ty::I64, t))
            }
            "float" => {
                let v = arg0!();
                let t = self.new_var();
                match v.ty {
                    Ty::Str => self.buf.push_str(&format!(
                        "  {} = call double @vredrs_float_of_str(%vredrs.str* {})\n",
                        t, v.repr
                    )),
                    Ty::I64 => self.buf.push_str(&format!(
                        "  {} = call double @vredrs_float_of_i64(i64 {})\n",
                        t, v.repr
                    )),
                    _ => return Err(CompilerError::codegen_error("float() needs str/i64")),
                }
                Ok(Val::new(Ty::F64, t))
            }
            "bool" => self.to_bool(arg0!()),
            "type_of" => {
                let v = arg0!();
                let name = match v.ty {
                    Ty::I64 => "int",
                    Ty::F64 => "float",
                    Ty::Bool => "bool",
                    Ty::Str => "str",
                    Ty::List => "list",
                    Ty::Dict => "dict",
                    Ty::Tuple => "tuple",
                    Ty::Set => "set",
                    Ty::Obj => "object",
                    Ty::Coro => "coroutine",
                    _ => "any",
                };
                let ptr = self.intern_str_for(name)?;
                Ok(Val::new(Ty::Str, ptr))
            }
            "range" => {
                let a = arg0!();
                let b = arg1!();
                let start = self.cast(a, &Ty::I64)?;
                let end = self.cast(b, &Ty::I64)?;
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_range(i64 {}, i64 {})\n",
                    t, start.repr, end.repr
                ));
                Ok(Val::new(Ty::List, t))
            }
            "enumerate" => {
                let v = arg0!();
                if v.ty != Ty::List {
                    return Err(CompilerError::codegen_error("enumerate() needs list"));
                }
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_enumerate(%vredrs.list* {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::List, t))
            }
            "zip" => {
                let a = arg0!();
                let b = arg1!();
                if a.ty != Ty::List || b.ty != Ty::List {
                    return Err(CompilerError::codegen_error("zip() needs two lists"));
                }
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_zip(%vredrs.list* {}, %vredrs.list* {})\n",
                    t, a.repr, b.repr
                ));
                Ok(Val::new(Ty::List, t))
            }
            "sum" => {
                let v = arg0!();
                if v.ty != Ty::List {
                    return Err(CompilerError::codegen_error("sum() needs list"));
                }
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_sum(%vredrs.list* {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            "min" => {
                let v = arg0!();
                if v.ty != Ty::List {
                    return Err(CompilerError::codegen_error("min() needs list"));
                }
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_min(%vredrs.list* {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            "max" => {
                let v = arg0!();
                if v.ty != Ty::List {
                    return Err(CompilerError::codegen_error("max() needs list"));
                }
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_max(%vredrs.list* {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            "sorted" => {
                let v = arg0!();
                if v.ty != Ty::List {
                    return Err(CompilerError::codegen_error("sorted() needs list"));
                }
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_sorted(%vredrs.list* {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::List, t))
            }
            "reversed" => {
                let v = arg0!();
                if v.ty != Ty::List {
                    return Err(CompilerError::codegen_error("reversed() needs list"));
                }
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_reversed(%vredrs.list* {})\n",
                    t, v.repr
                ));
                Ok(Val::new(Ty::List, t))
            }
            "print" | "paste" => {
                for a in &args {
                    match a.ty {
                        Ty::I64 => self
                            .buf
                            .push_str(&format!("  call i64 @vredrs_print_i64(i64 {})\n", a.repr)),
                        Ty::F64 => self.buf.push_str(&format!(
                            "  call i64 @vredrs_print_f64(double {})\n",
                            a.repr
                        )),
                        Ty::Bool => {
                            let bi = self.cast(a.clone(), &Ty::I64)?;
                            self.buf.push_str(&format!(
                                "  call i64 @vredrs_print_bool(i64 {})\n",
                                bi.repr
                            ));
                        }
                        Ty::Str => self.buf.push_str(&format!(
                            "  call i64 @vredrs_print_str(%vredrs.str* {})\n",
                            a.repr
                        )),
                        Ty::Value => self.buf.push_str(&format!(
                            "  call i64 @vredrs_print_value(%vredrs.value {})\n",
                            a.repr
                        )),
                        _ => {
                            let s_ptr = self.to_str(a.clone(), _vars)?;
                            self.buf.push_str(&format!(
                                "  call i64 @vredrs_print_str(%vredrs.str* {})\n",
                                s_ptr.repr
                            ));
                        }
                    }
                }
                Ok(Val::new(Ty::Void, ""))
            }
            "println" => {
                for a in &args {
                    match a.ty {
                        Ty::I64 => self
                            .buf
                            .push_str(&format!("  call i64 @vredrs_print_i64(i64 {})\n", a.repr)),
                        Ty::F64 => self.buf.push_str(&format!(
                            "  call i64 @vredrs_print_f64(double {})\n",
                            a.repr
                        )),
                        Ty::Bool => {
                            let bi = self.cast(a.clone(), &Ty::I64)?;
                            self.buf.push_str(&format!(
                                "  call i64 @vredrs_print_bool(i64 {})\n",
                                bi.repr
                            ));
                        }
                        Ty::Str => self.buf.push_str(&format!(
                            "  call i64 @vredrs_print_str(%vredrs.str* {})\n",
                            a.repr
                        )),
                        Ty::Value => self.buf.push_str(&format!(
                            "  call i64 @vredrs_print_value(%vredrs.value {})\n",
                            a.repr
                        )),
                        _ => {
                            let s_ptr = self.to_str(a.clone(), _vars)?;
                            self.buf.push_str(&format!(
                                "  call i64 @vredrs_print_str(%vredrs.str* {})\n",
                                s_ptr.repr
                            ));
                        }
                    }
                }
                self.buf.push_str("  call i64 @vredrs_println()\n");
                Ok(Val::new(Ty::Void, ""))
            }
            "input" => {
                let prompt = arg0!();
                let prompt_str = self.to_str(prompt, _vars)?;
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_input(%vredrs.str* {})\n",
                    t, prompt_str.repr
                ));
                Ok(Val::new(Ty::Str, t))
            }
            "open" => {
                let p = arg0!();
                let m = arg1!();
                let ps = self.to_str(p, _vars)?;
                let ms = self.to_str(m, _vars)?;
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_open(%vredrs.str* {}, %vredrs.str* {})\n",
                    t, ps.repr, ms.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            "close" => {
                let h = self.cast(arg0!(), &Ty::I64)?;
                self.buf
                    .push_str(&format!("  call void @vredrs_close(i64 {})\n", h.repr));
                Ok(Val::new(Ty::Void, ""))
            }
            "read" => {
                let h = self.cast(arg0!(), &Ty::I64)?;
                let n = self.cast(arg1!(), &Ty::I64)?;
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_read(i64 {}, i64 {})\n",
                    t, h.repr, n.repr
                ));
                Ok(Val::new(Ty::Str, t))
            }
            "write" => {
                let h = self.cast(arg0!(), &Ty::I64)?;
                let s = self.to_str(arg1!(), _vars)?;
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_write(i64 {}, %vredrs.str* {})\n",
                    t, h.repr, s.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            "read_file" => {
                let p = self.to_str(arg0!(), _vars)?;
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.str* @vredrs_read_file(%vredrs.str* {})\n",
                    t, p.repr
                ));
                Ok(Val::new(Ty::Str, t))
            }
            "write_file" => {
                let p = self.to_str(arg0!(), _vars)?;
                let c = self.to_str(arg1!(), _vars)?;
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_write_file(%vredrs.str* {}, %vredrs.str* {})\n",
                    t, p.repr, c.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            "file_exists" => {
                let p = self.to_str(arg0!(), _vars)?;
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_file_exists(%vredrs.str* {})\n",
                    t, p.repr
                ));
                let b = self.new_var();
                self.buf
                    .push_str(&format!("  {} = icmp ne i64 {}, 0\n", b, t));
                Ok(Val::new(Ty::Bool, b))
            }
            "exit" => {
                let code = self.cast(arg0!(), &Ty::I64)?;
                self.buf.push_str(&format!(
                    "  call void @vredrs_exit(i64 {})\n  unreachable\n",
                    code.repr
                ));
                Ok(Val::new(Ty::Void, ""))
            }
            "dict" => {
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.dict* @vredrs_dict_new()\n",
                    t
                ));
                Ok(Val::new(Ty::Dict, t))
            }
            "dict_get" => {
                let d = arg0!();
                let k = arg1!();
                let ks = self.to_str(k, _vars)?;
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_dict_get(%vredrs.dict* {}, %vredrs.str* {}, i64 0)\n",
                    t, d.repr, ks.repr
                ));
                Ok(Val::new(Ty::I64, t))
            }
            "dict_set" => {
                let d = arg0!();
                let k = arg1!();
                let v = args.get(2).cloned().unwrap_or(Val::new(Ty::I64, "0"));
                let ks = self.to_str(k, _vars)?;
                let raw = self.value_to_i64_slot(v)?;
                self.buf.push_str(&format!(
                    "  call void @vredrs_dict_set(%vredrs.dict* {}, %vredrs.str* {}, i64 {})\n",
                    d.repr, ks.repr, raw
                ));
                Ok(Val::new(Ty::Void, ""))
            }
            "dict_keys" => {
                let d = arg0!();
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_dict_keys(%vredrs.dict* {})\n",
                    t, d.repr
                ));
                Ok(Val::new(Ty::List, t))
            }
            "dict_values" => {
                let d = arg0!();
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_dict_values(%vredrs.dict* {})\n",
                    t, d.repr
                ));
                Ok(Val::new(Ty::List, t))
            }
            "dict_has" => {
                let d = arg0!();
                let k = arg1!();
                let ks = self.to_str(k, _vars)?;
                let t = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_dict_has(%vredrs.dict* {}, %vredrs.str* {})\n",
                    t, d.repr, ks.repr
                ));
                let b = self.new_var();
                self.buf
                    .push_str(&format!("  {} = icmp ne i64 {}, 0\n", b, t));
                Ok(Val::new(Ty::Bool, b))
            }
            "map" | "filter" => {
                // map(fn, list) -> list, filter(fn, list) -> list
                // The first arg is a closure (Ty::Fn i64 fn-ptr); the second
                // is a list. We inline the loop in IR because the C runtime
                // does not know how to call back into Vredrs closures.
                if args.len() != 2 {
                    return Err(CompilerError::codegen_error(format!(
                        "{} expects 2 args (fn, list), got {}",
                        name,
                        args.len()
                    )));
                }
                let closure = args[0].clone();
                let list_val = args[1].clone();
                if closure.ty != Ty::Fn {
                    return Err(CompilerError::codegen_error(format!(
                        "{} first arg must be a closure, got {:?}",
                        name, closure.ty
                    )));
                }
                if list_val.ty != Ty::List {
                    return Err(CompilerError::codegen_error(format!(
                        "{} second arg must be a list, got {:?}",
                        name, list_val.ty
                    )));
                }
                let sig = self.closure_sigs.get(&closure.repr).cloned().ok_or_else(
                    || CompilerError::codegen_error("map/filter closure without sig"),
                )?;
                if sig.params.len() != 1 {
                    return Err(CompilerError::codegen_error(
                        "map/filter closure must take 1 param",
                    ));
                }
                // Build the function pointer type for a 1-arg closure.
                let fn_ty = "%vredrs.value (%vredrs.value, ptr)*";
                let fn_ptr_typed = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = inttoptr i64 {} to {}\n",
                    fn_ptr_typed, closure.repr, fn_ty
                ));
                // Result list.
                let result = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.list* @vredrs_list_new()\n",
                    result
                ));
                let len = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call i64 @vredrs_list_len(%vredrs.list* {})\n",
                    len, list_val.repr
                ));
                let loop_lbl = self.new_lbl();
                let body_lbl = self.new_lbl();
                let done_lbl = self.new_lbl();
                let i_slot = self.new_local("__mf_i");
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
                    cond, cur, len
                ));
                self.buf.push_str(&format!(
                    "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
                    cond, body_lbl, done_lbl, body_lbl
                ));
                let elt = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.value @vredrs_list_get(%vredrs.list* {}, i64 {})\n",
                    elt, list_val.repr, cur
                ));
                let call_res = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = call %vredrs.value {}(%vredrs.value {}, ptr null)\n",
                    call_res, fn_ptr_typed, elt
                ));
                if name == "map" {
                    self.buf.push_str(&format!(
                        "  call void @vredrs_list_push(%vredrs.list* {}, %vredrs.value {})\n",
                        result, call_res
                    ));
                } else {
                    // filter: push only if call_res is truthy.
                    let keep = self.new_var();
                    self.buf.push_str(&format!(
                        "  {} = call i64 @vredrs_value_truthy(%vredrs.value {})\n",
                        keep, call_res
                    ));
                    let push_lbl = self.new_lbl();
                    let skip_lbl = self.new_lbl();
                    self.buf.push_str(&format!(
                        "  br i1 {}, label %L{}, label %L{}\nL{}:\n",
                        keep, push_lbl, skip_lbl, push_lbl
                    ));
                    self.buf.push_str(&format!(
                        "  call void @vredrs_list_push(%vredrs.list* {}, %vredrs.value {})\n",
                        result, elt
                    ));
                    self.buf.push_str(&format!(
                        "  br label %L{}\nL{}:\n",
                        skip_lbl, skip_lbl
                    ));
                }
                let next = self.new_var();
                self.buf.push_str(&format!(
                    "  {} = add i64 {}, 1\n  store i64 {}, i64* {}, align 8\n  br label %L{}\nL{}:\n",
                    next, cur, next, i_slot, loop_lbl, done_lbl
                ));
                Ok(Val::new(Ty::List, result))
            }
            "annotations" => {
                // This arm is unreachable in practice: `gen_call` intercepts
                // `annotations(identifier)` before args are lowered and
                // dispatches directly to `gen_annotations_dict`. We keep the
                // arm here so the match is exhaustive; if the user calls
                // `annotations` with a non-identifier arg, they get a clear
                // error rather than a generic "unknown builtin".
                let _ = args;
                Err(CompilerError::codegen_error(
                    "annotations() in native backend requires an identifier argument (e.g. annotations(my_fn))",
                ))
            }
            other => Err(CompilerError::codegen_error(format!(
                "unknown builtin '{}'",
                other
            ))),
        }
    }


}
