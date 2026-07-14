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
    /// While loop with condition string (LLVM IR condition expression).
    WhileLoop { label: String, condition: String, body: Vec<PirNode> },
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
    /// C*: a comment emitted verbatim into the LLVM IR. Used by the permissive
    /// lowering path to record statements/expressions that the strict C* PIR
    /// lowering rejects, so the output remains valid even when the source uses
    /// Vredrs-level constructs that have no direct physical lowering yet.
    Comment(String),
    /// C*: integer binary operation on two operands.  Used by the permissive
    /// lowering path to lower `set, y = x + 8` style assignments that the
    /// strict C* PIR lowering would reject.  `op` is one of "add", "sub",
    /// "mul", "div", "mod", "and", "or", "xor", "shl", "shr", "eq", "ne",
    /// "lt", "gt", "le", "ge".
    BinOp {
        dst: String,
        op: String,
        lhs: PirOperand,
        rhs: PirOperand,
        ty: PirType,
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
    /// P0.1: Auto-generated interrupt vector table entries.
    /// Each entry maps an exception/IRQ name to a handler function.
    pub vector_table: Vec<VectorTableEntry>,
    /// P0.2: Memory map for dynamic linker script generation.
    pub memory_map: Option<MemoryMap>,
    /// P0.3: Stack size in bytes (default: entire SRAM minus heap).
    pub stack_size: Option<u64>,
    /// P0.3: Heap size in bytes (default: 0).
    pub heap_size: Option<u64>,
    /// P2.9: Critical sections — list of function names marked @critical.
    pub critical_sections: Vec<String>,
    /// P2.10: Scheduler configuration from @scheduler annotation.
    pub scheduler: Option<SchedulerConfig>,
    /// P4.13: Hardware breakpoints — list of (address, handler) pairs.
    pub hw_breakpoints: Vec<(u64, String)>,
}

/// P0.1: A single interrupt vector table entry.
#[derive(Debug, Clone)]
pub struct VectorTableEntry {
    /// Exception/IRQ name: "reset", "nmi", "hard_fault", "svcall",
    /// "pendsv", "systick", or "irq0", "irq1", etc.
    pub name: String,
    /// Handler function name (or "_default_handler").
    pub handler: String,
    /// IRQ number for external interrupts (0-based, starting after system exceptions).
    pub irq_number: Option<i64>,
}

/// P0.2: Memory map for dynamic linker script generation.
#[derive(Debug, Clone, Default)]
pub struct MemoryMap {
    pub flash_origin: u64,
    pub flash_length: u64,
    pub sram_origin: u64,
    pub sram_length: u64,
    pub mmio_origin: Option<u64>,
    pub mmio_length: Option<u64>,
}

/// P2.10: Scheduler configuration.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    pub tick_ms: u64,
    pub max_tasks: u64,
    pub stack_size: u64,
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
    /// P0.1: Push a vector table entry.
    pub fn push_vector_entry(&mut self, entry: VectorTableEntry) {
        self.needs_linker_script = true;
        self.vector_table.push(entry);
    }
    /// P0.2: Set the memory map.
    pub fn set_memory_map(&mut self, map: MemoryMap) {
        self.needs_linker_script = true;
        self.memory_map = Some(map);
    }
    /// P0.3: Set stack size.
    pub fn set_stack_size(&mut self, size: u64) {
        self.needs_linker_script = true;
        self.stack_size = Some(size);
    }
    /// P0.3: Set heap size.
    pub fn set_heap_size(&mut self, size: u64) {
        self.needs_linker_script = true;
        self.heap_size = Some(size);
    }
    /// P2.9: Mark a function as a critical section.
    pub fn push_critical_section(&mut self, func_name: String) {
        self.critical_sections.push(func_name);
    }
    /// P2.10: Set scheduler configuration.
    pub fn set_scheduler(&mut self, config: SchedulerConfig) {
        self.needs_linker_script = true;
        self.scheduler = Some(config);
    }
    /// P4.13: Push a hardware breakpoint.
    pub fn push_hw_breakpoint(&mut self, addr: u64, handler: String) {
        self.hw_breakpoints.push((addr, handler));
    }
}
