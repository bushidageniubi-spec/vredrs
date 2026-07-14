//! Platform detection and environment-specific utilities.
//!
//! Detects Termux (Android), WSL, macOS, and other environments to
//! provide tailored error messages, linker flags, and toolchain hints.

use std::path::Path;

/// The detected platform environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Platform {
    /// Standard Linux (desktop/server).
    Linux,
    /// Termux on Android (PREFIX=/data/data/com.termux/files/usr).
    Termux,
    /// Windows Subsystem for Linux.
    Wsl,
    /// macOS (Darwin).
    Macos,
    /// Windows native.
    Windows,
    /// FreeBSD or other Unix.
    Other(String),
}

/// Detected platform information.
#[derive(Debug, Clone)]
pub struct PlatformInfo {
    pub platform: Platform,
    pub arch: &'static str,
    /// True if running on an ARM-based device (aarch64 or armv7l).
    pub is_arm: bool,
    /// The prefix path for the environment (Termux: /data/data/com.termux/files/usr).
    pub prefix: Option<String>,
    /// The package manager command (Termux: "pkg", Linux: "apt"/"dnf"/"pacman").
    pub package_manager: Option<String>,
}

impl PlatformInfo {
    /// Detect the current platform by examining environment variables,
    /// file paths, and std::env::consts.
    pub fn detect() -> Self {
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x86_64",
            "aarch64" => "aarch64",
            "arm" | "armv7l" => "arm",
            other => other,
        };
        let is_arm = arch == "aarch64" || arch == "arm";

        // Check for Termux: the PREFIX env var is set to
        // /data/data/com.termux/files/usr
        let prefix = std::env::var("PREFIX").ok();
        let is_termux = prefix
            .as_ref()
            .map(|p| p.contains("com.termux"))
            .unwrap_or(false)
            || Path::new("/data/data/com.termux").exists()
            || std::env::var("TERMUX_VERSION").is_ok();

        // Check for WSL: /proc/version contains "microsoft" or WSL env var.
        let is_wsl = std::env::var("WSL_DISTRO_NAME").is_ok()
            || std::fs::read_to_string("/proc/version")
                .map(|v| v.to_lowercase().contains("microsoft"))
                .unwrap_or(false);

        let platform = if is_termux {
            Platform::Termux
        } else if is_wsl {
            Platform::Wsl
        } else {
            match std::env::consts::OS {
                "linux" => Platform::Linux,
                "macos" => Platform::Macos,
                "windows" => Platform::Windows,
                other => Platform::Other(other.to_string()),
            }
        };

        let package_manager = match &platform {
            Platform::Termux => Some("pkg".to_string()),
            Platform::Linux => {
                // Detect the distro's package manager.
                if Path::new("/usr/bin/apt").exists() || Path::new("/usr/bin/apt-get").exists() {
                    Some("apt".to_string())
                } else if Path::new("/usr/bin/dnf").exists() {
                    Some("dnf".to_string())
                } else if Path::new("/usr/bin/yum").exists() {
                    Some("yum".to_string())
                } else if Path::new("/usr/bin/pacman").exists() {
                    Some("pacman".to_string())
                } else if Path::new("/usr/bin/apk").exists() {
                    Some("apk".to_string())
                } else if Path::new("/usr/bin/zypper").exists() {
                    Some("zypper".to_string())
                } else {
                    None
                }
            }
            Platform::Macos => Some("brew".to_string()),
            Platform::Wsl => {
                // WSL usually has apt (Ubuntu).
                if Path::new("/usr/bin/apt").exists() {
                    Some("apt".to_string())
                } else {
                    None
                }
            }
            _ => None,
        };

