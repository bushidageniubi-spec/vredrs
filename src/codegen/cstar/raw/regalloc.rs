//! Linear-scan register allocator for raw backends (x86 + ARM).
//!
//! The previous allocator was a simple sequential allocator: variables
//! were assigned to callee-saved registers in order of first use, and
//! once all registers were exhausted, variables were spilled to the
//! stack. This caused unnecessary spills when variables had
//! non-overlapping lifetimes.
//!
//! This module implements a proper **linear-scan** allocator:
//!
//! 1. **Live interval tracking**: Each variable gets a `[start, end)`
//!    interval representing the range of instructions where it's live.
//! 2. **Register reuse**: When a variable's lifetime ends (it's no
//!    longer used), its register is freed and can be reused for a
//!    new variable.
//! 3. **Spill minimization**: Only spill when ALL registers are
//!    occupied by live variables.
//! 4. **Caller-saved registers**: In addition to callee-saved
//!    registers, the allocator can use caller-saved scratch registers
//!    (x9-x11 on AArch64, r12 on ARM32, rax/rcx/rdx on x86) for
//!    short-lived temporaries, reducing pressure on callee-saved
//!    registers.
//!
//! The allocator is designed to be backend-agnostic: it takes a list
//! of register names and returns assignments.

use std::collections::HashMap;

/// A live interval for a variable: [start_pos, end_pos).
/// `start_pos` is the instruction index where the variable is first
/// defined (assigned), and `end_pos` is the instruction index of its
/// last use.
#[derive(Debug, Clone)]
pub struct LiveInterval {
    pub var_name: String,
    pub start: usize,
    pub end: usize,
    /// Whether this variable is a parameter (parameters are pre-assigned
    /// to argument registers and need to be moved to callee-saved).
    pub is_param: bool,
    /// Whether this variable is a loop variable (tends to be long-lived).
    pub is_loop_var: bool,
}

/// The result of register allocation: variable → location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AllocResult {
    /// Variable is allocated to a register.
    Register(String),
    /// Variable is spilled to a stack slot (offset from frame pointer).
    Spilled(i32),
}

/// The register allocator state.
pub struct RegisterAllocator {
    /// Available registers (callee-saved first, then caller-saved).
    registers: Vec<String>,
    /// Active allocations: register name → (var_name, end_pos).
    /// A register is "active" if it currently holds a live variable.
    active: HashMap<String, (String, usize)>,
    /// Final allocation results: var_name → AllocResult.
    allocations: HashMap<String, AllocResult>,
    /// Next stack offset for spilled variables (negative, grows down).
    next_stack_offset: i32,
    /// Instruction counter (tracks current position for interval tracking).
    current_pos: usize,
}

impl RegisterAllocator {
    /// Create a new allocator with the given register pool.
    /// Callee-saved registers should come first (they survive across
    /// calls), followed by caller-saved scratch registers.
    pub fn new(registers: Vec<String>, initial_stack_offset: i32) -> Self {
        RegisterAllocator {
            registers,
            active: HashMap::new(),
            allocations: HashMap::new(),
            next_stack_offset: initial_stack_offset,
            current_pos: 0,
        }
    }

    /// Advance the instruction counter.
    pub fn advance(&mut self) {
        self.current_pos += 1;
    }

    /// Get the current instruction position.
    pub fn current_pos(&self) -> usize {
        self.current_pos
    }

    /// Expire all intervals that end at or before the current position.
    /// This frees registers whose variables are no longer live.
    pub fn expire_old_intervals(&mut self) {
        let pos = self.current_pos;
        let expired: Vec<String> = self.active.iter()
            .filter(|(_, (_, end))| *end <= pos)
            .map(|(reg, _)| reg.clone())
            .collect();
        for reg in expired {
            self.active.remove(&reg);
        }
    }

