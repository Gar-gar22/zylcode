//! Package layout, module paths and import resolution.
//!
//! Resolution is a lookup problem, not an inference problem: given a specifier
//! written in source, find the file (and from there the symbol) it names. Three
//! things make that deterministic:
//!
//! * **Manifests.** `Cargo.toml` gives package names, dependencies and the crate
//!   root; `package.json` gives the same for npm packages.
//! * **File layout.** Rust `mod a;` in `src/x.rs` means `src/x/a.rs` (or
//!   `src/x/a/mod.rs`). That rule is exact, which is why `mod` declarations
//!   resolve without any guessing.
//! * **Extension probing.** A relative TS specifier resolves by trying the
//!   real extension set and `index.*`. When nothing matches we report
//!   `Unresolved`, never an invented target.
//!
//! Nothing here falls back to text search.

use std::collections::{HashMap, HashSet};
use std::path::Path;

// ---------------------------------------------------------------------------
// Packages
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageKind {
    Cargo,
    Npm,
}

/// One discovered package (a Cargo package or an npm package).
#[derive(Debug, Clone)]
pub struct Package {
    /// Manifest name: `zylcode-core`, `zylcode-desktop`.
    pub name: String,
    /// Repo-relative directory containing the manifest (`""` for the root).
    pub dir: String,
    /// Repo-relative manifest path.
    pub manifest: String,
    pub kind: PackageKind,
    /// Dependency names declared in the manifest.
    pub deps: Vec<String>,
    /// Repo-relative crate-root file (`src/lib.rs` / `src/main.rs`) when known.
    pub primary: Option<String>,
}

impl Package {
    /// The value used as `crate_key` for files belonging to this package.
    pub fn crate_key(&self) -> String {
        self.primary.clone().unwrap_or_else(|| self.dir.clone())
    }
}

/// All packages found under the repository root, plus workspace-level deps.
#[derive(Debug, Clone, Default)]
pub struct Layout {
    pub packages: Vec<Package>,
    /// Keys of the workspace root `[workspace.dependencies]` table. Members
    /// inherit these, so a bare `use serde::..` is still a known external.
    pub workspace_deps: Vec<String>,
    by_dir: HashMap<String, Vec<usize>>,
}

impl Layout {
    /// Walk for manifests and parse them. Manifest-only walk: source scanning
    /// is [`crate::scan::scan`]'s job.
    pub fn discover(root: &Path) -> Layout {
        let mut cargo: Vec<std::path::PathBuf> = Vec::new();
        let mut npm: Vec<std::path::PathBuf> = Vec::new();
        let walker = ignore::WalkBuilder::new(root)
            .hidden(false)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .parents(true)
            .build();
        for entry in walker.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            match path.file_name().and_then(|n| n.to_str()) {
                Some("Cargo.toml") => cargo.push(path.to_path_buf()),
                Some("package.json") => npm.push(path.to_path_buf()),
                _ => continue,
            }
        }
        cargo.sort();
        npm.sort();