        PlatformInfo {
            platform,
            arch,
            is_arm,
            prefix,
            package_manager,
        }
    }

    /// Returns true if running on Termux (Android).
    pub fn is_termux(&self) -> bool {
        self.platform == Platform::Termux
    }

    /// Returns true if running on an ARM device.
    pub fn is_arm_device(&self) -> bool {
        self.is_arm
    }

    /// Get the C compiler command to use for linking.
    /// On Termux, `cc` may not be available; use `clang` directly.
    pub fn cc_command(&self) -> &'static str {
        match self.platform {
            Platform::Termux => "clang", // Termux ships clang, not gcc
            _ => "cc",
        }
    }

    /// Get the assembler command to use.
    /// On Termux/ARM, use `as` (GNU assembler from binutils).
    pub fn as_command(&self) -> &'static str {
        "as"
    }

    /// Get the linker command to use.
    /// On Termux, use `cc` (clang) for linking since `ld` may not have the
    /// right paths configured.
    pub fn ld_command(&self) -> &'static str {
        match self.platform {
            Platform::Termux => "clang",
            _ => "ld",
        }
    }

    /// Get extra linker flags for this platform.
    /// On Termux, we need `-lm` and the Termusr lib path.
    pub fn extra_link_flags(&self) -> Vec<&'static str> {
        let mut flags = vec!["-lm"];
        match self.platform {
            Platform::Termux => {
                // Termux needs -lpthread and sometimes -landroid
                flags.push("-lpthread");
            }
            _ => {
                flags.push("-lpthread");
            }
        }
        flags
    }

    /// Get a human-readable platform name for display.
    pub fn display_name(&self) -> String {
        match &self.platform {
            Platform::Linux => format!("Linux ({})", self.arch),
            Platform::Termux => format!("Termux/Android ({})", self.arch),
            Platform::Wsl => format!("WSL ({})", self.arch),
            Platform::Macos => format!("macOS ({})", self.arch),
            Platform::Windows => format!("Windows ({})", self.arch),
            Platform::Other(name) => format!("{} ({})", name, self.arch),
        }
    }

    /// Get a hint for installing a missing toolchain package.
    /// Returns a string like "pkg install clang" (Termux) or
    /// "apt install clang" (Linux).
    pub fn install_hint(&self, package: &str) -> String {
        match &self.platform {
            Platform::Termux => format!("pkg install {}", package),
            Platform::Macos => format!("brew install {}", package),
            Platform::Linux | Platform::Wsl => {
                if let Some(ref pm) = self.package_manager {
                    match pm.as_str() {
                        "apt" => format!("sudo apt install {}", package),
                        "dnf" => format!("sudo dnf install {}", package),
                        "yum" => format!("sudo yum install {}", package),
                        "pacman" => format!("sudo pacman -S {}", package),
                        "apk" => format!("apk add {}", package),
                        "zypper" => format!("sudo zypper install {}", package),
                        _ => format!("{} install {}", pm, package),
                    }
                } else {
                    format!("install {}", package)
                }
            }
            _ => format!("install {}", package),
        }
    }

    /// Get Termux-specific advice for a missing tool.
    pub fn missing_tool_hint(&self, tool: &str) -> Option<String> {
        if self.is_termux() {
            let pkg = match tool {
                "clang" | "cc" | "gcc" => "clang",
                "as" | "ld" | "assembler" | "linker" => "binutils",
                "make" => "make",
                "cmake" => "cmake",
                _ => return None,
            };
            Some(format!(
                "On Termux, install {} with: pkg install {}",
                tool, pkg
            ))
        } else {
            None
        }
    }
}

// ============================================================================
// ANSI Color Utilities
// ============================================================================

/// ANSI color codes for terminal output.
/// All output respects the NO_COLOR environment variable.
pub struct Colors;

impl Colors {
    /// Check if colors should be used.
    /// Colors are disabled when:
    /// 1. The NO_COLOR environment variable is set (any value).
    /// 2. stdout/stderr is not a TTY (piped/redirected output).
    pub fn enabled() -> bool {
        if std::env::var("NO_COLOR").is_ok() {
            return false;
        }
        // Check if stderr is a TTY (vredrs writes diagnostic output to stderr).
        Self::is_tty()
    }

