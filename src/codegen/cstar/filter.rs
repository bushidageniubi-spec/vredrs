//! # cstar/filter — Static IR 过滤器（手册 Phase 4）
//!
//! 复活 Cstar：从"啃 AST 的死代码"转变为"Static IR 的中间过滤器"。
//!
//! ## 工作内容（手册 §6.4）
//!
//! 1. **识别循环结构**：扫描 IR 中的 `Jump` 和 `Label` 模式，标记循环入口/出口。
//! 2. **检测内存访问模式**：识别连续的 `Load`/`Store` 指令，判断是否可 DMA 化。
//! 3. **插入 `@pipeline` 标记**：在可 DMA 化的循环前插入 `Prefetch` 指令。
//! 4. **计算 WCET**：统计循环体的指令数，与 `@isr_group` 预算比较，超限处插入警告。
//! 5. **生成差分元数据**：在 `Comment` 节点中嵌入符号偏移信息，供 `@patch` 使用。
//!
//! ## 激活条件
//!
//! 默认情况下，IR 流绕过 Cstar 直接进入发射器。只有标注了 `@pipeline`、
//! `@isr_group`、`@patch`、`@repo` 的函数才激活 Cstar（手册 §6.6）。
//!
//! ## 不破坏语义
//!
//! Cstar 只插入调度指令（`Prefetch`/`PipelineMarker`/`IsrEntry`/`WcetNote`/
//! `DiffMeta`/`Comment`），不删除或改写已有指令。启用 Cstar 后生成的二进制
//! 与未启用时功能等价（仅优化不同）——手册 §6.5 验收标准。

use crate::codegen::ir::{IrFunction, IrModule, Operand, StaticInsn, TypeHint};

/// 循环结构信息（由 [`detect_loops`] 产出）。
#[derive(Debug, Clone)]
pub struct LoopInfo {
    /// 循环头标签。
    pub head_label: String,
    /// 循环退出标签。
    pub exit_label: String,
    /// 循环体在 body 中的指令范围 [start, end)。
    pub body_start: usize,
    pub body_end: usize,
    /// 循环体内是否包含 Load/Store（可 DMA 化候选）。
    pub has_memory_access: bool,
    /// 循环体指令数（用于 WCET 估算）。
    pub insn_count: usize,
}

/// 检测函数体内的所有循环。
///
/// 策略：扫描 `Jump{target}`，若 target 在当前指令之前出现（回跳），则认为
/// 是一个循环。循环头是 target 标签，循环体从头到 Jump 指令。
pub fn detect_loops(body: &[StaticInsn]) -> Vec<LoopInfo> {
    let mut loops = Vec::new();
    // 建立 label → 位置 索引。
    let mut label_pos: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, insn) in body.iter().enumerate() {
        if let StaticInsn::Label(l) = insn {
            label_pos.insert(l.clone(), i);
        }
    }
    // 扫描回跳 Jump。
    for (i, insn) in body.iter().enumerate() {
        if let StaticInsn::Jump { target } = insn {
            if let Some(&head_idx) = label_pos.get(target) {
                if head_idx < i {
                    // 回跳：循环。找退出标签（循环体后第一个 Label）。
                    let mut exit_label = String::new();
                    let mut body_end = i;
                    for j in (i + 1)..body.len() {
                        if let StaticInsn::Label(l) = &body[j] {
                            exit_label = l.clone();
                            body_end = j;
                            break;
                        }
                    }
                    // 统计循环体。
                    let mut has_mem = false;
                    let mut count = 0;
                    for j in head_idx..=i {
                        match &body[j] {
                            StaticInsn::Load { .. } | StaticInsn::Store { .. } => {
                                has_mem = true;
                                count += 1;
                            }
                            StaticInsn::Label(_) | StaticInsn::Comment(_) | StaticInsn::SrcLoc { .. } => {}
                            StaticInsn::Prefetch { .. } | StaticInsn::PipelineMarker { .. }
                            | StaticInsn::IsrEntry { .. } | StaticInsn::WcetNote { .. }
                            | StaticInsn::DiffMeta { .. } => {}
                            _ => count += 1,
                        }
                    }
                    loops.push(LoopInfo {
                        head_label: target.clone(),
                        exit_label,
                        body_start: head_idx,
                        body_end,
                        has_memory_access: has_mem,
                        insn_count: count,
                    });
                }
            }
        }
    }
    loops
}