        let mut layout = Layout::default();
        for path in cargo {
            if let Some(pkg) = parse_cargo(root, &path) {
                layout.push(pkg);
            }
        }
        for path in npm {
            if let Some(pkg) = parse_npm(root, &path) {
                layout.push(pkg);
            }
        }
        layout
    }

    fn push(&mut self, pkg: Package) {
        let idx = self.packages.len();
        self.by_dir.entry(pkg.dir.clone()).or_default().push(idx);
        self.packages.push(pkg);
    }

    /// Nearest ancestor directory with a manifest of `kind`.
    pub fn package_for_file(&self, file: &str, kind: PackageKind) -> Option<&Package> {
        let mut dir = dir_of(file);
        loop {
            if let Some(idxs) = self.by_dir.get(&dir) {
                if let Some(&idx) = idxs.iter().find(|&&i| self.packages[i].kind == kind) {
                    return Some(&self.packages[idx]);
                }
            }
            if dir.is_empty() {
                return None;
            }
            dir = dir_of(&dir);
        }
    }

    /// A Cargo package whose name matches `name` (Rust `crate_name` form).
    pub fn cargo_named(&self, name: &str) -> Option<&Package> {
        let want = name.replace('-', "_");
        self.packages
            .iter()
            .find(|p| p.kind == PackageKind::Cargo && p.name.replace('-', "_") == want)
    }

    /// An npm package whose name matches exactly (npm names keep their `-`).
    pub fn npm_named(&self, name: &str) -> Option<&Package> {
        self.packages
            .iter()
            .find(|p| p.kind == PackageKind::Npm && p.name == name)
    }

    pub fn workspace_dep_is_crate(&self, name: &str) -> bool {
        let want = name.replace('-', "_");
        self.workspace_deps
            .iter()
            .any(|d| d.replace('-', "_") == want)
    }
}

fn parse_cargo(root: &Path, path: &Path) -> Option<Package> {
    let dir = crate::scan::repo_relative(root, path.parent()?)?;
    let text = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = text.parse().ok()?;

    let mut deps = Vec::new();
    for table in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(t) = value.get(table).and_then(|t| t.as_table()) {
            deps.extend(t.keys().cloned());
        }
    }
    if let Some(ws) = value.get("workspace").and_then(|w| w.as_table()) {
        if let Some(t) = ws.get("dependencies").and_then(|d| d.as_table()) {
            deps.extend(t.keys().cloned());
        }
    }

    let name = value
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            // Virtual manifest (workspace root): identify it by directory.
            if dir.is_empty() {
                "workspace".to_string()
            } else {
                dir.rsplit('/').next().unwrap_or(&dir).to_string()
            }
        });

    let primary = ["src/lib.rs", "src/main.rs"].iter().find_map(|rel| {
        let candidate = if dir.is_empty() {
            (*rel).to_string()
        } else {
            format!("{dir}/{rel}")
        };
        if path_is_file(root, &candidate) {
            Some(candidate)
        } else {
            None
        }
    });

    Some(Package {
        name,
        dir,
        manifest: crate::scan::repo_relative(root, path)?,
        kind: PackageKind::Cargo,
        deps,
        primary,
    })
}

fn parse_npm(root: &Path, path: &Path) -> Option<Package> {
    let dir = crate::scan::repo_relative(root, path.parent()?)?;
    let text = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;

    let mut deps = Vec::new();
    for key in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(obj) = value.get(key).and_then(|d| d.as_object()) {
            deps.extend(obj.keys().cloned());
        }
    }
    let name = value
        .get("name")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if dir.is_empty() {
                "workspace".to_string()
            } else {
                dir.rsplit('/').next().unwrap_or(&dir).to_string()
            }
        });

    Some(Package {
        name,
        dir,
        manifest: crate::scan::repo_relative(root, path)?,
        kind: PackageKind::Npm,
        deps,
        primary: None,
    })
}

fn path_is_file(root: &Path, rel: &str) -> bool {
    root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
        .is_file()
}

// ---------------------------------------------------------------------------
// Placement
// ---------------------------------------------------------------------------

/// Where a file sits in the module tree: `(module, crate_key)`.
///
/// Rust `crate_key` is the crate **root file**, not the package directory, so
/// `src/lib.rs` and `src/main.rs` — two distinct `crate::` roots in one package —
/// never collide in the module table. npm `crate_key` is the package directory.
pub fn place(files: &HashSet<String>, layout: &Layout, file: &str) -> (String, String) {
    if file.ends_with(".rs") {
        return place_rust(files, layout, file);
    }
    place_script(layout, file)
}

