//! Cstar raw firmware generation.
//!
//! This module provides the infrastructure for `vredrs build --raw`:
//! - The default startup code (`startup.s`)
//! - The default linker script (`link.ld`)
//! - A native binary emitter (`emitter.rs`) that generates .bin files
//!   without needing an external ARM toolchain
//!
//! In 0.1.1, the raw backend generates a minimal ARM Cortex-M3 firmware
//! binary containing an IVT + reset handler that calls a stub main(). The
//! binary can be loaded in QEMU with:
//!   qemu-system-arm -M lm3s6965evb -kernel firmware.bin -nographic

pub mod emitter;
pub mod x86;
pub mod arm;
pub mod regalloc;
pub mod debuginfo;

pub use emitter::{build_firmware, build_firmware_with_program, verify_firmware};
pub use x86::{compile_to_x86_assembly, X86CodeGen};
pub use arm::{compile_to_arm_assembly, ArmCodeGen};

use std::path::Path;

/// The default startup assembly source, embedded as a string constant.
/// This is written to the build directory as `startup.s`.
pub const STARTUP_ASM: &str = include_str!("startup.s");

/// The default linker script, embedded as a string constant.
/// This is written to the build directory as `link.ld`.
pub const LINKER_SCRIPT: &str = include_str!("link.ld");

/// Configuration for a raw firmware build.
pub struct RawBuildConfig {
    /// Target triple (e.g. "armv7m-none-eabi").
    pub target: String,
    /// Output format: "bin" or "hex".
    pub format: String,
    /// Whether to include the default startup code.
    pub include_startup: bool,
    /// Whether to include the default linker script.
    pub include_linker: bool,
}

impl Default for RawBuildConfig {
    fn default() -> Self {
        RawBuildConfig {
            target: "armv7m-none-eabi".to_string(),
            format: "bin".to_string(),
            include_startup: true,
            include_linker: true,
        }
    }
}

/// Write the raw firmware support files to a build directory.
///
/// Returns the paths of the written files.
pub fn write_support_files(
    build_dir: &Path,
    config: &RawBuildConfig,
) -> std::io::Result<(std::path::PathBuf, std::path::PathBuf)> {
    let startup_path = build_dir.join("startup.s");
    let linker_path = build_dir.join("link.ld");
    if config.include_startup {
        std::fs::write(&startup_path, STARTUP_ASM)?;
    }
    if config.include_linker {
        std::fs::write(&linker_path, LINKER_SCRIPT)?;
    }
    Ok((startup_path, linker_path))
}

/// Generate the build commands for producing a .bin firmware from
/// the compiled object file. The caller is responsible for executing
/// these commands (e.g. via `std::process::Command`).
pub fn build_commands(obj_path: &Path, output_bin: &Path, build_dir: &Path) -> Vec<String> {
    let linker = build_dir.join("link.ld");
    let startup = build_dir.join("startup.o");
    vec![
        format!(
            "arm-none-eabi-as -mcpu=cortex-m3 -mthumb -o {} {}",
            startup.display(),
            build_dir.join("startup.s").display()
        ),
        format!(
            "arm-none-eabi-ld -T {} -o {}.elf {} {}",
            linker.display(),
            output_bin.with_extension("").display(),
            startup.display(),
            obj_path.display()
        ),
        format!(
            "arm-none-eabi-objcopy -O binary {}.elf {}",
            output_bin.with_extension("").display(),
            output_bin.display()
        ),
    ]
}