    /// Allocate a register for a variable with the given live interval.
    /// Returns the allocation result (Register or Spilled).
    ///
    /// The allocator:
    /// 1. First expires old intervals (frees dead registers).
    /// 2. Tries to find a free register.
    /// 3. If no free register, spills the variable with the furthest
    ///    end position (to maximize the chance of freeing a register
    ///    soon).
    pub fn allocate(&mut self, var_name: &str, start: usize, end: usize) -> AllocResult {
        // Check if already allocated (e.g., parameter pre-assigned).
        if let Some(result) = self.allocations.get(var_name) {
            return result.clone();
        }

        // Expire old intervals to free registers. Use the start
        // position (not current_pos) so that intervals ending before
        // this variable's start are properly expired.
        let expired: Vec<String> = self.active.iter()
            .filter(|(_, (_, e))| *e <= start)
            .map(|(reg, _)| reg.clone())
            .collect();
        for reg in expired {
            self.active.remove(&reg);
        }

        // Try to find a free register.
        // Prefer callee-saved registers (they survive across calls).
        let free_reg = self.registers.iter()
            .find(|reg| !self.active.contains_key(*reg))
            .cloned();

        if let Some(reg) = free_reg {
            // Found a free register — allocate it.
            self.active.insert(reg.clone(), (var_name.to_string(), end));
            let result = AllocResult::Register(reg.clone());
            self.allocations.insert(var_name.to_string(), result.clone());
            result
        } else {
            // No free register — need to spill.
            // Strategy: spill the active variable with the furthest end
            // position if it's further than ours (it'll live longer,
            // so freeing its register gives us more time). Otherwise,
            // spill ourselves.
            let furthest = self.active.iter()
                .max_by_key(|(_, (_, end))| *end)
                .map(|(reg, (vname, end))| (reg.clone(), vname.clone(), *end));

            if let Some((reg, spill_var, spill_end)) = furthest {
                // Spill strategy: keep the longer-lived variable in
                // the register, spill the shorter-lived one.
                //
                // If the existing variable (spill_var) lives longer
                // than us (spill_end > end), we spill OURSELVES.
                // If we live longer, we spill the existing variable
                // and take its register.
                if spill_end > end {
                    // Existing variable lives longer — spill ourselves.
                    let offset = self.next_stack_offset;
                    self.next_stack_offset -= 8;
                    let result = AllocResult::Spilled(offset);
                    self.allocations.insert(var_name.to_string(), result.clone());
                    result
                } else {
                    // We live longer — spill the existing variable,
                    // take its register.
                    let spill_offset = self.next_stack_offset;
                    self.next_stack_offset -= 8;
                    self.allocations.insert(spill_var.clone(), AllocResult::Spilled(spill_offset));
                    self.active.remove(&reg);
                    self.active.insert(reg.clone(), (var_name.to_string(), end));
                    let result = AllocResult::Register(reg);
                    self.allocations.insert(var_name.to_string(), result.clone());
                    result
                }
            } else {
                // Shouldn't happen (no active variables but no free registers).
                let offset = self.next_stack_offset;
                self.next_stack_offset -= 8;
                let result = AllocResult::Spilled(offset);
                self.allocations.insert(var_name.to_string(), result.clone());
                result
            }
        }
    }

    /// Get the allocation for a variable (if already allocated).
    pub fn get_allocation(&self, var_name: &str) -> Option<&AllocResult> {
        self.allocations.get(var_name)
    }

    /// Mark a variable as "last used" at the current position.
    /// This sets its interval end to the current position, allowing
    /// its register to be freed sooner.
    pub fn mark_last_use(&mut self, var_name: &str) {
        let pos = self.current_pos + 1; // End is exclusive.
        // Update the end position in the active map.
        for (_, (vname, end)) in self.active.iter_mut() {
            if vname == var_name {
                *end = pos;
                break;
            }
        }
    }

    /// Get the number of registers currently in use.
    pub fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Get the total stack space used for spills.
    pub fn stack_used(&self) -> i32 {
        -self.next_stack_offset
    }