fn place_rust(_files: &HashSet<String>, layout: &Layout, file: &str) -> (String, String) {
    let Some(pkg) = layout.package_for_file(file, PackageKind::Cargo) else {
        // Stray file with no package: it is its own crate root.
        return ("crate".to_string(), file.to_string());
    };
    let rel = match strip_prefix_dir(file, &pkg.dir) {
        Some(r) => r,
        None => return ("crate".to_string(), file.to_string()),
    };

    // Crate roots keep `crate` as their module and key the tree themselves.
    if rel == "src/lib.rs" || rel == "src/main.rs" {
        return ("crate".to_string(), file.to_string());
    }
    if let Some(rest) = rel.strip_prefix("src/bin/") {
        if !rest.contains('/') && rest.ends_with(".rs") {
            return ("crate".to_string(), file.to_string());
        }
    }
    if let Some(rest) = rel.strip_prefix("src/") {
        let mut rest = rest.strip_suffix(".rs").unwrap_or(rest).to_string();
        if rest == "lib" || rest == "main" {
            return ("crate".to_string(), file.to_string());
        }
        if let Some(stripped) = rest.strip_suffix("/mod") {
            rest = stripped.to_string();
        }
        let module = format!("crate::{}", rest.replace('/', "::"));
        return (module, pkg.crate_key());
    }
    // `tests/`, `examples/`, `benches/`, `build.rs`: each is its own crate root.
    ("crate".to_string(), file.to_string())
}

fn place_script(layout: &Layout, file: &str) -> (String, String) {
    let pkg = layout.package_for_file(file, PackageKind::Npm);
    let dir = pkg.map(|p| p.dir.clone()).unwrap_or_default();
    let rel = strip_prefix_dir(file, &dir).unwrap_or_else(|| file.to_string());
    (module_from_script_path(&rel), dir)
}

/// `src/components/shell/SurfaceHost.tsx` -> `src::components::shell::SurfaceHost`.
fn module_from_script_path(rel: &str) -> String {
    let mut path = rel.to_string();
    if let Some(stripped) = path.strip_suffix(".d.ts") {
        path = stripped.to_string();
    } else {
        let dot = path
            .rfind('.')
            .filter(|i| *i > path.rfind('/').unwrap_or(0));
        if let Some(i) = dot {
            path.truncate(i);
        }
    }
    path.replace('/', "::")
}

fn strip_prefix_dir(file: &str, dir: &str) -> Option<String> {
    if dir.is_empty() {
        return Some(file.to_string());
    }
    let with_sep = format!("{dir}/");
    file.strip_prefix(&with_sep).map(|s| s.to_string())
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// Where a module specifier landed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModuleTarget {
    /// Resolved to a file in this repository.
    File(String),
    /// Declared in a manifest (or an implicit system crate) — outside the repo.
    External(String),
    /// Neither found locally nor declared anywhere. Reported, never invented.
    Unresolved(String),
}

const SCRIPT_EXTS: &[&str] = &[
    ".ts", ".tsx", ".d.ts", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue",
];
const INDEX_FILES: &[&str] = &[
    "index.ts",
    "index.tsx",
    "index.d.ts",
    "index.js",
    "index.jsx",
    "index.mjs",
    "index.cjs",
];
const IMPLICIT_RUST_CRATES: &[&str] = &["std", "core", "alloc", "proc_macro"];

/// Cross-file resolution over a known file set.
#[derive(Debug, Clone, Default)]
pub struct Resolver {
    pub files: HashSet<String>,
    pub layout: Layout,
    /// `(crate_key, module)` -> file. Path-derived modules are inserted before
    /// inline-module aliases, so an exact file wins a collision.
    module_index: HashMap<(String, String), String>,
}

impl Resolver {
    pub fn new(files: HashSet<String>, layout: Layout) -> Resolver {
        Resolver {
            files,
            layout,
            module_index: HashMap::new(),
        }
    }

    pub fn add_module(&mut self, crate_key: &str, module: &str, file: &str) {
        self.module_index
            .entry((crate_key.to_string(), module.to_string()))
            .or_insert_with(|| file.to_string());
    }

