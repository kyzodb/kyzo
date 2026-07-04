//! Check 2: no `assert!`/`unwrap()`/`panic!`/`expect(` reachable from a
//! declared decode surface — the shape of the historical `RelationId`
//! bug (its derived `Deserialize` bound-checked only by an `assert!`, fed
//! stored catalog bytes by `RelationHandle::decode`; a corrupt row could
//! panic the whole process instead of refusing typed).
//!
//! The decode surface is DECLARED, not inferred: `xtask/decode-surfaces.toml`
//! names the exact entrypoint functions/methods per file. From each
//! entrypoint the check closes over same-file callees only (one function
//! calling another defined in the same file) — a narrow, deterministic,
//! auditable notion of "reachable," not a whole-crate call graph that could
//! silently drift as the crate grows.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use serde::Deserialize;
use syn::visit::{self, Visit};

use crate::allowlist::Allowlist;
use crate::fsutil::{SourceFile, span_line};
use crate::synutil::mod_is_test_scope;

#[derive(Debug, Deserialize)]
struct SurfaceConfig {
    surface: Vec<Surface>,
}

#[derive(Debug, Deserialize)]
struct Surface {
    file: String,
    entrypoints: Vec<String>,
}

pub fn load_config(root: &std::path::Path) -> anyhow::Result<Vec<(String, Vec<String>)>> {
    let path = root.join("xtask/decode-surfaces.toml");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
    let cfg: SurfaceConfig =
        toml::from_str(&text).map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;
    Ok(cfg
        .surface
        .into_iter()
        .map(|s| (s.file, s.entrypoints))
        .collect())
}

/// A single function/method body, collected under a qualified name
/// (`Type::method` for impl methods, bare name for free functions).
struct FnEntry {
    qualified: String,
    bare: String,
    block: syn::Block,
}

struct FnCollector {
    fns: Vec<FnEntry>,
}

impl<'ast> Visit<'ast> for FnCollector {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if mod_is_test_scope(&node.ident, &node.attrs) {
            return;
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        let name = node.sig.ident.to_string();
        self.fns.push(FnEntry {
            qualified: name.clone(),
            bare: name,
            block: (*node.block).clone(),
        });
        visit::visit_item_fn(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if node.trait_.is_some() {
            visit::visit_item_impl(self, node);
            return;
        }
        let type_name = match node.self_ty.as_ref() {
            syn::Type::Path(tp) => tp.path.segments.last().map(|s| s.ident.to_string()),
            _ => None,
        };
        for item in &node.items {
            if let syn::ImplItem::Fn(f) = item {
                let bare = f.sig.ident.to_string();
                let qualified = match &type_name {
                    Some(t) => format!("{t}::{bare}"),
                    None => bare.clone(),
                };
                self.fns.push(FnEntry {
                    qualified,
                    bare,
                    block: f.block.clone(),
                });
            }
        }
        visit::visit_item_impl(self, node);
    }
}

/// Collects every bare identifier used as a call target (`foo(...)`) or
/// method-call target (`x.foo(...)`) in a block — the raw material for the
/// same-file reachability closure.
struct CallTargets(BTreeSet<String>);
impl<'ast> Visit<'ast> for CallTargets {
    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        if let syn::Expr::Call(c) = node
            && let syn::Expr::Path(p) = c.func.as_ref()
            && let Some(seg) = p.path.segments.last()
        {
            self.0.insert(seg.ident.to_string());
        }
        if let syn::Expr::MethodCall(m) = node {
            self.0.insert(m.method.to_string());
        }
        visit::visit_expr(self, node);
    }
}

const PANIC_MACROS: &[&str] = &[
    "assert",
    "assert_eq",
    "assert_ne",
    "panic",
    "todo",
    "unimplemented",
    "unreachable",
];

pub struct Occurrence {
    pub file: String,
    pub function: String,
    pub line: usize,
    pub kind: String,
}