    /// Check if stderr is a TTY.
    fn is_tty() -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;
            let fd = std::io::stderr().as_raw_fd();
            // isatty(fd) — use libc::isatty if available, otherwise
            // fall back to checking /proc/self/fd/2.
            extern "C" {
                fn isatty(fd: i32) -> i32;
            }
            unsafe { isatty(fd) != 0 }
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Bold red — for error headers and critical messages.
    pub fn red_bold() -> &'static str {
        if Self::enabled() { "\x1b[1;31m" } else { "" }
    }

    /// Red — for error text.
    pub fn red() -> &'static str {
        if Self::enabled() { "\x1b[31m" } else { "" }
    }

    /// Green — for success messages.
    pub fn green() -> &'static str {
        if Self::enabled() { "\x1b[32m" } else { "" }
    }

    /// Green bold — for success headers.
    pub fn green_bold() -> &'static str {
        if Self::enabled() { "\x1b[1;32m" } else { "" }
    }

    /// Yellow — for warnings.
    pub fn yellow() -> &'static str {
        if Self::enabled() { "\x1b[33m" } else { "" }
    }

    /// Yellow bold — for warning headers.
    pub fn yellow_bold() -> &'static str {
        if Self::enabled() { "\x1b[1;33m" } else { "" }
    }

    /// Blue — for informational messages.
    pub fn blue() -> &'static str {
        if Self::enabled() { "\x1b[34m" } else { "" }
    }

    /// Blue bold — for the [vredrs] prefix.
    pub fn blue_bold() -> &'static str {
        if Self::enabled() { "\x1b[1;34m" } else { "" }
    }

    /// Magenta — for the REPL prompt and quotes.
    pub fn magenta() -> &'static str {
        if Self::enabled() { "\x1b[35m" } else { "" }
    }

    /// Magenta bold.
    pub fn magenta_bold() -> &'static str {
        if Self::enabled() { "\x1b[1;35m" } else { "" }
    }

    /// Cyan — for file paths and line numbers.
    pub fn cyan() -> &'static str {
        if Self::enabled() { "\x1b[36m" } else { "" }
    }

    /// Cyan bold.
    pub fn cyan_bold() -> &'static str {
        if Self::enabled() { "\x1b[1;36m" } else { "" }
    }

    /// White — for source code display.
    pub fn white() -> &'static str {
        if Self::enabled() { "\x1b[37m" } else { "" }
    }

    /// White bold.
    pub fn white_bold() -> &'static str {
        if Self::enabled() { "\x1b[1;37m" } else { "" }
    }

    /// Dim/gray — for secondary information.
    pub fn dim() -> &'static str {
        if Self::enabled() { "\x1b[2m" } else { "" }
    }

    /// Reset all formatting.
    pub fn reset() -> &'static str {
        if Self::enabled() { "\x1b[0m" } else { "" }
    }

    /// Colorize a string with the given color code.
    pub fn paint(text: &str, color: &str) -> String {
        if Self::enabled() {
            format!("{}{}{}", color, text, Self::reset())
        } else {
            text.to_string()
        }
    }
}

// ============================================================================
// Console Output Helpers
// ============================================================================

/// Print a colored [vredrs] info message to stderr.
pub fn info(msg: &str) {
    eprintln!(
        "{}[vredrs]{} {}",
        Colors::blue_bold(),
        Colors::reset(),
        msg
    );
}

/// Print a colored [vredrs] success message to stderr.
pub fn success(msg: &str) {
    eprintln!(
        "{}[vredrs]{} {}{}{}",
        Colors::blue_bold(),
        Colors::reset(),
        Colors::green(),
        msg,
        Colors::reset()
    );
}

/// Print a colored [vredrs] warning message to stderr.
pub fn warning(msg: &str) {
    eprintln!(
        "{}[vredrs]{} {}warning:{} {}",
        Colors::blue_bold(),
        Colors::reset(),
        Colors::yellow_bold(),
        Colors::reset(),
        msg
    );
}

/// Print a colored [vredrs] error message to stderr.
pub fn error(msg: &str) {
    eprintln!(
        "{}[vredrs]{} {}error:{} {}",
        Colors::blue_bold(),
        Colors::reset(),
        Colors::red_bold(),
        Colors::reset(),
        msg
    );
}

/// Print a platform-specific hint (e.g., "On Termux, install clang with: pkg install clang").
pub fn hint(msg: &str) {
    eprintln!(
        "{}[vredrs]{} {}hint:{} {}",
        Colors::blue_bold(),
        Colors::reset(),
        Colors::cyan_bold(),
        Colors::reset(),
        msg
    );
}

/// Print a platform info banner (used at startup for --raw builds).
pub fn print_platform_info(info: &PlatformInfo) {
    eprintln!(
        "{}[vredrs]{} platform: {} {}{}{}",
        Colors::blue_bold(),
        Colors::reset(),
        Colors::cyan(),
        info.display_name(),
        if info.is_termux() { " (Termux)" } else { "" },
        Colors::reset()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_platform_detect() {
        let info = PlatformInfo::detect();
        // Should always return something.
        assert!(!info.display_name().is_empty());
    }

    #[test]
    fn test_install_hint() {
        let info = PlatformInfo::detect();
        let hint = info.install_hint("clang");
        assert!(!hint.is_empty());
    }

    #[test]
    fn test_colors_disabled_with_no_color() {
        std::env::set_var("NO_COLOR", "1");
        assert!(!Colors::enabled());
        std::env::remove_var("NO_COLOR");
    }

    #[test]
    fn test_paint() {
        // When colors are disabled (non-TTY in test), paint returns plain text.
        let result = Colors::paint("hello", Colors::red());
        assert!(!result.is_empty());
    }
}
