//! vredrs 1.0 运行时模块
//!
//! 提供 ARC/GC/线性类型内存管理、协程调度、通道通信。

pub mod memory;
pub mod channel;
pub mod coroutine;

use crate::error::Result;

/// 运行时初始化
pub fn init() -> Result<()> {
    Ok(())
}