/// 过滤器配置。
#[derive(Debug, Clone)]
pub struct FilterConfig {
    /// 是否启用 Prefetch 插入（`@pipeline`）。
    pub enable_prefetch: bool,
    /// 是否启用 WCET 计算（`@isr_group`）。
    pub enable_wcet: bool,
    /// 是否启用差分元数据（`@patch`）。
    pub enable_diff: bool,
    /// WCET 预算（指令数）。超限则插入警告。
    pub wcet_budget: u64,
    /// Prefetch 距离（提前多少条指令预取）。
    pub prefetch_distance: usize,
}

impl Default for FilterConfig {
    fn default() -> Self {
        FilterConfig {
            enable_prefetch: true,
            enable_wcet: true,
            enable_diff: true,
            wcet_budget: 1000,
            prefetch_distance: 4,
        }
    }
}

/// 过滤单个函数的 IR 流。
///
/// 根据函数注解决定是否激活各子功能。返回增强后的 IR 流（可能插入了
/// `Prefetch`/`WcetNote`/`DiffMeta`/`Comment` 指令）。
pub fn filter_function(f: &IrFunction, cfg: &FilterConfig) -> Vec<StaticInsn> {
    let has_pipeline = f.annotations.iter().any(|a| a == "pipeline");
    let has_isr = f.annotations.iter().any(|a| a == "isr_group" || a == "isr");
    let has_patch = f.annotations.iter().any(|a| a == "patch");
    let has_repo = f.annotations.iter().any(|a| a == "repo" || a == "package");

    // 若没有任何 Cstar 注解，直接返回原 IR（手册 §6.6：不破坏 Raw）。
    if !has_pipeline && !has_isr && !has_patch && !has_repo {
        return f.body.clone();
    }

    let mut out: Vec<StaticInsn> = Vec::with_capacity(f.body.len() + 16);
    let loops = detect_loops(&f.body);

    // 插入函数入口标记。
    if has_pipeline {
        out.push(StaticInsn::PipelineMarker { name: f.name.clone() });
    }
    if has_isr {
        // 从注解参数提取优先级（简化：默认 0）。
        out.push(StaticInsn::IsrEntry { group: f.name.clone(), priority: 0 });
    }

    // 构建"循环头位置"集合，便于在头之前插入 Prefetch。
    let loop_heads: std::collections::HashMap<usize, &LoopInfo> = loops.iter().map(|l| (l.body_start, l)).collect();

    for (i, insn) in f.body.iter().enumerate() {
        // 在循环头之前插入 Prefetch（若启用且循环有内存访问）。
        if let Some(loop_info) = loop_heads.get(&i) {
            if cfg.enable_prefetch && has_pipeline && loop_info.has_memory_access {
                out.push(StaticInsn::Comment(format!(
                    "cstar: pipeline loop @{} ({} insns, mem=yes)",
                    loop_info.head_label, loop_info.insn_count
                )));
                // 插入 Prefetch：对循环内第一个 Load/Store 的地址预取。
                if let Some(prefetch_addr) = find_first_memory_addr(&f.body[loop_info.body_start..loop_info.body_end + 1]) {
                    out.push(StaticInsn::Prefetch { addr: prefetch_addr, hint: "pldl1keep".into() });
                }
            }
            if cfg.enable_wcet && has_isr {
                let cycles = loop_info.insn_count as u64;
                let over = cycles > cfg.wcet_budget;
                out.push(StaticInsn::WcetNote { cycles, budget: cfg.wcet_budget });
                if over {
                    out.push(StaticInsn::Comment(format!(
                        "cstar: WCET OVERFLOW in loop @{} ({} > {})",
                        loop_info.head_label, cycles, cfg.wcet_budget
                    )));
                }
            }
        }
        // 在 Call 指令处插入差分元数据（供 @patch 定位）。
        if cfg.enable_diff && has_patch {
            if let StaticInsn::Call { func, .. } = insn {
                out.push(StaticInsn::DiffMeta { symbol: func.clone(), offset: i as u64 * 8 });
            }
        }
        out.push(insn.clone());
    }

    // 函数级 WCET 摘要。
    if cfg.enable_wcet && has_isr {
        let total_cycles: usize = loops.iter().map(|l| l.insn_count).sum::<usize>() + f.body.len();
        out.push(StaticInsn::Comment(format!(
            "cstar: function {} total WCET ~= {} cycles ({} loops)",
            f.name, total_cycles, loops.len()
        )));
    }

    out
}

