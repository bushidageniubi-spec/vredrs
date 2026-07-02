//! Low-level LLVM IR text emitter.
//!
//! Encapsulates buffer management and SSA variable/label generation so the
//! codegen layer never touches `format!` + `push_str` directly.

use std::fmt::Write;

#[derive(Clone, Copy)]
pub struct Ssa(pub usize);
#[derive(Clone, Copy)]
pub struct Lbl(pub usize);

pub struct IrEmitter {
    buf: String,
    tmp: usize,
    slot: usize,
    lbl: usize,
}

impl IrEmitter {
    pub fn new(capacity: usize) -> Self {
        IrEmitter {
            buf: String::with_capacity(capacity),
            tmp: 0,
            slot: 0,
            lbl: 0,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.buf
    }
    pub fn finish(self) -> String {
        self.buf
    }

    pub fn fresh(&mut self) -> Ssa {
        let n = self.tmp;
        self.tmp += 1;
        Ssa(n)
    }

    pub fn fresh_slot(&mut self, hint: &str) -> String {
        let n = self.slot;
        self.slot += 1;
        let safe: String = hint
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        format!("%{}.addr{}", safe, n)
    }

    pub fn fresh_lbl(&mut self) -> Lbl {
        let n = self.lbl;
        self.lbl += 1;
        Lbl(n)
    }

    #[inline]
    pub fn raw(&mut self, s: &str) {
        self.buf.push_str(s);
    }

    #[inline]
    pub fn line(&mut self, args: std::fmt::Arguments<'_>) {
        let _ = write!(self.buf, "{}", args);
    }

    pub fn assign(&mut self, rhs: &str) -> String {
        let n = self.fresh();
        let _ = write!(self.buf, "  %t{} = {}\n", n.0, rhs);
        format!("%t{}", n.0)
    }

    pub fn call(&mut self, ret_ty: &str, func: &str, args: &[String]) -> String {
        let joined = args.join(", ");
        let f = if func.starts_with("@") {
            func.to_string()
        } else {
            format!("@{}", func)
        };
        self.assign(&format!("call {} {}({})", ret_ty, f, joined))
    }

    pub fn branch(&mut self, cond: &str, t: Lbl, f: Lbl) {
        let _ = write!(
            self.buf,
            "  br i1 {}, label %L{}, label %L{}\n",
            cond, t.0, f.0
        );
    }

    pub fn jump(&mut self, l: Lbl) {
        let _ = write!(self.buf, "  br label %L{}\n", l.0);
    }

    pub fn mark(&mut self, l: Lbl) {
        let _ = write!(self.buf, "L{}:\n", l.0);
    }

    pub fn store(&mut self, ty: &str, val: &str, ptr: &str, align: usize) {
        let _ = write!(
            self.buf,
            "  store {} {}, {}* {}, align {}\n",
            ty, val, ty, ptr, align
        );
    }

    pub fn load(&mut self, ty: &str, ptr: &str, align: usize) -> String {
        self.assign(&format!("load {}, {}* {}, align {}", ty, ty, ptr, align))
    }

    pub fn alloca(&mut self, ty: &str, align: usize) -> String {
        let ptr = self.fresh_slot("tmp");
        let _ = write!(self.buf, "  {} = alloca {}, align {}\n", ptr, ty, align);
        ptr
    }

    pub fn ret(&mut self, ty: &str, val: &str) {
        let _ = write!(self.buf, "  ret {} {}\n", ty, val);
    }

    pub fn ret_void(&mut self) {
        self.buf.push_str("  ret void\n");
    }
    pub fn unreachable(&mut self) {
        self.buf.push_str("  unreachable\n");
    }
    pub fn comment(&mut self, text: &str) {
        let _ = write!(self.buf, "  ; {}\n", text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_produces_monotonic_names() {
        let mut e = IrEmitter::new(64);
        assert_eq!(e.fresh().0, 0);
        assert_eq!(e.fresh().0, 1);
        assert_eq!(e.fresh().0, 2);
    }

    #[test]
    fn assign_emits_correct_syntax() {
        let mut e = IrEmitter::new(64);
        let name = e.assign("add i64 1, 2");
        assert_eq!(name, "%t0");
        assert_eq!(e.as_str(), "  %t0 = add i64 1, 2\n");
    }

    #[test]
    fn call_joins_args_with_commas() {
        let mut e = IrEmitter::new(64);
        let name = e.call("i64", "@foo", &["i64 1".into(), "i64 2".into()]);
        assert_eq!(name, "%t0");
        assert_eq!(e.as_str(), "  %t0 = call i64 @foo(i64 1, i64 2)\n");
    }

    #[test]
    fn branch_and_label_round_trip() {
        let mut e = IrEmitter::new(64);
        let t = e.fresh_lbl();
        let f = e.fresh_lbl();
        e.branch("true", t, f);
        e.mark(t);
        e.jump(f);
        e.mark(f);
        assert_eq!(
            e.as_str(),
            "  br i1 true, label %L0, label %L1\nL0:\n  br label %L1\nL1:\n"
        );
    }

    #[test]
    fn alloca_returns_unique_slot_names() {
        let mut e = IrEmitter::new(64);
        let a = e.alloca("i64", 8);
        let b = e.alloca("i64", 8);
        assert_ne!(a, b);
        assert!(a.starts_with("%tmp.addr"));
    }

    #[test]
    fn load_store_emit_aligned_access() {
        let mut e = IrEmitter::new(64);
        let ptr = e.alloca("i64", 8);
        e.store("i64", "42", &ptr, 8);
        let loaded = e.load("i64", &ptr, 8);
        assert!(e
            .as_str()
            .contains(&format!("store i64 42, i64* {}, align 8", ptr)));
        assert!(e
            .as_str()
            .contains(&format!("{} = load i64, i64* {}, align 8", loaded, ptr)));
    }
}
