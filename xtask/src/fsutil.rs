//! Shared tree-walking: every check operates over the same notion of "the
//! engine source tree" so a bite-proof run against a throwaway copy sees
//! exactly the files a real CI run would.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// One parsed source file: its path (relative to `root`), the raw text (for
/// line lookups), and its `syn` AST.
pub struct SourceFile {
    /// Repo-root-relative path, e.g. `kyzo-core/src/data/tuple.rs`. Stable
    /// across a bite-proof's throwaway rsync copy, so allowlist entries
    /// (which cite this form) still resolve there.
    pub rel_path: String,
    pub text: String,
    pub ast: syn::File,
}

/// Every `.rs` file under `kyzo-core/src` and `kyzo-bin/src`, relative to
/// `root` (the workspace root — a real checkout or a bite-proof's rsync
/// copy). Both engine crates, never the bindings: the ontology gate is
/// scoped to the isolated core, same boundary as the pure-Rust gate.
pub fn walk_engine_sources(root: &Path) -> Result<Vec<SourceFile>> {
    let mut out = Vec::new();
    for crate_dir in ["kyzo-core/src", "kyzo-bin/src"] {
        let abs = root.join(crate_dir);
        if !abs.exists() {
            continue;
        }
        for entry in walkdir::WalkDir::new(&abs)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            let ast = syn::parse_file(&text)
                .with_context(|| format!("parsing {} as Rust", path.display()))?;
            let rel_path = path
                .strip_prefix(root)
                .unwrap_or(path)
                .to_string_lossy()
                .replace('\\', "/");
            out.push(SourceFile {
                rel_path,
                text,
                ast,
            });
        }
    }
    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

/// Byte offset -> 1-based line number, for reporting `file:line` against a
/// `proc_macro2::Span` (which `syn`, run outside a proc-macro, only gives us
/// as a line/column pair already — kept here as the one place that maps a
/// span to the line-number convention the rest of the tool reports in).
pub fn span_line(span: &proc_macro2::Span) -> usize {
    span.start().line
}

pub fn repo_root() -> Result<PathBuf> {
    // xtask's own manifest dir is `<root>/xtask`; the workspace root is one
    // level up. Overridable so bite-proofs can point at a throwaway copy.
    if let Ok(r) = std::env::var("RESONANCE_ROOT") {
        return Ok(PathBuf::from(r));
    }
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR")
        .context("CARGO_MANIFEST_DIR not set (run via `cargo run -p xtask`)")?;
    Ok(PathBuf::from(manifest_dir)
        .parent()
        .context("xtask has no parent directory")?
        .to_path_buf())
}
