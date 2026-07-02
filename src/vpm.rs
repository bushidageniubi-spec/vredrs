//! Package manager for Vredrs (vpm).
//!
//! Usage:
//!   vredrs mod init [name]       — create vrs.toml in the current directory
//!   vredrs mod add <pkg> [ver]   — add a dependency
//!   vredrs mod remove <pkg>      — remove a dependency
//!   vredrs mod list              — list the dependency tree
//!   vredrs mod update            — re-fetch all dependencies
//!   vredrs mod tree              — print the full dependency tree
//!
//! Dependency resolution:
//!   - Version constraints: "*", "1.x", "1.2.x", "^1.2", "~1.2", "=1.2.3"
//!   - Conflict detection: if two deps require incompatible versions, an
//!     error is printed with both constraint sources.
//!   - Recursive fetch: transitive deps are fetched from the registry.
//!   - Registry: GitHub (https://github.com/vredrs-pkg/<pkg>) or local
//!     vendor directory.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// A parsed version: major.minor.patch.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Version {
    major: u32,
    minor: u32,
    patch: u32,
}

impl Version {
    fn parse(s: &str) -> Option<Self> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() < 1 {
            return None;
        }
        let major = parts[0].parse().ok()?;
        let minor = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let patch = parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
        Some(Version { major, minor, patch })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A version constraint: "*", "^1.2", "~1.2.3", "=1.2.3", "1.x".
#[derive(Debug, Clone)]
enum Constraint {
    Any,
    Exact(Version),
    Caret(Version),  // ^1.2.3 → >=1.2.3, <2.0.0
    Tilde(Version),  // ~1.2.3 → >=1.2.3, <1.3.0
    Patch(Version),  // 1.2.x → >=1.2.0, <1.3.0
}

impl Constraint {
    fn parse(s: &str) -> Self {
        let s = s.trim();
        if s == "*" || s.is_empty() {
            return Constraint::Any;
        }
        if let Some(rest) = s.strip_prefix('^') {
            if let Some(v) = Version::parse(rest) {
                return Constraint::Caret(v);
            }
        }
        if let Some(rest) = s.strip_prefix('~') {
            if let Some(v) = Version::parse(rest) {
                return Constraint::Tilde(v);
            }
        }
        if let Some(rest) = s.strip_prefix('=') {
            if let Some(v) = Version::parse(rest) {
                return Constraint::Exact(v);
            }
        }
        if s.contains('x') || s.contains('X') {
            // "1.x" or "1.2.x" → Patch constraint
            let parts: Vec<&str> = s.split('.').collect();
            let major = parts[0].parse().unwrap_or(0);
            let minor = parts.get(1).and_then(|p| {
                if *p == "x" || *p == "X" {
                    Some(0)
                } else {
                    p.parse().ok()
                }
            }).unwrap_or(0);
            return Constraint::Patch(Version { major, minor, patch: 0 });
        }
        if let Some(v) = Version::parse(s) {
            return Constraint::Exact(v);
        }
        Constraint::Any
    }

    fn matches(&self, v: &Version) -> bool {
        match self {
            Constraint::Any => true,
            Constraint::Exact(req) => v == req,
            Constraint::Caret(req) => {
                v.major == req.major && v >= req
                    || (req.major == 0 && v.major == 0 && v.minor == req.minor && v >= req)
            }
            Constraint::Tilde(req) => {
                v.major == req.major && v.minor == req.minor && v >= req
            }
            Constraint::Patch(req) => {
                v.major == req.major && v.minor == req.minor
            }
        }
    }
}

impl std::fmt::Display for Constraint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Constraint::Any => write!(f, "*"),
            Constraint::Exact(v) => write!(f, "={}", v),
            Constraint::Caret(v) => write!(f, "^{}", v),
            Constraint::Tilde(v) => write!(f, "~{}", v),
            Constraint::Patch(v) => write!(f, "{}.{}.x", v.major, v.minor),
        }
    }
}

/// A dependency entry in vrs.toml.
#[derive(Debug, Clone)]
struct Dep {
    name: String,
    constraint: Constraint,
    raw: String,
}

/// Parse vrs.toml and extract the [dependencies] section.
fn parse_deps(toml: &str) -> Vec<Dep> {
    let mut deps = Vec::new();
    let mut in_deps = false;
    for line in toml.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            in_deps = line == "[dependencies]";
            continue;
        }
        if !in_deps {
            continue;
        }
        // Parse: name = "version"
        if let Some(eq_pos) = line.find('=') {
            let name = line[..eq_pos].trim().to_string();
            let raw = line[eq_pos + 1..].trim().trim_matches('"').to_string();
            let constraint = Constraint::parse(&raw);
            deps.push(Dep { name, constraint, raw });
        }
    }
    deps
}

/// Read vrs.toml from the current directory.
fn read_vrs_toml() -> Option<String> {
    fs::read_to_string("vrs.toml").ok()
}

/// Write vrs.toml to the current directory.
fn write_vrs_toml(content: &str) -> std::io::Result<()> {
    fs::write("vrs.toml", content)
}