    /// Get all allocated register names (for prologue/epilogue generation).
    pub fn used_registers(&self) -> Vec<String> {
        self.allocations.values()
            .filter_map(|r| match r {
                AllocResult::Register(reg) => Some(reg.clone()),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    }

    /// Reset the allocator for a new function.
    pub fn reset(&mut self, initial_stack_offset: i32) {
        self.active.clear();
        self.allocations.clear();
        self.next_stack_offset = initial_stack_offset;
        self.current_pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_allocation() {
        let mut alloc = RegisterAllocator::new(
            vec!["r1".to_string(), "r2".to_string(), "r3".to_string()],
            0,
        );
        // Variable A: live from 0 to 5
        let r = alloc.allocate("A", 0, 5);
        assert!(matches!(r, AllocResult::Register(_)));
        // Variable B: live from 1 to 3
        let r = alloc.allocate("B", 1, 3);
        assert!(matches!(r, AllocResult::Register(_)));
        // Variable C: live from 2 to 4
        let r = alloc.allocate("C", 2, 4);
        assert!(matches!(r, AllocResult::Register(_)));
    }

    #[test]
    fn test_register_reuse() {
        let mut alloc = RegisterAllocator::new(
            vec!["r1".to_string(), "r2".to_string()],
            0,
        );
        // A: live 0-2
        alloc.allocate("A", 0, 2);
        assert_eq!(alloc.active_count(), 1);
        // B: live 1-3
        alloc.allocate("B", 1, 3);
        assert_eq!(alloc.active_count(), 2);
        // C: live 3-5 — A's end (2) <= C's start (3), so A is expired.
        let r = alloc.allocate("C", 3, 5);
        assert!(matches!(r, AllocResult::Register(_)));
        // B is still active (end 3 > start 3? No, 3 <= 3 is true, so B is also expired)
        // Actually: B's end is 3, C's start is 3, 3 <= 3 is true, so B is expired too.
        // So only C should be active.
        assert_eq!(alloc.active_count(), 1); // Only C
    }

    #[test]
    fn test_spill_when_full() {
        let mut alloc = RegisterAllocator::new(
            vec!["r1".to_string()], // Only 1 register
            0,
        );
        // A: live 0-10
        alloc.allocate("A", 0, 10);
        // B: live 1-5 — A lives longer (end 10 > 5), so B should be spilled
        let r = alloc.allocate("B", 1, 5);
        assert!(matches!(r, AllocResult::Spilled(_)));
    }

    #[test]
    fn test_spill_furthest() {
        let mut alloc = RegisterAllocator::new(
            vec!["r1".to_string()], // Only 1 register
            0,
        );
        // A: live 0-10 (long-lived)
        alloc.allocate("A", 0, 10);
        // B: live 1-20 (even longer-lived)
        // A's end (10) < B's end (20), so A should be spilled and B gets the register
        let r = alloc.allocate("B", 1, 20);
        assert!(matches!(r, AllocResult::Register(_)));
        // A should now be spilled
        let a_alloc = alloc.get_allocation("A").unwrap();
        assert!(matches!(a_alloc, AllocResult::Spilled(_)));
    }

    #[test]
    fn test_no_spill_for_non_overlapping() {
        let mut alloc = RegisterAllocator::new(
            vec!["r1".to_string()], // Only 1 register
            0,
        );
        // A: live 0-3
        alloc.allocate("A", 0, 3);
        // Advance past A
        alloc.current_pos = 3;
        alloc.expire_old_intervals();
        // B: live 4-6 — should reuse r1 (A expired)
        let r = alloc.allocate("B", 4, 6);
        assert!(matches!(r, AllocResult::Register(_)));
    }

    #[test]
    fn test_used_registers() {
        let mut alloc = RegisterAllocator::new(
            vec!["r1".to_string(), "r2".to_string(), "r3".to_string()],
            0,
        );
        alloc.allocate("A", 0, 5);
        alloc.allocate("B", 1, 3);
        alloc.allocate("C", 2, 4);
        let used = alloc.used_registers();
        assert_eq!(used.len(), 3);
    }
}