    pub fn module_to_file(&self, crate_key: &str, module: &str) -> Option<&str> {
        self.module_index
            .get(&(crate_key.to_string(), module.to_string()))
            .map(|s| s.as_str())
    }

    /// Index lookup followed by an exact filesystem probe of the Rust `mod`
    /// layout. The probe is what lets `crate::x` from `src/main.rs` find
    /// `src/x.rs` even though that file belongs to the library crate.
    pub fn lookup_rust(&self, crate_key: &str, segs: &[String]) -> Option<String> {
        let rest: &[String] = if segs.first().map(|s| s == "crate").unwrap_or(false) {
            &segs[1..]
        } else {
            segs
        };
        let module = if rest.is_empty() {
            "crate".to_string()
        } else {
            format!("crate::{}", rest.join("::"))
        };
        if let Some(f) = self.module_to_file(crate_key, &module) {
            return Some(f.to_string());
        }
        if rest.is_empty() {
            return self
                .files
                .contains(crate_key)
                .then(|| crate_key.to_string());
        }
        let dir = if crate_key.ends_with(".rs") {
            dir_of(crate_key)
        } else {
            crate_key.to_string()
        };
        if dir.is_empty() {
            return None;
        }
        let base = format!("{dir}/{}", rest.join("/"));
        [format!("{base}.rs"), format!("{base}/mod.rs")]
            .into_iter()
            .find(|candidate| self.files.contains(candidate))
    }

    /// Resolve a Rust module specifier written in `from_file`, appearing in
    /// scope `decl_scope`, against that file's `crate_key`.
    pub fn resolve_rust(&self, crate_key: &str, decl_scope: &str, spec: &str) -> ModuleTarget {
        let segs: Vec<String> = split_path(spec);
        if segs.is_empty() {
            return ModuleTarget::Unresolved(spec.to_string());
        }
        let scope: Vec<String> = split_path(decl_scope);

        match segs[0].as_str() {
            "crate" => self.target(self.lookup_rust(crate_key, &segs), spec),
            "self" => {
                let mut full = scope;
                full.extend(segs[1..].iter().cloned());
                self.target(self.lookup_rust(crate_key, &full), spec)
            }
            "super" => {
                let mut full = scope;
                let mut i = 0;
                while i < segs.len() && segs[i] == "super" {
                    if full.len() <= 1 {
                        // Cannot climb above the crate root: `super` is invalid
                        // here and we say so rather than clamping silently.
                        return ModuleTarget::Unresolved(spec.to_string());
                    }
                    full.pop();
                    i += 1;
                }
                full.extend(segs[i..].iter().cloned());
                self.target(self.lookup_rust(crate_key, &full), spec)
            }
            _ => self.resolve_bare_rust(crate_key, &segs, spec),
        }
    }

    fn resolve_bare_rust(&self, crate_key: &str, segs: &[String], spec: &str) -> ModuleTarget {
        let first = segs[0].replace('-', "_");

        // 1. A module of this crate written without a prefix (2015-edition
        //    style, or a path that happens to resolve locally).
        if let Some(f) = self.lookup_rust(crate_key, segs) {
            return ModuleTarget::File(f);
        }
        // 2. Another package in this workspace.
        if let Some(pkg) = self.layout.cargo_named(&first) {
            let key = pkg.crate_key();
            let mut full = vec!["crate".to_string()];
            full.extend(segs[1..].iter().cloned());
            if let Some(f) = self.lookup_rust(&key, &full) {
                return ModuleTarget::File(f);
            }
            return ModuleTarget::External(first);
        }
        // 3. System crates and workspace-level dependencies.
        if IMPLICIT_RUST_CRATES.contains(&first.as_str())
            || self.layout.workspace_dep_is_crate(&first)
        {
            return ModuleTarget::External(first);
        }
        // 4. A dependency declared by the package that owns `crate_key`.
        //    Longest matching prefix wins: the workspace root (dir `""`)
        //    prefixes everything and must not answer first.
        if let Some(pkg) = self
            .layout
            .packages
            .iter()
            .filter(|p| p.kind == PackageKind::Cargo && crate_key.starts_with(&pkg_prefix(p)))
            .max_by_key(|p| p.dir.len())
        {
            if pkg.deps.iter().any(|d| d.replace('-', "_") == first) {
                return ModuleTarget::External(first);
            }
        }
        ModuleTarget::Unresolved(spec.to_string())
    }