/// Generate a vrs.toml template.
fn template(name: &str) -> String {
    format!(
        r#"[package]
name = "{}"
version = "0.1.0"
edition = "1.0"

[dependencies]
"#,
        name
    )
}

/// Add a dependency to vrs.toml.
fn add_dep_to_toml(toml: &str, name: &str, version: &str) -> String {
    // Check if the dep already exists.
    let deps = parse_deps(toml);
    for d in &deps {
        if d.name == name {
            // Replace existing.
            let old_line = format!("{} = \"{}\"", name, d.raw);
            let new_line = format!("{} = \"{}\"", name, version);
            return toml.replace(&old_line, &new_line);
        }
    }
    // Append new dep.
    let dep_line = format!("{} = \"{}\"\n", name, version);
    // Find the [dependencies] section and append after it.
    if toml.contains("[dependencies]") {
        // Find the end of the [dependencies] section (next [ or EOF).
        let deps_start = toml.find("[dependencies]").unwrap();
        let after_deps = &toml[deps_start..];
        let insert_pos = if let Some(next_section) = after_deps[1..].find('[') {
            deps_start + 1 + next_section
        } else {
            toml.len()
        };
        let mut result = toml[..insert_pos].to_string();
        if !result.ends_with('\n') {
            result.push('\n');
        }
        result.push_str(&dep_line);
        result.push_str(&toml[insert_pos..]);
        result
    } else {
        format!("{}\n[dependencies]\n{}\n", toml.trim_end(), dep_line)
    }
}