/// 在循环体内找到第一个 Load/Store 的地址操作数。
fn find_first_memory_addr(body: &[StaticInsn]) -> Option<Operand> {
    for insn in body {
        match insn {
            StaticInsn::Load { addr, .. } | StaticInsn::Store { addr, .. } => return Some(addr.clone()),
            _ => {}
        }
    }
    None
}

/// 过滤整个 IR 模块。返回增强后的模块。
pub fn filter_module(m: &IrModule, cfg: &FilterConfig) -> IrModule {
    let mut new_funcs = Vec::with_capacity(m.functions.len());
    for f in &m.functions {
        let filtered_body = filter_function(f, cfg);
        let mut nf = f.clone();
        nf.body = filtered_body;
        new_funcs.push(nf);
    }
    IrModule {
        functions: new_funcs,
        string_pool: m.string_pool.clone(),
        globals: m.globals.clone(),
        entry: m.entry.clone(),
    }
}

/// 生成 Cstar 只读分析报告（不改写 IR）。
///
/// 手册 §9.2 R4 防护：Phase 4 初期 Cstar 只做只读分析，输出报告。等验证
/// 稳定后再启用"改写 IR"模式（即 [`filter_module`]）。
pub fn analyze_module(m: &IrModule) -> String {
    let mut report = String::new();
    report.push_str("=== Cstar IR Analysis Report ===\n\n");
    let mut total_loops = 0;
    let mut dma_candidates = 0;
    let mut wcet_warnings = 0;
    for f in &m.functions {
        let loops = detect_loops(&f.body);
        if loops.is_empty() {
            continue;
        }
        report.push_str(&format!("function {} ({} loops):\n", f.name, loops.len()));
        for l in &loops {
            total_loops += 1;
            let dma = l.has_memory_access;
            if dma {
                dma_candidates += 1;
            }
            report.push_str(&format!(
                "  loop @{} [{}..{}] {} insns, mem={}\n",
                l.head_label, l.body_start, l.body_end, l.insn_count, if dma { "yes (DMA candidate)" } else { "no" }
            ));
            // 启发式 WCET 估算：若循环体 > 100 指令，警告。
            if l.insn_count > 100 {
                wcet_warnings += 1;
                report.push_str(&format!("    ⚠ WCET estimate {} > 100 (consider splitting)\n", l.insn_count));
            }
        }
        report.push('\n');
    }
    report.push_str("=== Summary ===\n");
    report.push_str(&format!("total loops: {}\n", total_loops));
    report.push_str(&format!("DMA candidates: {}\n", dma_candidates));
    report.push_str(&format!("WCET warnings: {}\n", wcet_warnings));
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codegen::ir::{BinOp, Cond, Operand, StaticInsn, TypeHint};

    fn make_loop_body() -> Vec<StaticInsn> {
        // 一个简单循环：
        // .L0_head:
        //   cmp v0, v1
        //   branch Ge, v0, v1, .L0_exit
        //   load v2, [v3]
        //   store [v3], v2
        //   jump .L0_head
        // .L0_exit:
        vec![
            StaticInsn::Label(".L0_head".into()),
            StaticInsn::Cmp { dst: Operand::Reg("v4".into(), TypeHint::Bool), cond: Cond::Ge, lhs: Operand::Reg("v0".into(), TypeHint::I64), rhs: Operand::Reg("v1".into(), TypeHint::I64), ty: TypeHint::I64 },
            StaticInsn::Branch { cond: Cond::Ge, lhs: Operand::Reg("v0".into(), TypeHint::I64), rhs: Operand::Reg("v1".into(), TypeHint::I64), target: ".L0_exit".into() },
            StaticInsn::Load { dst: Operand::Reg("v2".into(), TypeHint::I64), addr: Operand::Reg("v3".into(), TypeHint::Ptr), size: 8, ty: TypeHint::I64 },
            StaticInsn::Store { addr: Operand::Reg("v3".into(), TypeHint::Ptr), value: Operand::Reg("v2".into(), TypeHint::I64), size: 8, ty: TypeHint::I64 },
            StaticInsn::Jump { target: ".L0_head".into() },
            StaticInsn::Label(".L0_exit".into()),
        ]
    }

    #[test]
    fn detect_simple_loop() {
        let body = make_loop_body();
        let loops = detect_loops(&body);
        assert_eq!(loops.len(), 1);
        let l = &loops[0];
        assert_eq!(l.head_label, ".L0_head");
        assert!(l.has_memory_access);
        assert!(l.insn_count > 0);
    }

    #[test]
    fn filter_inserts_prefetch_for_pipeline() {
        let f = IrFunction {
            name: "process".into(),
            params: vec![],
            param_regs: vec![],
            return_ty: TypeHint::Unit,
            body: make_loop_body(),
            is_main: false,
            annotations: vec!["pipeline".into()],
            locals: vec![],
        };
        let cfg = FilterConfig::default();
        let filtered = filter_function(&f, &cfg);
        // 应该插入 PipelineMarker 和 Prefetch。
        let has_marker = filtered.iter().any(|i| matches!(i, StaticInsn::PipelineMarker { .. }));
        let has_prefetch = filtered.iter().any(|i| matches!(i, StaticInsn::Prefetch { .. }));
        assert!(has_marker, "expected PipelineMarker");
        assert!(has_prefetch, "expected Prefetch for DMA-able loop");
    }

    #[test]
    fn filter_inserts_wcet_for_isr() {
        let f = IrFunction {
            name: "handler".into(),
            params: vec![],
            param_regs: vec![],
            return_ty: TypeHint::Unit,
            body: make_loop_body(),
            is_main: false,
            annotations: vec!["isr_group".into()],
            locals: vec![],
        };
        let cfg = FilterConfig::default();
        let filtered = filter_function(&f, &cfg);
        let has_wcet = filtered.iter().any(|i| matches!(i, StaticInsn::WcetNote { .. }));
        assert!(has_wcet, "expected WcetNote for @isr_group");
    }

    #[test]
    fn no_annotation_passes_through() {
        // 没有任何 Cstar 注解的函数，IR 应该原样返回。
        let f = IrFunction {
            name: "plain".into(),
            params: vec![],
            param_regs: vec![],
            return_ty: TypeHint::Unit,
            body: make_loop_body(),
            is_main: false,
            annotations: vec![],
            locals: vec![],
        };
        let cfg = FilterConfig::default();
        let filtered = filter_function(&f, &cfg);
        assert_eq!(filtered.len(), f.body.len());
    }

    #[test]
    fn filter_preserves_semantics() {
        // 启用 Cstar 后，原有指令数量应该 ≥ 原始（只增不减）。
        let f = IrFunction {
            name: "p".into(),
            params: vec![],
            param_regs: vec![],
            return_ty: TypeHint::Unit,
            body: make_loop_body(),
            is_main: false,
            annotations: vec!["pipeline".into(), "isr_group".into(), "patch".into()],
            locals: vec![],
        };
        let cfg = FilterConfig::default();
        let filtered = filter_function(&f, &cfg);
        // 原始 7 条 + 至少 PipelineMarker/IsrEntry/Prefetch/WcetNote/Comment。
        assert!(filtered.len() > f.body.len());
        // 所有原始指令都应该保留。
        let orig_labels: Vec<_> = f.body.iter().filter_map(|i| {
            if let StaticInsn::Label(l) = i { Some(l.clone()) } else { None }
        }).collect();
        for l in orig_labels {
            assert!(filtered.iter().any(|i| matches!(i, StaticInsn::Label(x) if x == &l)), "label {} must be preserved", l);
        }
    }

    #[test]
    fn analyze_report_smoke() {
        let m = IrModule {
            functions: vec![IrFunction {
                name: "f".into(),
                params: vec![],
            param_regs: vec![],
                return_ty: TypeHint::Unit,
                body: make_loop_body(),
                is_main: false,
                annotations: vec![],
                locals: vec![],
            }],
            string_pool: vec![],
            globals: vec![],
            entry: Some("f".into()),
        };
        let report = analyze_module(&m);
        assert!(report.contains("total loops: 1"));
        assert!(report.contains("DMA candidates: 1"));
    }
}