    fn target(&self, found: Option<String>, spec: &str) -> ModuleTarget {
        match found {
            Some(f) => ModuleTarget::File(f),
            None => ModuleTarget::Unresolved(spec.to_string()),
        }
    }

    /// Resolve a relative (or root-absolute) script specifier by extension and
    /// `index.*` probing.
    pub fn resolve_relative(&self, from_file: &str, spec: &str) -> ModuleTarget {
        let spec = spec
            .split('#')
            .next()
            .unwrap_or(spec)
            .split('?')
            .next()
            .unwrap_or(spec);
        let cleaned = spec.trim();
        if let Some(rest) = cleaned.strip_prefix('/') {
            let norm = normalize_dots(rest);
            return self.probe(&norm).map_or_else(
                || ModuleTarget::Unresolved(spec.to_string()),
                ModuleTarget::File,
            );
        }
        if !(cleaned.starts_with("./") || cleaned.starts_with("../") || cleaned == ".") {
            return ModuleTarget::Unresolved(spec.to_string());
        }
        let base = dir_of(from_file);
        let joined = if base.is_empty() {
            cleaned.to_string()
        } else {
            format!("{base}/{cleaned}")
        };
        let norm = normalize_dots(&joined);
        self.probe(&norm).map_or_else(
            || ModuleTarget::Unresolved(spec.to_string()),
            ModuleTarget::File,
        )
    }