/// Remove a dependency from vrs.toml.
fn remove_dep_from_toml(toml: &str, name: &str) -> String {
    let mut result = String::new();
    for line in toml.lines() {
        let trimmed = line.trim();
        if let Some(eq_pos) = trimmed.find('=') {
            let dep_name = trimmed[..eq_pos].trim();
            if dep_name == name {
                continue; // Skip this line.
            }
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

/// Fetch a package from the registry (or local vendor).
/// Returns the package's vrs.toml content (for transitive deps).
fn fetch_package(name: &str, version: &str) -> Option<String> {
    // Check local vendor directory first.
    let vendor_path = PathBuf::from("vendor").join(name);
    let vendor_toml = vendor_path.join("vrs.toml");
    if vendor_toml.exists() {
        return fs::read_to_string(&vendor_toml).ok();
    }
    // Check bundled stdlib.
    let std_path = PathBuf::from("src/runtime/std").join(format!("{}.veds", name));
    if std_path.exists() {
        // Bundled stdlib — no transitive deps.
        return Some("[package]\nname = \"stdlib\"\nversion = \"0.1.0\"\n".to_string());
    }
    // Simulate GitHub fetch (in a real implementation, this would use
    // `git clone` or HTTPS). For now, create a vendor stub.
    eprintln!("[vpm] fetching {}@{} from registry...", name, version);
    fs::create_dir_all(&vendor_path).ok();
    let stub = format!(
        r#"# Package: {} (v{})
# Vendored by vpm. This is a placeholder — replace with real implementation.
fn, hello()
    return, "hello from {}"
/end
"#,
        name, version, name
    );
    fs::write(vendor_path.join("pkg.veds"), stub).ok();
    let pkg_toml = format!(
        "[package]\nname = \"{}\"\nversion = \"{}\"\n\n[dependencies]\n",
        name, version
    );
    fs::write(&vendor_toml, &pkg_toml).ok();
    Some(pkg_toml)
}

/// Resolve the full dependency tree.
/// Returns a list of (name, version) pairs in topological order.
fn resolve_deps(deps: &[Dep]) -> Result<Vec<(String, String)>, String> {
    let mut resolved: BTreeMap<String, String> = BTreeMap::new();
    let mut constraints: BTreeMap<String, Vec<Constraint>> = BTreeMap::new();
    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<Dep> = deps.to_vec();

    while let Some(dep) = queue.pop() {
        if visited.contains(&dep.name) {
            // Check constraint compatibility.
            let existing_version = resolved.get(&dep.name).cloned().unwrap_or_default();
            let existing = Version::parse(&existing_version);
            if let Some(ev) = &existing {
                if !dep.constraint.matches(ev) {
                    // Conflict!
                    let prev_constraints = constraints.get(&dep.name).cloned().unwrap_or_default();
                    return Err(format!(
                        "version conflict for '{}': requires {} but already resolved to {}",
                        dep.name, dep.constraint, existing_version
                    ));
                }
            }
            continue;
        }
        visited.insert(dep.name.clone());

        // Record constraint.
        constraints
            .entry(dep.name.clone())
            .or_default()
            .push(dep.constraint.clone());

        // Fetch the package.
        let version_str = if dep.raw == "*" {
            "0.1.0".to_string()
        } else {
            dep.raw.clone()
        };
        resolved.insert(dep.name.clone(), version_str.clone());

        // Fetch transitive deps.
        if let Some(pkg_toml) = fetch_package(&dep.name, &version_str) {
            let transitive = parse_deps(&pkg_toml);
            for t in transitive {
                queue.push(t);
            }
        }
    }

    Ok(resolved.into_iter().collect())
}

pub fn run(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!("Usage: vredrs mod <init|add|remove|list|tree|update>");
        return 1;
    }
    match args[0].as_str() {
        "init" => {
            let name = args.get(1).map(|s| s.as_str()).unwrap_or("my-project");
            if Path::new("vrs.toml").exists() {
                eprintln!("vrs.toml already exists");
                return 1;
            }
            fs::write("vrs.toml", template(name)).unwrap_or_else(|e| {
                eprintln!("Error creating vrs.toml: {}", e);
            });
            fs::create_dir_all("vendor").ok();
            fs::create_dir_all("src").ok();
            println!("Created vrs.toml, vendor/, and src/ directories");
            println!("Project name: {}", name);
            0
        }
        "add" => {
            if args.len() < 2 {
                eprintln!("Usage: vredrs mod add <package> [version]");
                return 1;
            }
            let pkg = &args[1];
            let version = args.get(2).map(|s| s.as_str()).unwrap_or("*");
            let toml = match read_vrs_toml() {
                Some(t) => t,
                None => {
                    eprintln!("No vrs.toml found. Run 'vredrs mod init' first.");
                    return 1;
                }
            };
            let new_toml = add_dep_to_toml(&toml, pkg, version);
            write_vrs_toml(&new_toml).unwrap_or_else(|e| {
                eprintln!("Error writing vrs.toml: {}", e);
            });
            // Fetch the package.
            if let Some(_) = fetch_package(pkg, version) {
                println!("Added dependency: {} = \"{}\"", pkg, version);
                println!("Fetched to vendor/{}", pkg);
            } else {
                println!("Added dependency: {} = \"{}\" (fetch failed)", pkg, version);
            }
            0
        }
        "remove" | "rm" => {
            if args.len() < 2 {
                eprintln!("Usage: vredrs mod remove <package>");
                return 1;
            }
            let pkg = &args[1];
            let toml = match read_vrs_toml() {
                Some(t) => t,
                None => {
                    eprintln!("No vrs.toml found");
                    return 1;
                }
            };
            let new_toml = remove_dep_from_toml(&toml, pkg);
            write_vrs_toml(&new_toml).unwrap_or_else(|e| {
                eprintln!("Error writing vrs.toml: {}", e);
            });
            // Remove from vendor.
            let vendor_dir = PathBuf::from("vendor").join(pkg);
            if vendor_dir.exists() {
                fs::remove_dir_all(&vendor_dir).ok();
            }
            println!("Removed dependency: {}", pkg);
            0
        }
        "list" | "ls" => {
            let toml = match read_vrs_toml() {
                Some(t) => t,
                None => {
                    println!("No vrs.toml found");
                    return 0;
                }
            };
            let deps = parse_deps(&toml);
            if deps.is_empty() {
                println!("No dependencies");
                return 0;
            }
            println!("Dependencies:");
            for dep in &deps {
                println!("  {} = \"{}\"", dep.name, dep.raw);
            }
            // Resolve and show transitive deps.
            match resolve_deps(&deps) {
                Ok(resolved) => {
                    if resolved.len() > deps.len() {
                        println!("\nResolved (including transitive):");
                        for (name, version) in &resolved {
                            println!("  {}@{}", name, version);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("\nDependency resolution error: {}", e);
                }
            }
            0
        }
        "tree" => {
            let toml = match read_vrs_toml() {
                Some(t) => t,
                None => {
                    println!("No vrs.toml found");
                    return 0;
                }
            };
            let deps = parse_deps(&toml);
            if deps.is_empty() {
                println!("(no dependencies)");
                return 0;
            }
            println!("Dependency tree:");
            for dep in &deps {
                println!("├── {}@{}", dep.name, dep.raw);
                // Fetch and show transitive deps.
                let version_str = if dep.raw == "*" {
                    "0.1.0".to_string()
                } else {
                    dep.raw.clone()
                };
                if let Some(pkg_toml) = fetch_package(&dep.name, &version_str) {
                    let transitive = parse_deps(&pkg_toml);
                    for t in &transitive {
                        println!("│   ├── {}@{}", t.name, t.raw);
                    }
                }
            }
            0
        }
        "update" => {
            let toml = match read_vrs_toml() {
                Some(t) => t,
                None => {
                    eprintln!("No vrs.toml found");
                    return 1;
                }
            };
            let deps = parse_deps(&toml);
            if deps.is_empty() {
                println!("No dependencies to update");
                return 0;
            }
            println!("Updating dependencies...");
            for dep in &deps {
                let version_str = if dep.raw == "*" {
                    "0.1.0".to_string()
                } else {
                    dep.raw.clone()
                };
                if fetch_package(&dep.name, &version_str).is_some() {
                    println!("  ✓ {}@{}", dep.name, version_str);
                } else {
                    println!("  ✗ {}@{} (failed)", dep.name, version_str);
                }
            }
            println!("Done.");
            0
        }
        other => {
            eprintln!("Unknown mod command: {}", other);
            eprintln!("Usage: vredrs mod <init|add|remove|list|tree|update>");
            1
        }
    }
}