struct PanicScanner {
    hits: Vec<(usize, String)>,
}
impl<'ast> Visit<'ast> for PanicScanner {
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if let Some(seg) = node.path.segments.last() {
            let name = seg.ident.to_string();
            if PANIC_MACROS.contains(&name.as_str()) {
                self.hits.push((span_line(&node.path.span()), name));
            }
        }
        visit::visit_macro(self, node);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let m = node.method.to_string();
        if m == "unwrap" || m == "unwrap_err" || m == "expect" {
            self.hits.push((span_line(&node.method.span()), m));
        }
        visit::visit_expr_method_call(self, node);
    }
}

use syn::spanned::Spanned;

pub fn check(
    files: &[SourceFile],
    surfaces: &[(String, Vec<String>)],
    allow: &Allowlist,
) -> (Vec<Occurrence>, Vec<String>) {
    let mut occurrences = Vec::new();
    let mut missing_files = Vec::new();
    // Every raw hit found, before allowlist filtering — the stale-waiver
    // check below needs this to know whether a waiver still matches
    // anything real.
    let mut all_hits: Vec<(String, String, usize)> = Vec::new();

    for (surface_file, entrypoints) in surfaces {
        let Some(f) = files
            .iter()
            .find(|f| f.rel_path.ends_with(surface_file.trim_start_matches("./")))
        else {
            missing_files.push(format!(
                "decode-surfaces.toml declares `{surface_file}` but it is not in the tree"
            ));
            continue;
        };

        let mut collector = FnCollector { fns: Vec::new() };
        collector.visit_file(&f.ast);

        let by_qualified: BTreeMap<&str, &FnEntry> = collector
            .fns
            .iter()
            .map(|e| (e.qualified.as_str(), e))
            .collect();
        let mut by_bare: BTreeMap<&str, Vec<&FnEntry>> = BTreeMap::new();
        for e in &collector.fns {
            by_bare.entry(e.bare.as_str()).or_default().push(e);
        }

        let mut visited: BTreeSet<&str> = BTreeSet::new();
        let mut queue: VecDeque<&str> = VecDeque::new();
        for ep in entrypoints {
            if by_qualified.contains_key(ep.as_str()) {
                queue.push_back(ep.as_str());
            } else if let Some(matches) = by_bare.get(ep.as_str()) {
                for m in matches {
                    queue.push_back(m.qualified.as_str());
                }
            }
        }

        while let Some(name) = queue.pop_front() {
            if !visited.insert(name) {
                continue;
            }
            let Some(entry) = by_qualified.get(name) else {
                continue;
            };
            let mut targets = CallTargets(BTreeSet::new());
            targets.visit_block(&entry.block);
            for t in targets.0 {
                if let Some(matches) = by_bare.get(t.as_str()) {
                    for m in matches {
                        if !visited.contains(m.qualified.as_str()) {
                            queue.push_back(m.qualified.as_str());
                        }
                    }
                }
            }
        }

        for name in &visited {
            let Some(entry) = by_qualified.get(name) else {
                continue;
            };
            let mut scanner = PanicScanner { hits: Vec::new() };
            scanner.visit_block(&entry.block);
            for (line, kind) in scanner.hits {
                all_hits.push((f.rel_path.clone(), name.to_string(), line));
                let allowed = allow.panic_lint.iter().any(|e| {
                    f.rel_path.ends_with(e.file.trim_start_matches("./"))
                        && &e.function == name
                        && e.line == line
                });
                if allowed {
                    continue;
                }
                occurrences.push(Occurrence {
                    file: f.rel_path.clone(),
                    function: name.to_string(),
                    line,
                    kind,
                });
            }
        }
    }

    for e in &allow.panic_lint {
        let still_matches = all_hits.iter().any(|(file, func, line)| {
            file.ends_with(e.file.trim_start_matches("./"))
                && func == &e.function
                && *line == e.line
        });
        if !still_matches {
            missing_files.push(format!(
                "panic_lint waiver for {}::{} at line {} no longer matches any panic-shaped construct — remove it",
                e.file, e.function, e.line
            ));
        }
    }

    (occurrences, missing_files)
}
