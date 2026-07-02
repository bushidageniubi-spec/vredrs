//! C* physical intermediate representation (PIR).
//!
//! PIR is deliberately independent from the interpreter `Value` enum.  It is a
//! physical, no-runtime representation used only by `vredrs build --raw`: raw
//! pointers, linear ownership, sections, ISR records, package metadata, firmware
//! patch metadata, prefetch markers, DMA/pipeline markers and inline assembly.

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PirType {
    Void,
    I1,
    I8,
    I32,
    I64,
    F64,
    Ptr(Box<PirType>),
}

impl PirType {
    pub fn is_ptr(&self) -> bool {
        matches!(self, PirType::Ptr(_))
    }

    pub fn llvm(&self) -> String {
        match self {
            PirType::Void => "void".to_string(),
            PirType::I1 => "i1".to_string(),
            PirType::I8 => "i8".to_string(),
            PirType::I32 => "i32".to_string(),
            PirType::I64 => "i64".to_string(),
            PirType::F64 => "double".to_string(),
            PirType::Ptr(inner) => format!("{}*", inner.llvm()),
        }
    }

    pub fn ptr_erased() -> Self {
        PirType::Ptr(Box::new(PirType::I8))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PirOperand {
    Var(String),
    Int(i64),
    Bool(bool),
    NullPtr,
}

impl PirOperand {
    pub fn as_var(&self) -> Option<&str> {
        match self {
            PirOperand::Var(v) => Some(v),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum PirNode {
    /// C*: pointer load, e.g. `set, v = p.load()` or a lowered `p[i]` read.
    Load {
        dst: String,
        ptr: PirOperand,
        ty: PirType,
        volatile: bool,
    },
    /// C*: pointer store, e.g. `p.store(v)` or a lowered `p[i] = v` write.
    Store {
        ptr: PirOperand,
        value: PirOperand,
        ty: PirType,
        volatile: bool,
    },
    /// C*: direct raw function call; `free`/`consume` are linear effects.
    Call {
        dst: Option<String>,
        func: String,
        args: Vec<PirOperand>,
        ret_ty: PirType,
    },
    /// C*: `asm { ... }`; emitted as LLVM inline asm sideeffect.
    AsmBlock {
        template: String,
        inputs: Vec<PirOperand>,
        outputs: Vec<String>,
    },
    /// C*: `malloc(n) as ptr[T]`; allocation creates a MustConsume resource.
    AllocLinear {
        dst: String,
        bytes: PirOperand,
        ty: PirType,
    },
    /// C*: `move(p) as ptr[T]`; transfers linear ownership.
    Move {
        dst: String,
        src: String,
        ty: PirType,
    },
    /// C*: `consume(p)`; explicitly consumes without calling free.
    Consume { var: String },
    /// C*: `@pipeline` marker; the scheduler/emitter emits DMA + compute windows.
    Pipeline {
        function: String,
        priority: String,
        dma_stage: String,
        compute_stage: String,
    },
    /// C*: `@prefetch` marker; emitted as `llvm.prefetch` in the function body.
    Prefetch {
        ptr: Option<PirOperand>,
        hint: String,
        stride: i64,
    },
    /// C*: a physical loop marker.  Phase 3 keeps it structured but lowers body once
    /// with clear LLVM comments/markers so the path is not silently ignored.
    Loop { label: String, body: Vec<PirNode> },
    /// C*: `@patch` metadata; side artifacts generate the byte-level patch file.
    PatchMarker {
        function: String,
        base: String,
        output: String,
    },
    /// C*: package/repo physical layout metadata emitted to `.pkgmeta`.
    PackageMarker {
        name: String,
        partition: String,
        size: u64,
        offset: u64,
        checksum: u64,
    },
}

#[derive(Debug, Clone)]
pub struct PirFunction {
    pub name: String,
    pub params: Vec<(String, PirType)>,
    pub ret_ty: PirType,
    pub body: Vec<PirNode>,
    pub section: Option<String>,
    pub is_pipeline: bool,
    pub is_prefetch: bool,
}

#[derive(Debug, Clone)]
pub struct PirExtern {
    pub name: String,
    pub params: Vec<PirType>,
    pub ret_ty: PirType,
    pub link: String,
}

#[derive(Debug, Clone)]
pub struct IsrEntry {
    pub handler: String,
    pub priority: i64,
    pub budget_us: i64,
    pub wcet_cycles: u64,
}

#[derive(Debug, Clone)]
pub struct PatchPlan {
    pub function: String,
    pub base: String,
    pub output: String,
}

#[derive(Debug, Clone)]
pub struct PackageEntry {
    pub name: String,
    pub partition: String,
    pub size: u64,
    pub offset: u64,
    pub checksum: u64,
}

#[derive(Debug, Clone, Default)]
pub struct RepoConfig {
    pub enabled: bool,
    pub sources: Vec<String>,
    pub split_by: String,
    pub output_header: String,
    pub output_map: String,
    pub output_binary: String,
}

#[derive(Debug, Clone, Default)]
pub struct PirProgram {
    pub nodes: Vec<PirNode>,
    pub functions: Vec<PirFunction>,
    pub externs: Vec<PirExtern>,
    pub isr_entries: Vec<IsrEntry>,
    pub patch_plans: Vec<PatchPlan>,
    pub packages: Vec<PackageEntry>,
    pub repo: RepoConfig,
    pub needs_linker_script: bool,
}

impl PirProgram {
    pub fn push(&mut self, node: PirNode) {
        self.nodes.push(node);
    }
    pub fn push_function(&mut self, f: PirFunction) {
        self.functions.push(f);
    }
    pub fn push_extern(&mut self, e: PirExtern) {
        self.externs.push(e);
    }
    pub fn push_isr(&mut self, e: IsrEntry) {
        self.needs_linker_script = true;
        self.isr_entries.push(e);
    }
    pub fn push_patch(&mut self, p: PatchPlan) {
        self.needs_linker_script = true;
        self.patch_plans.push(p);
    }
    pub fn push_package(&mut self, p: PackageEntry) {
        self.needs_linker_script = true;
        self.packages.push(p);
    }
    pub fn mark_linker_script(&mut self) {
        self.needs_linker_script = true;
    }
    pub fn enable_repo(&mut self, repo: RepoConfig) {
        self.repo = repo;
        self.needs_linker_script = true;
    }
}