    /// `base` with no extension: try the real extension set, then `index.*`.
    pub fn probe(&self, base: &str) -> Option<String> {
        if base.is_empty() {
            return None;
        }
        if self.files.contains(base) {
            return Some(base.to_string());
        }
        for ext in SCRIPT_EXTS {
            let candidate = format!("{base}{ext}");
            if self.files.contains(&candidate) {
                return Some(candidate);
            }
        }
        for index in INDEX_FILES {
            let candidate = format!("{base}/{index}");
            if self.files.contains(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// Bare npm specifier: workspace package first (pnpm resolves them
    /// locally), otherwise an external package.
    pub fn resolve_npm(&self, spec: &str) -> ModuleTarget {
        let cleaned = spec.split('#').next().unwrap_or(spec);
        let cleaned = cleaned.split('?').next().unwrap_or(cleaned);
        let parts = split_path(cleaned);
        if parts.is_empty() {
            return ModuleTarget::Unresolved(spec.to_string());
        }
        let scoped = parts[0].starts_with('@');
        let name = if scoped && parts.len() > 1 {
            format!("{}/{}", parts[0], parts[1])
        } else {
            parts[0].clone()
        };
        let rest: &[String] = if scoped && parts.len() > 1 {
            &parts[2..]
        } else {
            &parts[1..]
        };
        if let Some(pkg) = self.layout.npm_named(&name) {
            let base = if pkg.dir.is_empty() {
                rest.join("/")
            } else if rest.is_empty() {
                pkg.dir.clone()
            } else {
                format!("{}/{}", pkg.dir, rest.join("/"))
            };
            if let Some(f) = self.probe(&base) {
                return ModuleTarget::File(f);
            }
            if rest.is_empty() {
                if let Some(main) = self.probe(&pkg.dir) {
                    return ModuleTarget::File(main);
                }
            }
            return ModuleTarget::External(name);
        }
        ModuleTarget::External(name)
    }

    /// Resolve an import statement's target file, choosing the algorithm by
    /// language and specifier shape.
    pub fn resolve_specifier(
        &self,
        is_rust: bool,
        crate_key: &str,
        decl_scope: &str,
        from_file: &str,
        spec: &str,
    ) -> ModuleTarget {
        if is_rust {
            return self.resolve_rust(crate_key, decl_scope, spec);
        }
        let trimmed = spec.trim();
        if trimmed.starts_with('.') || trimmed.starts_with('/') {
            self.resolve_relative(from_file, trimmed)
        } else if trimmed.is_empty() {
            ModuleTarget::Unresolved(spec.to_string())
        } else {
            self.resolve_npm(trimmed)
        }
    }
}

fn pkg_prefix(p: &Package) -> String {
    if p.dir.is_empty() {
        String::new()
    } else {
        format!("{}/", p.dir)
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// `a/b/c` -> `["a","b","c"]`; `crate::a::b` -> `["crate","a","b"]`.
/// Empty segments are dropped so `a::::b` and `a::b` agree.
pub fn split_path(spec: &str) -> Vec<String> {
    spec.split(['/', ':'])
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Directory part of a `/`-separated path; `""` at the top level.
pub fn dir_of(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

/// Resolve `.` and `..` in a `/`-separated relative path.
pub fn normalize_dots(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            p => out.push(p),
        }
    }
    out.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_path_handles_both_separators() {
        assert_eq!(split_path("crate::a::b"), vec!["crate", "a", "b"]);
        assert_eq!(split_path("./foo/bar"), vec![".", "foo", "bar"]);
        assert_eq!(split_path(""), Vec::<String>::new());
    }

    #[test]
    fn normalize_resolves_dot_segments() {
        assert_eq!(normalize_dots("src/../lib"), "lib");
        assert_eq!(normalize_dots("./a/./b"), "a/b");
        assert_eq!(normalize_dots("a/b/../../c"), "c");
        assert_eq!(normalize_dots("src/.."), "");
    }

    #[test]
    fn script_module_strips_the_right_extension() {
        assert_eq!(
            module_from_script_path("src/components/App.tsx"),
            "src::components::App"
        );
        assert_eq!(module_from_script_path("src/types.d.ts"), "src::types");
        assert_eq!(module_from_script_path("src/x.js"), "src::x");
    }

    #[test]
    fn placement_of_script_uses_package_dir() {
        let mut layout = Layout::default();
        layout.push(Package {
            name: "zylcode-desktop".into(),
            dir: "apps/zylcode-desktop".into(),
            manifest: "apps/zylcode-desktop/package.json".into(),
            kind: PackageKind::Npm,
            deps: vec![],
            primary: None,
        });
        let (module, key) = place(&HashSet::new(), &layout, "apps/zylcode-desktop/src/App.tsx");
        assert_eq!(module, "src::App");
        assert_eq!(key, "apps/zylcode-desktop");
    }

    #[test]
    fn rust_crate_roots_are_their_own_crate_key() {
        let mut layout = Layout::default();
        layout.push(Package {
            name: "demo".into(),
            dir: "crates/demo".into(),
            manifest: "crates/demo/Cargo.toml".into(),
            kind: PackageKind::Cargo,
            deps: vec![],
            primary: Some("crates/demo/src/lib.rs".into()),
        });
        let mut files = HashSet::new();
        files.insert("crates/demo/src/lib.rs".to_string());
        files.insert("crates/demo/src/a.rs".to_string());

        let (m, k) = place(&files, &layout, "crates/demo/src/lib.rs");
        assert_eq!(
            (m.as_str(), k.as_str()),
            ("crate", "crates/demo/src/lib.rs")
        );

        let (m, k) = place(&files, &layout, "crates/demo/src/a.rs");
        assert_eq!(m, "crate::a");
        assert_eq!(k, "crates/demo/src/lib.rs");
    }
}
