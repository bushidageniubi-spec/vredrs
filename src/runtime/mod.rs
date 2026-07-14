//! vredrs 1.0 运行时模块
//!
//! 提供 ARC/GC/线性类型内存管理、协程调度、通道通信。

// runtime/memory.rs was removed — it was dead code (never integrated).
// Object lifecycle is managed by Rc<RefCell<...>> in the VM and by
// the C runtime's reference counting in the LLVM backend.
pub mod channel;
pub mod coroutine;

use crate::error::Result;

/// 运行时初始化
pub fn init() -> Result<()> {
    Ok(())
}




