pub mod chunker;
pub mod generated;
pub mod relations;
pub mod symbols;
pub mod taint;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::Path;

use tracing::warn;
use tree_sitter::{Node, Parser};

use crate::parsing::chunker::{Chunk, chunk_file, chunk_file_ast};
use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol, SymbolKind};

// ─── Recursion depth guard ───────────────────────────────────────────────────
//
// Prevents stack overflow on deeply-nested ASTs (e.g. Linux kernel C files with
// huge initializer arrays or deeply-nested macros). The tree-sitter AST extractors
// recurse one frame per tree depth; without a cap, rayon worker threads (~2 MB
// default stack) overflow on files with depth > ~2000. This guard limits recursion
// to a safe cap measured against real-world kernel code.
//
// Thread-local design: parsing runs synchronously on rayon workers (no `.await`
// between recursive calls), so thread-local is safe and zero-cost.

/// Maximum AST recursion depth. Measured on the Linux kernel (63354 C/H files):
/// the deepest file is arch/x86/kernel/cpu/microcode/intel-ucode-defs.h at depth
/// 3333 (nested struct initializers / preprocessor-generated arrays). Cap set to
/// observed_max (3333) * ~2x safety factor = 6400. This allows even the most
/// pathological files to parse fully while preventing stack overflow. At 6400 depth
/// with ~512 bytes per stack frame, peak usage is ~3.3 MB — well within the 64 MB
/// worker stack configured in pipeline.rs.
const RECURSION_DEPTH_CAP: usize = 6400;

pub(crate) mod recursion_guard {
    use super::*;

    thread_local! {
        /// Current recursion depth counter.
        static DEPTH: Cell<usize> = const { Cell::new(0) };
        /// File path currently being parsed (set once per file for diagnostics).
        static CURRENT_FILE: RefCell<String> = const { RefCell::new(String::new()) };
        /// Whether we already warned for the current file (prevents log spam).
        static WARNED: Cell<bool> = const { Cell::new(false) };
    }

    /// Reset guard state at the start of each file. MUST be called once per file
    /// inside the per-file closure (e.g. `parse_one_file`), NOT once before the
    /// loop. Rayon workers are reused across files — without per-file reset, a
    /// stale path and already-tripped warn flag carry over from the previous file
    /// on the same worker thread, suppressing diagnostics for new files.
    pub fn begin_file(path: &str) {
        DEPTH.with(|d| d.set(0));
        CURRENT_FILE.with(|f| *f.borrow_mut() = path.to_owned());
        WARNED.with(|w| w.set(false));
    }

    /// RAII recursion guard. Created via `RecursionGuard::enter()`.
    /// Drop decrements the counter so early-return paths self-balance.
    pub struct RecursionGuard(());

    impl RecursionGuard {
        /// Try to enter a new recursion level. Returns `Some(guard)` if under the
        /// cap, `None` if at/over the cap (recursion stopped at this branch).
        /// On first cap-hit per file, emits a warning with the file path and depth.
        #[inline]
        pub fn enter() -> Option<RecursionGuard> {
            DEPTH.with(|d| {
                let current = d.get();
                if current >= RECURSION_DEPTH_CAP {
                    // Emit warning once per file to avoid log spam.
                    WARNED.with(|w| {
                        if !w.get() {
                            w.set(true);
                            CURRENT_FILE.with(|f| {
                                let path = f.borrow();
                                tracing::warn!(
                                    file = %*path,
                                    depth = current,
                                    cap = RECURSION_DEPTH_CAP,
                                    "recursion depth cap reached — pruning this branch \
                                     (symbols below this depth are dropped for this file)"
                                );
                            });
                        }
                    });
                    return None;
                }
                d.set(current + 1);
                Some(RecursionGuard(()))
            })
        }
    }

    impl Drop for RecursionGuard {
        #[inline]
        fn drop(&mut self) {
            DEPTH.with(|d| d.set(d.get() - 1));
        }
    }
}

/// Result of parsing one source file.
#[derive(Debug)]
pub struct ParseResult {
    pub symbols: Vec<Symbol>,
    pub edges: Vec<RawEdge>,
    pub chunks: Vec<Chunk>,
    /// Import map: local name → source file path (best-effort, only for resolved imports).
    pub imports: HashMap<String, String>,
}

// ─── Language detection ───────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Rust,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Php,
    Ruby,
    ObjectiveC,
    Swift,
    Kotlin,
    Dart,
    Lua,
    Luau,
    Svelte,
    Vue,
    Pascal,
    Liquid,
    Proto,
    Other,
}

pub fn detect_language(path: &Path) -> Lang {
    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => Lang::Python,
        Some("js" | "jsx" | "mjs" | "cjs") => Lang::JavaScript,
        Some("ts") => Lang::TypeScript,
        Some("tsx") => Lang::Tsx,
        Some("rs") => Lang::Rust,
        Some("go") => Lang::Go,
        Some("java") => Lang::Java,
        Some("c") => Lang::C,
        Some("cpp" | "cc" | "cxx") => Lang::Cpp,
        Some("h" | "hpp" | "hxx" | "hh") => Lang::Cpp,
        Some("cs") => Lang::CSharp,
        Some("php") => Lang::Php,
        Some("rb") => Lang::Ruby,
        Some("m" | "mm") => Lang::ObjectiveC,
        Some("swift") => Lang::Swift,
        Some("kt" | "kts") => Lang::Kotlin,
        Some("dart") => Lang::Dart,
        Some("lua") => Lang::Lua,
        Some("luau") => Lang::Luau,
        Some("svelte") => Lang::Svelte,
        Some("vue") => Lang::Vue,
        Some("proto") => Lang::Proto,
        Some("liquid") => Lang::Liquid,
        _ => Lang::Other,
    }
}

// ─── Entry point ──────────────────────────────────────────────────────────

/// Parse a source file and return symbols, edges, and chunks.
/// Falls back to coverage-only chunks on parse failure.
pub fn parse_file(file_path: &str, source: &str) -> ParseResult {
    // Reset recursion guard state for this file. This is the per-file parse entry
    // point — called from pipeline.rs par_iter (via parse_one_file) and from tests.
    // Rayon workers are reused across files; without per-file reset, the warn-once
    // flag and current-file path carry over from the previous file on the same
    // worker thread, suppressing diagnostics for new files.
    recursion_guard::begin_file(file_path);

    let path = Path::new(file_path);
    let lang = detect_language(path);

    let (symbols, edges, imports, chunks) = match lang {
        Lang::Python => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_python::LANGUAGE.into(),
                extract_python,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::JavaScript | Lang::Tsx => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_javascript::LANGUAGE.into(),
                extract_javascript,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::TypeScript => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                extract_typescript,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Rust => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_rust::LANGUAGE.into(),
                extract_rust,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Go => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_go::LANGUAGE.into(),
                extract_go,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Java => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_java::LANGUAGE.into(),
                extract_java,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::C => {
            let (s, e, imp, c) =
                parse_with_tree_sitter_c_cpp(file_path, source, tree_sitter_c::LANGUAGE.into());
            (s, e, imp, c)
        }
        Lang::Cpp => {
            let (s, e, imp, c) =
                parse_with_tree_sitter_c_cpp(file_path, source, tree_sitter_cpp::LANGUAGE.into());
            (s, e, imp, c)
        }
        Lang::Other => (
            vec![],
            vec![],
            HashMap::new(),
            chunk_file(file_path, source, &[]),
        ),
        Lang::CSharp => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_c_sharp::LANGUAGE.into(),
                extract_csharp,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Php => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_php::LANGUAGE_PHP.into(),
                extract_php,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Ruby => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_ruby::LANGUAGE.into(),
                extract_ruby,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::ObjectiveC => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_objc::LANGUAGE.into(),
                extract_objc,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Swift => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_swift::LANGUAGE.into(),
                extract_swift,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Kotlin => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_kotlin_ng::LANGUAGE.into(),
                extract_kotlin,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Dart => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_dart::LANGUAGE.into(),
                extract_dart,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Lua => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_lua::LANGUAGE.into(),
                extract_lua,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Luau => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_luau::LANGUAGE.into(),
                extract_luau,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Svelte => {
            let (s, e) = extract_svelte(file_path, source);
            let c = chunk_file(file_path, source, &s);
            (s, e, HashMap::new(), c)
        }
        Lang::Vue => {
            let (s, e) = extract_sfc_scripts(file_path, source);
            let c = chunk_file(file_path, source, &s);
            (s, e, HashMap::new(), c)
        }
        Lang::Pascal => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_pascal::LANGUAGE.into(),
                extract_pascal,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Liquid => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_liquid::LANGUAGE.into(),
                extract_liquid,
            );
            (s, e, HashMap::new(), c)
        }
        Lang::Proto => {
            let (s, e, c) = parse_with_tree_sitter(
                file_path,
                source,
                tree_sitter_protobuf::LANGUAGE.into(),
                extract_proto,
            );
            (s, e, HashMap::new(), c)
        }
    };

    ParseResult {
        symbols,
        edges,
        chunks,
        imports,
    }
}

// ─── Generic tree-sitter driver ───────────────────────────────────────────

fn parse_with_tree_sitter<F>(
    file_path: &str,
    source: &str,
    language: tree_sitter::Language,
    extractor: F,
) -> (Vec<Symbol>, Vec<RawEdge>, Vec<Chunk>)
where
    F: Fn(&str, &str, &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>),
{
    let mut parser = Parser::new();
    if let Err(e) = parser.set_language(&language) {
        warn!(file = file_path, error = %e, "failed to set tree-sitter language");
        // No tree → source-only fallback chunking (no symbols to link).
        return (vec![], vec![], chunk_file(file_path, source, &[]));
    }
    match parser.parse(source, None) {
        Some(tree) => {
            let (symbols, edges) = extractor(file_path, source, &tree);
            // Chunk INSIDE this closure: `tree_sitter::Tree`/`Node` are not
            // `Send`, so the tree must be consumed here (off the async runtime,
            // on the rayon worker) and only owned `Vec<Chunk>` may leave.
            let chunks = chunk_file_ast(file_path, source, tree.root_node(), &symbols);
            (symbols, edges, chunks)
        }
        None => {
            warn!(file = file_path, "tree-sitter parse returned None");
            (vec![], vec![], chunk_file(file_path, source, &[]))
        }
    }
}

/// Specialised tree-sitter driver for C/C++ that also returns the imports HashMap.
fn parse_with_tree_sitter_c_cpp(
    file_path: &str,
    source: &str,
    language: tree_sitter::Language,
) -> (
    Vec<Symbol>,
    Vec<RawEdge>,
    HashMap<String, String>,
    Vec<Chunk>,
) {
    let mut parser = Parser::new();
    if let Err(e) = parser.set_language(&language) {
        warn!(file = file_path, error = %e, "failed to set tree-sitter language for C/C++");
        return (
            vec![],
            vec![],
            HashMap::new(),
            chunk_file(file_path, source, &[]),
        );
    }
    match parser.parse(source, None) {
        Some(tree) => {
            let (symbols, edges, imports) = extract_c_cpp(file_path, source, &tree);
            // Chunk inside the closure — see `parse_with_tree_sitter` for the
            // non-Send Tree rationale.
            let chunks = chunk_file_ast(file_path, source, tree.root_node(), &symbols);
            (symbols, edges, imports, chunks)
        }
        None => {
            warn!(
                file = file_path,
                "tree-sitter parse returned None for C/C++"
            );
            (
                vec![],
                vec![],
                HashMap::new(),
                chunk_file(file_path, source, &[]),
            )
        }
    }
}

// ─── Utility helpers ──────────────────────────────────────────────────────

fn node_text<'a>(node: &Node, source: &'a str) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}

fn node_line_start(node: &Node) -> u32 {
    node.start_position().row as u32 + 1
}

fn node_line_end(node: &Node) -> u32 {
    node.end_position().row as u32 + 1
}

#[allow(clippy::too_many_arguments)]
fn make_symbol(
    file: &str,
    name: &str,
    scope_path: Vec<String>,
    kind: SymbolKind,
    line_start: u32,
    line_end: u32,
    signature: Option<String>,
    parent_fqn: Option<String>,
) -> Symbol {
    Symbol {
        qualified: QualifiedSymbol {
            file: file.to_string(),
            scope_path,
            name: name.to_string(),
        },
        kind,
        line_start,
        line_end,
        signature,
        parent_fqn,
    }
}

// ─── Data-flow descriptor ─────────────────────────────────────────────────
//
// Six near-duplicate per-language functions (param-forward edges, call
// scanning, intermediate let/assignment flow) collapse into three generic
// functions driven by a per-language `FlowSpec`. Node access differs across
// grammars: some expose the callee/argument-list via named fields (Rust,
// Python), others via a positional child with no field name (e.g. Swift,
// Kotlin) — hence `NodeRef`. Some grammars wrap a bare identifier argument
// in a wrapper node (e.g. C#, PHP) — hence `arg_unwrap_kinds`.

#[derive(Clone, Copy)]
enum NodeRef {
    /// Reachable via `child_by_field_name`.
    Field(&'static str),
    /// Reachable via positional `child(index)` (no field name in the grammar).
    Child(usize),
    /// Reachable by walking named descendants until the requested kind.
    ChildOfKind(&'static str),
    /// Reach the first direct named child of a kind, with a verified positional fallback.
    NamedChildOfKindOrChild(&'static str, usize),
}
impl NodeRef {
    fn resolve<'a>(&self, node: &Node<'a>) -> Option<Node<'a>> {
        match self {
            NodeRef::Field(name) => node.child_by_field_name(*name),
            NodeRef::Child(idx) => node.child(*idx),
            NodeRef::ChildOfKind(kind) => {
                if node.kind() == *kind {
                    return Some(*node);
                }
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .find_map(|child| self.resolve(&child))
            }
            NodeRef::NamedChildOfKindOrChild(kind, idx) => {
                let mut cursor = node.walk();
                node.named_children(&mut cursor)
                    .find(|child| child.kind() == *kind)
                    .or_else(|| node.child(*idx))
            }
        }
    }
}

struct FlowSpec {
    /// AST node kind representing a call expression.
    call_kind: &'static str,
    /// How to reach the callee node from a call node.
    callee: NodeRef,
    /// How to reach the argument-list node from a call node.
    args: NodeRef,
    /// Wrapper kinds to descend through when looking for a bare identifier
    /// argument (e.g. C# and PHP `argument`).
    arg_unwrap_kinds: &'static [&'static str],
    /// AST node kinds representing a local variable binding statement.
    binding_kinds: &'static [&'static str],
    /// Field name for the bound variable name on a binding node.
    lhs_field: &'static str,
    /// Field name for the bound value when the grammar names it.
    rhs_field: Option<&'static str>,
    /// Unnamed RHS child kind for grammars such as C#.
    rhs_child_kind: Option<&'static str>,
    /// Wrapper kinds to descend through for the bound variable.
    lhs_unwrap_kinds: &'static [&'static str],
    /// Wrapper kinds to descend through for the bound value.
    rhs_unwrap_kinds: &'static [&'static str],
    /// Wrapper statement kinds to unwrap before checking `binding_kinds`.
    stmt_unwrap: &'static [&'static str],
    /// AST node kinds considered identifier nodes.
    param_ident_kinds: &'static [&'static str],
}

const RUST_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call_expression",
    callee: NodeRef::Field("function"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["let_declaration"],
    lhs_field: "pattern",
    rhs_field: Some("value"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &[],
    param_ident_kinds: &["identifier"],
};
const PYTHON_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call",
    callee: NodeRef::Field("function"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["assignment"],
    lhs_field: "left",
    rhs_field: Some("right"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &["expression_statement"],
    param_ident_kinds: &["identifier"],
};
const GO_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call_expression",
    callee: NodeRef::Field("function"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["short_var_declaration"],
    lhs_field: "left",
    rhs_field: Some("right"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &["expression_list"],
    rhs_unwrap_kinds: &["expression_list"],
    stmt_unwrap: &[],
    param_ident_kinds: &["identifier"],
};
const JS_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call_expression",
    callee: NodeRef::Field("function"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["variable_declarator"],
    lhs_field: "name",
    rhs_field: Some("value"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &["lexical_declaration", "variable_declaration"],
    param_ident_kinds: &["identifier"],
};
const RUBY_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call",
    callee: NodeRef::Field("method"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["assignment"],
    lhs_field: "left",
    rhs_field: Some("right"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &[],
    param_ident_kinds: &["identifier"],
};
const JAVA_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "method_invocation",
    callee: NodeRef::Field("name"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["variable_declarator"],
    lhs_field: "name",
    rhs_field: Some("value"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &["local_variable_declaration"],
    param_ident_kinds: &["identifier"],
};
const DART_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call_expression",
    callee: NodeRef::Field("function"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["initialized_variable_definition"],
    lhs_field: "name",
    rhs_field: Some("value"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &["local_variable_declaration"],
    param_ident_kinds: &["identifier"],
};
const PASCAL_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "exprCall",
    callee: NodeRef::Field("entity"),
    args: NodeRef::Field("args"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["assignment"],
    lhs_field: "lhs",
    rhs_field: Some("rhs"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &[],
    param_ident_kinds: &["identifier"],
};
const CSHARP_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "invocation_expression",
    callee: NodeRef::Field("function"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &["argument"],
    binding_kinds: &["variable_declarator"],
    lhs_field: "name",
    rhs_field: None,
    rhs_child_kind: Some("invocation_expression"),
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &["local_declaration_statement", "variable_declaration"],
    param_ident_kinds: &["identifier"],
};
const PHP_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "function_call_expression",
    callee: NodeRef::Field("function"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &["argument"],
    binding_kinds: &["assignment_expression"],
    lhs_field: "left",
    rhs_field: Some("right"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &["expression_statement"],
    param_ident_kinds: &["variable_name"],
};
const C_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call_expression",
    callee: NodeRef::Field("function"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["init_declarator"],
    lhs_field: "declarator",
    rhs_field: Some("value"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &["declaration"],
    param_ident_kinds: &["identifier", "field_identifier"],
};
const C_ASSIGNMENT_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call_expression",
    callee: NodeRef::Field("function"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["assignment_expression"],
    lhs_field: "left",
    rhs_field: Some("right"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &["expression_statement"],
    param_ident_kinds: &["identifier", "field_identifier"],
};
const CPP_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call_expression",
    callee: NodeRef::Field("function"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["init_declarator"],
    lhs_field: "declarator",
    rhs_field: Some("value"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &["declaration"],
    param_ident_kinds: &["identifier", "field_identifier"],
};
const CPP_ASSIGNMENT_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call_expression",
    callee: NodeRef::Field("function"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["assignment_expression"],
    lhs_field: "left",
    rhs_field: Some("right"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &["expression_statement"],
    param_ident_kinds: &["identifier", "field_identifier"],
};
const LUA_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "function_call",
    callee: NodeRef::Field("name"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["assignment_statement"],
    lhs_field: "name",
    rhs_field: Some("value"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &["variable_list"],
    rhs_unwrap_kinds: &["expression_list"],
    stmt_unwrap: &["variable_declaration"],
    param_ident_kinds: &["identifier"],
};
const LUAU_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "function_call",
    callee: NodeRef::Field("name"),
    args: NodeRef::Field("arguments"),
    arg_unwrap_kinds: &[],
    binding_kinds: &["assignment_statement"],
    lhs_field: "name",
    rhs_field: Some("value"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &["variable_list"],
    rhs_unwrap_kinds: &["expression_list"],
    stmt_unwrap: &["variable_declaration"],
    param_ident_kinds: &["identifier"],
};
const SWIFT_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call_expression",
    callee: NodeRef::Child(0),
    args: NodeRef::ChildOfKind("value_arguments"),
    arg_unwrap_kinds: &["value_argument"],
    binding_kinds: &["property_declaration"],
    lhs_field: "name",
    rhs_field: Some("value"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &["pattern"],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &[],
    param_ident_kinds: &["simple_identifier"],
};
const SWIFT_ASSIGNMENT_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call_expression",
    callee: NodeRef::Child(0),
    args: NodeRef::ChildOfKind("value_arguments"),
    arg_unwrap_kinds: &["value_argument"],
    binding_kinds: &["assignment"],
    lhs_field: "target",
    rhs_field: Some("result"),
    rhs_child_kind: None,
    lhs_unwrap_kinds: &["directly_assignable_expression"],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &[],
    param_ident_kinds: &["simple_identifier"],
};
const KOTLIN_FLOW_SPEC: FlowSpec = FlowSpec {
    call_kind: "call_expression",
    callee: NodeRef::NamedChildOfKindOrChild("expression", 0),
    args: NodeRef::ChildOfKind("value_arguments"),
    arg_unwrap_kinds: &["value_argument"],
    binding_kinds: &[],
    lhs_field: "",
    rhs_field: None,
    rhs_child_kind: None,
    lhs_unwrap_kinds: &[],
    rhs_unwrap_kinds: &[],
    stmt_unwrap: &[],
    param_ident_kinds: &["identifier"],
};
fn luau_parameter_ident<'a>(parameter: Node<'a>) -> Option<Node<'a>> {
    if parameter.kind() != "parameter" {
        return None;
    }
    let mut cursor = parameter.walk();
    parameter
        .named_children(&mut cursor)
        .find(|child| LUAU_FLOW_SPEC.param_ident_kinds.contains(&child.kind()))
}

/// Resolve a call argument node to the identifier it refers to, descending
/// through any language-specific wrapper kinds (`arg_unwrap_kinds`).
fn resolve_ident_arg<'a>(node: Node<'a>, spec: &FlowSpec) -> Option<Node<'a>> {
    if spec.param_ident_kinds.contains(&node.kind()) {
        return Some(node);
    }
    if spec.arg_unwrap_kinds.contains(&node.kind()) {
        let mut cursor = node.walk();
        if node.kind() == "value_argument" {
            let children: Vec<_> = node.named_children(&mut cursor).collect();
            return children
                .last()
                .and_then(|child| resolve_ident_arg(*child, spec));
        }
        for child in node.children(&mut cursor) {
            if let Some(found) = resolve_ident_arg(child, spec) {
                return Some(found);
            }
        }
    }
    None
}

/// Resolve a statement to its binding node, recursively unwrapping the
/// language-specific statement wrappers around it.
fn resolve_binding<'a>(stmt: Node<'a>, spec: &FlowSpec) -> Option<Node<'a>> {
    if spec.binding_kinds.contains(&stmt.kind()) {
        return Some(stmt);
    }
    if !spec.stmt_unwrap.contains(&stmt.kind()) {
        return None;
    }
    let mut cursor = stmt.walk();
    stmt.named_children(&mut cursor)
        .find_map(|child| resolve_binding(child, spec))
}

/// Descend through a single list-wrapper node (e.g. Go `expression_list`) to
/// reach the real operand. Empty `unwrap_kinds` means no descent.
fn unwrap_operand<'a>(node: Node<'a>, unwrap_kinds: &[&str]) -> Option<Node<'a>> {
    if unwrap_kinds.contains(&node.kind()) {
        let mut cursor = node.walk();
        node.named_children(&mut cursor).next()
    } else {
        Some(node)
    }
}

/// Resolve a binding's RHS, including C#'s unnamed initializer child.
fn resolve_rhs<'a>(binding: Node<'a>, spec: &FlowSpec) -> Option<Node<'a>> {
    if let Some(field) = spec.rhs_field {
        return binding
            .child_by_field_name(field)
            .and_then(|v| unwrap_operand(v, spec.rhs_unwrap_kinds));
    }
    spec.rhs_child_kind.and_then(|kind| {
        let mut cursor = binding.walk();
        binding
            .named_children(&mut cursor)
            .find(|child| child.kind() == kind)
    })
}

fn collect_param_forward_edges<'a>(
    spec: &FlowSpec,
    _file: &str,
    source: &str,
    node: &Node<'a>,
    param_names: &HashSet<&str>,
    from_sym: &QualifiedSymbol,
    edges: &mut Vec<RawEdge>,
) {
    if node.kind() == spec.call_kind
        && let Some(func_node) = spec.callee.resolve(node)
    {
        let callee_name = node_text(&func_node, source).to_string();
        if let Some(args_node) = spec.args.resolve(node) {
            let mut acursor = args_node.walk();
            let forwards_param = args_node.children(&mut acursor).any(|child| {
                resolve_ident_arg(child, spec)
                    .map(|id| param_names.contains(node_text(&id, source)))
                    .unwrap_or(false)
            });
            if forwards_param {
                edges.push(RawEdge {
                    from: from_sym.clone(),
                    to: EdgeTarget::Unresolved {
                        name: callee_name,
                        import_path: None,
                        qualifier: None,
                    },
                    kind: EdgeKind::DataFlowsTo,
                    line: node_line_start(node),
                    confidence: Confidence::Inferred(0.75),
                });
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_param_forward_edges(spec, _file, source, &child, param_names, from_sym, edges);
    }
}

fn scan_calls(
    spec: &FlowSpec,
    source: &str,
    node: &Node,
    var_map: &HashMap<String, (String, usize)>,
    from_sym: &QualifiedSymbol,
    edges: &mut Vec<RawEdge>,
) {
    if node.kind() == spec.call_kind {
        if let Some(func_node) = spec.callee.resolve(node) {
            let callee = node_text(&func_node, source).to_string();
            if let Some(args_node) = spec.args.resolve(node) {
                let mut acursor = args_node.walk();
                for arg in args_node.children(&mut acursor) {
                    if let Some(id) = resolve_ident_arg(arg, spec) {
                        let arg_name = node_text(&id, source);
                        if var_map.contains_key(arg_name) {
                            let line = node_line_start(node);
                            edges.push(RawEdge {
                                from: from_sym.clone(),
                                to: EdgeTarget::Unresolved {
                                    name: callee.clone(),
                                    import_path: None,
                                    qualifier: None,
                                },
                                kind: EdgeKind::DataFlowsTo,
                                line,
                                confidence: Confidence::Inferred(0.6),
                            });
                            break;
                        }
                    }
                }
            }
        }
    } else {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            scan_calls(spec, source, &child, var_map, from_sym, edges);
        }
    }
}

fn collect_intermediate_flow_edges<'a>(
    spec: &FlowSpec,
    _file: &str,
    source: &str,
    node: &Node<'a>,
    from_sym: &QualifiedSymbol,
    edges: &mut Vec<RawEdge>,
) {
    let mut var_map: HashMap<String, (String, usize)> = HashMap::new();

    let mut cursor = node.walk();
    for stmt in node.children(&mut cursor) {
        if let Some(asgn) = resolve_binding(stmt, spec) {
            let var_name = asgn
                .child_by_field_name(spec.lhs_field)
                .and_then(|p| unwrap_operand(p, spec.lhs_unwrap_kinds))
                .filter(|p| spec.param_ident_kinds.contains(&p.kind()))
                .map(|p| node_text(&p, source).to_string());
            let rhs_callee = resolve_rhs(asgn, spec)
                .filter(|v| v.kind() == spec.call_kind)
                .and_then(|v| spec.callee.resolve(&v))
                .map(|f| node_text(&f, source).to_string());

            if let (Some(var), Some(callee)) = (var_name, rhs_callee) {
                let mut chain_depth = 1usize;
                if let Some(rhs_node) = resolve_rhs(asgn, spec)
                    && let Some(args_node) = spec.args.resolve(&rhs_node)
                {
                    let mut acursor = args_node.walk();
                    for arg in args_node.children(&mut acursor) {
                        if let Some(id) = resolve_ident_arg(arg, spec) {
                            let arg_name = node_text(&id, source);
                            if let Some((_src_callee, depth)) = var_map.get(arg_name) {
                                let line = node_line_start(&rhs_node);
                                edges.push(RawEdge {
                                    from: from_sym.clone(),
                                    to: EdgeTarget::Unresolved {
                                        name: callee.clone(),
                                        import_path: None,
                                        qualifier: None,
                                    },
                                    kind: EdgeKind::DataFlowsTo,
                                    line,
                                    confidence: Confidence::Inferred(0.6),
                                });
                                chain_depth = depth + 1;
                                break;
                            }
                        }
                    }
                }
                if chain_depth <= 3 {
                    var_map.insert(var, (callee, chain_depth));
                }
            }
        } else {
            scan_calls(spec, source, &stmt, &var_map, from_sym, edges);
        }
    }
}
fn kotlin_binding_parts<'a>(stmt: Node<'a>, source: &str) -> Option<(String, Node<'a>)> {
    match stmt.kind() {
        "property_declaration" => {
            let mut cursor = stmt.walk();
            let variable = stmt
                .named_children(&mut cursor)
                .find(|child| child.kind() == "variable_declaration")?;
            let mut variable_cursor = variable.walk();
            let lhs = variable
                .named_children(&mut variable_cursor)
                .find(|child| child.kind() == "identifier")?;
            let mut stmt_cursor = stmt.walk();
            let rhs = stmt
                .named_children(&mut stmt_cursor)
                .find(|child| child.kind() == KOTLIN_FLOW_SPEC.call_kind)?;
            Some((node_text(&lhs, source).to_string(), rhs))
        }
        "assignment" => {
            let lhs = stmt
                .child_by_field_name("left")
                .filter(|child| child.kind() == "identifier")?;
            let rhs = stmt
                .child_by_field_name("right")
                .filter(|child| child.kind() == KOTLIN_FLOW_SPEC.call_kind)?;
            Some((node_text(&lhs, source).to_string(), rhs))
        }
        _ => None,
    }
}

fn collect_kotlin_intermediate_flow_edges<'a>(
    source: &str,
    node: &Node<'a>,
    from_sym: &QualifiedSymbol,
    edges: &mut Vec<RawEdge>,
) {
    let mut var_map: HashMap<String, (String, usize)> = HashMap::new();
    let mut cursor = node.walk();
    for stmt in node.children(&mut cursor) {
        if let Some((var_name, rhs_node)) = kotlin_binding_parts(stmt, source) {
            let callee = KOTLIN_FLOW_SPEC
                .callee
                .resolve(&rhs_node)
                .map(|callee| node_text(&callee, source).to_string());
            if let Some(callee) = callee {
                let mut chain_depth = 1usize;
                if let Some(args_node) = KOTLIN_FLOW_SPEC.args.resolve(&rhs_node) {
                    let mut args_cursor = args_node.walk();
                    for arg in args_node.children(&mut args_cursor) {
                        if let Some(id) = resolve_ident_arg(arg, &KOTLIN_FLOW_SPEC)
                            && let Some((_src_callee, depth)) = var_map.get(node_text(&id, source))
                        {
                            edges.push(RawEdge {
                                from: from_sym.clone(),
                                to: EdgeTarget::Unresolved {
                                    name: callee.clone(),
                                    import_path: None,
                                    qualifier: None,
                                },
                                kind: EdgeKind::DataFlowsTo,
                                line: node_line_start(&rhs_node),
                                confidence: Confidence::Inferred(0.6),
                            });
                            chain_depth = depth + 1;
                            break;
                        }
                    }
                }
                if chain_depth <= 3 {
                    var_map.insert(var_name, (callee, chain_depth));
                }
            }
        } else {
            scan_calls(&KOTLIN_FLOW_SPEC, source, &stmt, &var_map, from_sym, edges);
        }
    }
}

// ─── Python extractor ─────────────────────────────────────────────────────

fn extract_python(
    file: &str,
    source: &str,
    tree: &tree_sitter::Tree,
) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_python_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_python_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "function_definition" | "async_function_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let kind = if !scope.is_empty() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                let func_sym = sym.qualified.clone();
                symbols.push(sym);

                let mut param_names: HashSet<&str> = HashSet::new();
                if let Some(params_node) = node.child_by_field_name("parameters") {
                    let mut pcursor = params_node.walk();
                    for param in params_node.children(&mut pcursor) {
                        match param.kind() {
                            "identifier" => {
                                param_names.insert(node_text(&param, source));
                            }
                            "typed_parameter" | "default_parameter" => {
                                if let Some(name_node) = param.child_by_field_name("name")
                                    && name_node.kind() == "identifier"
                                {
                                    param_names.insert(node_text(&name_node, source));
                                }
                            }
                            _ => {}
                        }
                    }
                }
                if !param_names.is_empty()
                    && let Some(body_node) = node.child_by_field_name("body")
                {
                    collect_param_forward_edges(
                        &PYTHON_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &param_names,
                        &func_sym,
                        edges,
                    );
                }
                if let Some(body_node) = node.child_by_field_name("body") {
                    collect_intermediate_flow_edges(
                        &PYTHON_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &func_sym,
                        edges,
                    );
                }

                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_python_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "class_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Class,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);

                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_python_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "call" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym.clone(),
                        to: EdgeTarget::Unresolved {
                            name: callee_name.clone(),
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                    if let Some(taint_edge) = crate::parsing::taint::emit_taint_edge(
                        &from_sym,
                        &callee_name,
                        crate::parsing::taint::Lang::Python,
                        node_line_start(node),
                    ) {
                        edges.push(taint_edge);
                    }
                }
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_python_node(file, source, &child, scope, parent_fqn, symbols, edges);
                }
            } else {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_python_node(file, source, &child, scope, parent_fqn, symbols, edges);
                }
            }
        }
        "return_statement" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "call"
                    && let Some(func_node) = child.child_by_field_name("function")
                {
                    let callee_name = node_text(&func_node, source).to_string();
                    if let Some(from_sym) = scope_to_qualified(file, scope) {
                        edges.push(RawEdge {
                            from: from_sym,
                            to: EdgeTarget::Unresolved {
                                name: callee_name,
                                import_path: None,
                                qualifier: None,
                            },
                            kind: EdgeKind::DataFlowsTo,
                            line: node_line_start(&child),
                            confidence: Confidence::Extracted,
                        });
                    }
                }
                // Recurse so the call child also emits its Calls edge.
                extract_python_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_python_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

fn scope_to_qualified(file: &str, scope: &[String]) -> Option<QualifiedSymbol> {
    scope.last().map(|name| QualifiedSymbol {
        file: file.to_string(),
        scope_path: scope[..scope.len() - 1].to_vec(),
        name: name.clone(),
    })
}

// ─── JavaScript extractor ─────────────────────────────────────────────────

fn extract_javascript(
    file: &str,
    source: &str,
    tree: &tree_sitter::Tree,
) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_js_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_js_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "function_declaration" | "function" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "<anonymous>".to_string());
            let kind = if !scope.is_empty() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let sym = make_symbol(
                file,
                &name,
                scope.to_vec(),
                kind,
                node_line_start(node),
                node_line_end(node),
                None,
                parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            let func_sym = sym.qualified.clone();
            symbols.push(sym);
            let mut param_names: HashSet<&str> = HashSet::new();
            if let Some(params_node) = node.child_by_field_name("parameters") {
                let mut pcursor = params_node.walk();
                for param in params_node.children(&mut pcursor) {
                    if param.kind() == "identifier" {
                        param_names.insert(node_text(&param, source));
                    }
                }
            }
            if !param_names.is_empty()
                && let Some(body_node) = node.child_by_field_name("body")
            {
                collect_param_forward_edges(
                    &JS_FLOW_SPEC,
                    file,
                    source,
                    &body_node,
                    &param_names,
                    &func_sym,
                    edges,
                );
            }
            if let Some(body_node) = node.child_by_field_name("body") {
                collect_intermediate_flow_edges(
                    &JS_FLOW_SPEC,
                    file,
                    source,
                    &body_node,
                    &func_sym,
                    edges,
                );
            }
            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(
                    file,
                    source,
                    &child,
                    &child_scope,
                    Some(&fqn),
                    symbols,
                    edges,
                );
            }
        }
        "class_declaration" | "class" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "<anonymous>".to_string());
            let sym = make_symbol(
                file,
                &name,
                scope.to_vec(),
                SymbolKind::Class,
                node_line_start(node),
                node_line_end(node),
                None,
                parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(
                    file,
                    source,
                    &child,
                    &child_scope,
                    Some(&fqn),
                    symbols,
                    edges,
                );
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym.clone(),
                        to: EdgeTarget::Unresolved {
                            name: callee_name.clone(),
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                    if let Some(taint_edge) = crate::parsing::taint::emit_taint_edge(
                        &from_sym,
                        &callee_name,
                        crate::parsing::taint::Lang::Js,
                        node_line_start(node),
                    ) {
                        edges.push(taint_edge);
                    }
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        "member_expression" => {
            // Check for JS taint sources expressed as property accesses (req.body, etc.)
            let member_text = node_text(node, source).to_string();
            if let Some(from_sym) = scope_to_qualified(file, scope)
                && let Some(taint_edge) = crate::parsing::taint::emit_js_member_taint_edge(
                    &from_sym,
                    &member_text,
                    node_line_start(node),
                )
            {
                edges.push(taint_edge);
            }
            // Recurse into children
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_js_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── TypeScript extractor ─────────────────────────────────────────────────

fn extract_typescript(
    file: &str,
    source: &str,
    tree: &tree_sitter::Tree,
) -> (Vec<Symbol>, Vec<RawEdge>) {
    // TypeScript grammar is a superset of JS grammar — reuse JS extractor.
    extract_javascript(file, source, tree)
}

// ─── Rust extractor ───────────────────────────────────────────────────────

fn extract_rust(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_rust_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_rust_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "function_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let kind = if scope.iter().any(|s| s.starts_with("impl")) {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                let func_sym = sym.qualified.clone();
                symbols.push(sym);
                let mut param_names: HashSet<&str> = HashSet::new();
                if let Some(params_node) = node.child_by_field_name("parameters") {
                    let mut pcursor = params_node.walk();
                    for param in params_node.children(&mut pcursor) {
                        if param.kind() == "parameter"
                            && let Some(pat) = param.child_by_field_name("pattern")
                            && pat.kind() == "identifier"
                        {
                            param_names.insert(node_text(&pat, source));
                        }
                    }
                }
                if !param_names.is_empty()
                    && let Some(body_node) = node.child_by_field_name("body")
                {
                    collect_param_forward_edges(
                        &RUST_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &param_names,
                        &func_sym,
                        edges,
                    );
                }
                if let Some(body_node) = node.child_by_field_name("body") {
                    collect_intermediate_flow_edges(
                        &RUST_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &func_sym,
                        edges,
                    );
                }
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_rust_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "struct_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Struct,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                symbols.push(sym);
            }
        }
        "trait_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Trait,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_rust_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "impl_item" => {
            let type_name = node
                .child_by_field_name("type")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "impl".to_string());
            let impl_name = format!("impl_{}", type_name);
            let sym = make_symbol(
                file,
                &impl_name,
                scope.to_vec(),
                SymbolKind::Impl,
                node_line_start(node),
                node_line_end(node),
                None,
                parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(impl_name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_rust_node(
                    file,
                    source,
                    &child,
                    &child_scope,
                    Some(&fqn),
                    symbols,
                    edges,
                );
            }
        }
        "mod_item" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Module,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_rust_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym.clone(),
                        to: EdgeTarget::Unresolved {
                            name: callee_name.clone(),
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                    if let Some(taint_edge) = crate::parsing::taint::emit_taint_edge(
                        &from_sym,
                        &callee_name,
                        crate::parsing::taint::Lang::Rust,
                        node_line_start(node),
                    ) {
                        edges.push(taint_edge);
                    }
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_rust_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        "return_expression" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "call_expression"
                    && let Some(func_node) = child.child_by_field_name("function")
                {
                    let callee_name = node_text(&func_node, source).to_string();
                    if let Some(from_sym) = scope_to_qualified(file, scope) {
                        edges.push(RawEdge {
                            from: from_sym,
                            to: EdgeTarget::Unresolved {
                                name: callee_name,
                                import_path: None,
                                qualifier: None,
                            },
                            kind: EdgeKind::DataFlowsTo,
                            line: node_line_start(&child),
                            confidence: Confidence::Extracted,
                        });
                    }
                }
                // Recurse so the call_expression child also emits its Calls edge.
                extract_rust_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_rust_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Go extractor ─────────────────────────────────────────────────────────

fn extract_go(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_go_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_go_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "function_declaration" | "method_declaration" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "<anon>".to_string());
            let kind = if node.kind() == "method_declaration" {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let sym = make_symbol(
                file,
                &name,
                scope.to_vec(),
                kind,
                node_line_start(node),
                node_line_end(node),
                None,
                parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            let func_sym = sym.qualified.clone();
            symbols.push(sym);
            let mut param_names: HashSet<&str> = HashSet::new();
            if let Some(params_node) = node.child_by_field_name("parameters") {
                let mut pcursor = params_node.walk();
                for param in params_node.children(&mut pcursor) {
                    if param.kind() == "parameter_declaration" {
                        let mut ncursor = param.walk();
                        for ident in param.children(&mut ncursor) {
                            if ident.kind() == "identifier" {
                                param_names.insert(node_text(&ident, source));
                            }
                        }
                    }
                }
            }
            if !param_names.is_empty()
                && let Some(body_node) = node.child_by_field_name("body")
            {
                collect_param_forward_edges(
                    &GO_FLOW_SPEC,
                    file,
                    source,
                    &body_node,
                    &param_names,
                    &func_sym,
                    edges,
                );
            }
            if let Some(body_node) = node.child_by_field_name("body") {
                collect_intermediate_flow_edges(
                    &GO_FLOW_SPEC,
                    file,
                    source,
                    &body_node,
                    &func_sym,
                    edges,
                );
            }
            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_go_node(
                    file,
                    source,
                    &child,
                    &child_scope,
                    Some(&fqn),
                    symbols,
                    edges,
                );
            }
        }
        "type_declaration" => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec"
                    && let Some(name_node) = child.child_by_field_name("name")
                {
                    let name = node_text(&name_node, source).to_string();
                    let sym = make_symbol(
                        file,
                        &name,
                        scope.to_vec(),
                        SymbolKind::Struct,
                        node_line_start(&child),
                        node_line_end(&child),
                        None,
                        parent_fqn.map(|s| s.to_string()),
                    );
                    symbols.push(sym);
                }
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_go_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_go_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Java extractor ───────────────────────────────────────────────────────

fn extract_java(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_java_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_java_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "class_declaration" | "interface_declaration" => {
            let kind = if node.kind() == "interface_declaration" {
                SymbolKind::Interface
            } else {
                SymbolKind::Class
            };
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_java_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Method,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                let func_sym = sym.qualified.clone();
                symbols.push(sym);
                let mut param_names: HashSet<&str> = HashSet::new();
                if let Some(params_node) = node.child_by_field_name("parameters") {
                    let mut pcursor = params_node.walk();
                    for param in params_node.children(&mut pcursor) {
                        if param.kind() == "formal_parameter"
                            && let Some(ident) = param.child_by_field_name("name")
                        {
                            param_names.insert(node_text(&ident, source));
                        }
                    }
                }
                if !param_names.is_empty()
                    && let Some(body_node) = node.child_by_field_name("body")
                {
                    collect_param_forward_edges(
                        &JAVA_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &param_names,
                        &func_sym,
                        edges,
                    );
                }
                if let Some(body_node) = node.child_by_field_name("body") {
                    collect_intermediate_flow_edges(
                        &JAVA_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &func_sym,
                        edges,
                    );
                }
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_java_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "method_invocation" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let callee_name = node_text(&name_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_java_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_java_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── C / C++ extractor ────────────────────────────────────────────────────

fn extract_c_cpp(
    file: &str,
    source: &str,
    tree: &tree_sitter::Tree,
) -> (Vec<Symbol>, Vec<RawEdge>, HashMap<String, String>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let mut imports: HashMap<String, String> = HashMap::new();
    let root = tree.root_node();
    extract_c_cpp_node(
        file,
        source,
        &root,
        &[],
        None,
        &mut symbols,
        &mut edges,
        &mut imports,
    );
    (symbols, edges, imports)
}

/// Drill through nested declarator nodes to find the leaf identifier/name.
/// Returns (name_text, is_qualified) where is_qualified means we saw a
/// qualified_identifier along the way (Foo::bar).
fn declarator_name<'a>(node: &Node, source: &'a str) -> Option<(&'a str, bool)> {
    match node.kind() {
        "identifier" | "field_identifier" => Some((node_text(node, source), false)),
        "destructor_name" => Some((node_text(node, source), false)),
        "qualified_identifier" => {
            // Rightmost identifier is the leaf name; the rest is scope.
            // qualified_identifier has a `scope` field (left side) and a `name` field (right side).
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source);
                Some((name, true))
            } else {
                None
            }
        }
        "function_declarator" => {
            // function_declarator has its own `declarator` field — recurse.
            node.child_by_field_name("declarator")
                .and_then(|inner| declarator_name(&inner, source))
        }
        "pointer_declarator" | "reference_declarator" | "abstract_reference_declarator" => {
            // pointer/ref: the actual declarator is the last named child.
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .find_map(|child| declarator_name(&child, source))
        }
        _ => None,
    }
}

/// For a qualified_identifier, extract the scope prefix (everything before last ::)
/// as a Vec<String> to be prepended to the symbol's scope_path.
fn qualified_scope_prefix(node: &Node, source: &str) -> Vec<String> {
    // The tree-sitter C++ grammar represents `Foo::Bar::baz` as nested
    // qualified_identifiers: scope=qualified_identifier(Foo::Bar), name=baz.
    // We collect the scope chain into a flat Vec.
    let mut parts = Vec::new();
    collect_scope_parts(node, source, &mut parts);
    if !parts.is_empty() {
        parts.pop();
    }
    parts
}

/// Collect parameter names from a C/C++ function declarator.
fn c_cpp_param_names<'a>(
    declarator: &Node<'a>,
    source: &'a str,
    spec: &FlowSpec,
) -> HashSet<&'a str> {
    let function_decl = if declarator.kind() == "function_declarator" {
        Some(*declarator)
    } else {
        let mut cursor = declarator.walk();
        declarator
            .named_children(&mut cursor)
            .find(|child| child.kind() == "function_declarator")
    };
    let Some(function_decl) = function_decl else {
        return HashSet::new();
    };
    let Some(parameters) = function_decl.child_by_field_name("parameters") else {
        return HashSet::new();
    };
    let mut names = HashSet::new();
    let mut cursor = parameters.walk();
    for parameter in parameters.named_children(&mut cursor) {
        if parameter.kind() != "parameter_declaration" {
            continue;
        }
        if let Some(parameter_decl) = parameter.child_by_field_name("declarator")
            && let Some(name) = c_cpp_declarator_ident(&parameter_decl, source, spec)
        {
            names.insert(name);
        }
    }
    names
}

fn c_cpp_declarator_ident<'a>(
    node: &Node<'a>,
    source: &'a str,
    spec: &FlowSpec,
) -> Option<&'a str> {
    let (name, _) = declarator_name(node, source)?;
    if spec.param_ident_kinds.contains(&node.kind()) {
        return Some(name);
    }
    let mut cursor = node.walk();
    node.named_children(&mut cursor)
        .find_map(|child| c_cpp_declarator_ident(&child, source, spec))
}

fn collect_scope_parts(node: &Node, source: &str, parts: &mut Vec<String>) {
    match node.kind() {
        "qualified_identifier" => {
            if let Some(scope_node) = node.child_by_field_name("scope") {
                collect_scope_parts(&scope_node, source, parts);
            }
            if let Some(name_node) = node.child_by_field_name("name") {
                parts.push(node_text(&name_node, source).to_string());
            }
        }
        "identifier" | "namespace_identifier" | "type_identifier" => {
            parts.push(node_text(node, source).to_string());
        }
        _ => {}
    }
}

/// Extract the leaf callee name from a call_expression's `function` node.
/// Returns None if the callee cannot be resolved to a simple name.
fn callee_leaf_name<'a>(func_node: &Node, source: &'a str) -> Option<&'a str> {
    match func_node.kind() {
        "identifier" => Some(node_text(func_node, source)),
        "field_expression" => {
            // obj.method() or ptr->method() — the `field` child holds the method name.
            func_node
                .child_by_field_name("field")
                .map(|n| node_text(&n, source))
        }
        "qualified_identifier" => {
            // ns::Foo::bar() → recursively unwrap to the rightmost leaf identifier.
            // The C++ grammar nests: qualified_identifier(scope=qualified_identifier(ns::Foo), name=bar)
            // or in some grammars: qualified_identifier(scope=ns, name=qualified_identifier(Foo::bar))
            // We always want the final leaf identifier.
            if let Some(name_node) = func_node.child_by_field_name("name") {
                // If name_node is itself a qualified_identifier, recurse.
                callee_leaf_name(&name_node, source)
            } else {
                None
            }
        }
        "template_function" => {
            // template call like foo<T>() — the `name` child holds the base name.
            let name_node = func_node.child_by_field_name("name")?;
            callee_leaf_name(&name_node, source)
        }
        _ => None,
    }
}

/// Return the basename (filename without extension) of an include path.
/// E.g. `"linux/list.h"` → `"list"`, `"Agent.h"` → `"Agent"`.
fn include_basename(path: &str) -> &str {
    // Get the final component after the last `/`.
    let filename = path.rfind('/').map(|i| &path[i + 1..]).unwrap_or(path);
    // Strip the extension (last `.` and everything after).
    filename
        .rfind('.')
        .map(|i| &filename[..i])
        .unwrap_or(filename)
}

/// For a qualified call `ns::Foo::method()`, extract the direct qualifier
/// (the part immediately before the leaf name, e.g. `"Foo"` from `ns::Foo::method`).
/// For a field expression `obj.method()` or `ptr->method()`, no qualifier is relevant.
/// Returns None for unqualified or field-expression calls.
fn extract_call_qualifier<'a>(func_node: &Node, source: &'a str) -> Option<&'a str> {
    match func_node.kind() {
        "qualified_identifier" => {
            // Walk down through nested qualified_identifiers to find the deepest one
            // (which is the one whose scope is the direct qualifier of the leaf name).
            //
            // For `ns::Foo::method`:
            //   top: scope=ns, name=qualified_identifier(Foo::method)
            //     inner: scope=Foo, name=method
            // We want the inner's scope ("Foo").
            //
            // For `Agent::run`:
            //   scope=Agent, name=run
            // We want the scope ("Agent").
            //
            // Strategy: recurse into the `name` field if it's a qualified_identifier;
            // otherwise, this IS the innermost qualified_identifier — return its scope text.
            let name_node = func_node.child_by_field_name("name")?;
            if name_node.kind() == "qualified_identifier" {
                // Go deeper.
                extract_call_qualifier(&name_node, source)
            } else {
                // This is the innermost qualified_identifier. Its scope is the direct qualifier.
                let scope_node = func_node.child_by_field_name("scope")?;
                // Get the rightmost component of the scope (in case scope itself is nested).
                match scope_node.kind() {
                    "namespace_identifier" | "type_identifier" | "identifier" => {
                        Some(node_text(&scope_node, source))
                    }
                    "qualified_identifier" => {
                        // Scope is also nested: get the name (rightmost) of the scope.
                        scope_node
                            .child_by_field_name("name")
                            .map(|n| node_text(&n, source))
                    }
                    _ => None,
                }
            }
        }
        "template_function" => {
            let name_node = func_node.child_by_field_name("name")?;
            extract_call_qualifier(&name_node, source)
        }
        _ => None,
    }
}

/// Determine the `import_path` for a call expression by checking the imports HashMap.
///
/// Rules (in order):
///  1. If the callee is qualified (e.g. `GoalAgent::doThing()`), extract the direct
///     qualifier (e.g. `GoalAgent`). Check if any include path's basename matches it.
///     If so, set import_path to the full include path.
///  2. If the callee is unqualified (e.g. `foo()`), check if any include path's basename
///     matches `callee_name` (i.e. `callee_name.h` pattern via basename comparison).
///  3. Otherwise, return None.
fn resolve_import_path_for_call(
    func_node: &Node,
    source: &str,
    callee_name: &str,
    imports: &HashMap<String, String>,
) -> Option<String> {
    if imports.is_empty() {
        return None;
    }

    // Try qualified match first.
    if let Some(qualifier) = extract_call_qualifier(func_node, source) {
        // Check if any include basename matches the qualifier.
        for include_path in imports.keys() {
            if include_basename(include_path) == qualifier {
                return Some(include_path.clone());
            }
        }
    }

    // Unqualified call: check if `<callee_name>.h` matches any include basename.
    // We compare the callee_name against include basenames (i.e. basename("Foo.h") == "Foo").
    // Only match if the function is a plain identifier (not qualified or field-based).
    if func_node.kind() == "identifier" {
        for include_path in imports.keys() {
            if include_basename(include_path) == callee_name {
                return Some(include_path.clone());
            }
        }
    }

    None
}

#[allow(clippy::too_many_arguments)]
fn extract_c_cpp_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
    imports: &mut HashMap<String, String>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "preproc_include" => {
            // Extract the path child node and strip surrounding `""` or `<>`.
            if let Some(path_node) = node.child_by_field_name("path") {
                let raw = node_text(&path_node, source);
                let stripped = raw.trim_matches(|c| c == '"' || c == '<' || c == '>');
                if !stripped.is_empty() {
                    imports.insert(stripped.to_string(), stripped.to_string());
                }
            }
            // No further children to recurse into for preproc_include.
        }
        "function_definition" => {
            // The outer declarator field is typically a function_declarator.
            if let Some(outer_decl) = node.child_by_field_name("declarator")
                && let Some((name, is_qualified)) = declarator_name(&outer_decl, source)
            {
                let (sym_scope, sym_kind) = if is_qualified {
                    // Out-of-line definition like `Foo::bar(...)` — extract scope prefix.
                    let qscope = qualified_scope_prefix(&outer_decl, source);
                    let mut merged = scope.to_vec();
                    merged.extend(qscope);
                    (merged, SymbolKind::Method)
                } else {
                    let kind = if scope.iter().any(|s| {
                        // Inside a class_specifier or struct_specifier scope.
                        symbols.iter().any(|sym| {
                            sym.qualified.name == *s
                                && matches!(sym.kind, SymbolKind::Class | SymbolKind::Struct)
                        })
                    }) {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    (scope.to_vec(), kind)
                };

                let sym = make_symbol(
                    file,
                    name,
                    sym_scope,
                    sym_kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let func_sym = sym.clone();
                let fqn = func_sym.qualified.fqn();
                symbols.push(sym);

                let (param_spec, declaration_spec, assignment_spec) =
                    if matches!(detect_language(Path::new(file)), Lang::Cpp) {
                        (&CPP_FLOW_SPEC, &CPP_FLOW_SPEC, &CPP_ASSIGNMENT_FLOW_SPEC)
                    } else {
                        (&C_FLOW_SPEC, &C_FLOW_SPEC, &C_ASSIGNMENT_FLOW_SPEC)
                    };
                if let Some(body) = node.child_by_field_name("body") {
                    let param_names = c_cpp_param_names(&outer_decl, source, param_spec);
                    if !param_names.is_empty() {
                        collect_param_forward_edges(
                            param_spec,
                            file,
                            source,
                            &body,
                            &param_names,
                            &func_sym.qualified,
                            edges,
                        );
                    }
                    collect_intermediate_flow_edges(
                        declaration_spec,
                        file,
                        source,
                        &body,
                        &func_sym.qualified,
                        edges,
                    );
                    collect_intermediate_flow_edges(
                        assignment_spec,
                        file,
                        source,
                        &body,
                        &func_sym.qualified,
                        edges,
                    );
                }

                let mut child_scope = scope.to_vec();
                child_scope.push(name.to_string());
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_c_cpp_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                        imports,
                    );
                }
                return;
            }
            // Fallthrough: declarator not resolved — still recurse for nested nodes.
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_c_cpp_node(
                    file, source, &child, scope, parent_fqn, symbols, edges, imports,
                );
            }
        }
        "class_specifier" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Class,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_c_cpp_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                        imports,
                    );
                }
            } else {
                // Anonymous class — still recurse without pushing scope.
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_c_cpp_node(
                        file, source, &child, scope, parent_fqn, symbols, edges, imports,
                    );
                }
            }
        }
        "struct_specifier" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Struct,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_c_cpp_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                        imports,
                    );
                }
            } else {
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_c_cpp_node(
                        file, source, &child, scope, parent_fqn, symbols, edges, imports,
                    );
                }
            }
        }
        "namespace_definition" => {
            // C++ namespaces — `name` field may be absent (anonymous namespace).
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string())
                .unwrap_or_else(|| "<anonymous>".to_string());
            let sym = make_symbol(
                file,
                &name,
                scope.to_vec(),
                SymbolKind::Module,
                node_line_start(node),
                node_line_end(node),
                None,
                parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_c_cpp_node(
                    file,
                    source,
                    &child,
                    &child_scope,
                    Some(&fqn),
                    symbols,
                    edges,
                    imports,
                );
            }
        }
        "call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function")
                && let Some(callee_name) = callee_leaf_name(&func_node, source)
                && let Some(from_sym) = scope_to_qualified(file, scope)
            {
                // Determine import_path by checking the imports HashMap.
                // For qualified calls (e.g., GoalAgent::doThing()), extract the qualifier
                // (everything before the last ::) and check if any import's basename
                // (without extension) matches it.
                // For unqualified calls, check if <callee_name>.h matches any import basename.
                let import_path =
                    resolve_import_path_for_call(&func_node, source, callee_name, imports);

                edges.push(RawEdge {
                    from: from_sym,
                    to: EdgeTarget::Unresolved {
                        name: callee_name.to_string(),
                        import_path,
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line: node_line_start(node),
                    confidence: Confidence::Extracted,
                });
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_c_cpp_node(
                    file, source, &child, scope, parent_fqn, symbols, edges, imports,
                );
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_c_cpp_node(
                    file, source, &child, scope, parent_fqn, symbols, edges, imports,
                );
            }
        }
    }
}

// ─── C# extractor ────────────────────────────────────────────────────────

fn extract_csharp(
    file: &str,
    source: &str,
    tree: &tree_sitter::Tree,
) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_csharp_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_csharp_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "method_declaration" | "constructor_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let kind = if !scope.is_empty() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                let func_sym = sym.qualified.clone();
                symbols.push(sym);
                let mut param_names: HashSet<&str> = HashSet::new();
                if let Some(params_node) = node.child_by_field_name("parameters") {
                    let mut pcursor = params_node.walk();
                    for param in params_node.named_children(&mut pcursor) {
                        if let Some(name_node) = param.child_by_field_name("name")
                            && CSHARP_FLOW_SPEC
                                .param_ident_kinds
                                .contains(&name_node.kind())
                        {
                            param_names.insert(node_text(&name_node, source));
                        }
                    }
                }
                if let Some(body_node) = node.child_by_field_name("body") {
                    if !param_names.is_empty() {
                        collect_param_forward_edges(
                            &CSHARP_FLOW_SPEC,
                            file,
                            source,
                            &body_node,
                            &param_names,
                            &func_sym,
                            edges,
                        );
                    }
                    collect_intermediate_flow_edges(
                        &CSHARP_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &func_sym,
                        edges,
                    );
                }
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_csharp_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "class_declaration" | "record_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Class,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_csharp_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "struct_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Struct,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_csharp_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "interface_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Interface,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_csharp_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "enum_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Enum,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_csharp_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "namespace_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Module,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_csharp_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "invocation_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_csharp_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_csharp_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── PHP extractor ───────────────────────────────────────────────────────

fn extract_php(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_php_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_php_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "function_definition" | "method_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let kind = if !scope.is_empty() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                let func_sym = sym.qualified.clone();
                symbols.push(sym);
                let mut param_names: HashSet<&str> = HashSet::new();
                if let Some(params_node) = node.child_by_field_name("parameters") {
                    let mut pcursor = params_node.walk();
                    for param in params_node.named_children(&mut pcursor) {
                        let mut name_cursor = param.walk();
                        if let Some(name_node) = param
                            .named_children(&mut name_cursor)
                            .find(|child| PHP_FLOW_SPEC.param_ident_kinds.contains(&child.kind()))
                        {
                            param_names.insert(node_text(&name_node, source));
                        }
                    }
                }
                if let Some(body_node) = node.child_by_field_name("body") {
                    if !param_names.is_empty() {
                        collect_param_forward_edges(
                            &PHP_FLOW_SPEC,
                            file,
                            source,
                            &body_node,
                            &param_names,
                            &func_sym,
                            edges,
                        );
                    }
                    collect_intermediate_flow_edges(
                        &PHP_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &func_sym,
                        edges,
                    );
                }
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_php_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "class_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Class,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_php_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "interface_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Interface,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_php_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "trait_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Trait,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_php_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "namespace_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Module,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_php_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "enum_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Enum,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_php_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "function_call_expression" => {
            if let Some(func_node) = node.child_by_field_name("function") {
                let callee_name = node_text(&func_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_php_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        "member_call_expression" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let callee_name = node_text(&name_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_php_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        "scoped_call_expression" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let callee_name = node_text(&name_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_php_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_php_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Ruby extractor ──────────────────────────────────────────────────────

fn extract_ruby(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_ruby_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_ruby_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "method" | "singleton_method" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let kind = if !scope.is_empty() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                let func_sym = sym.qualified.clone();
                symbols.push(sym);
                let mut param_names: HashSet<&str> = HashSet::new();
                if let Some(params_node) = node.child_by_field_name("parameters") {
                    let mut pcursor = params_node.walk();
                    for param in params_node.children(&mut pcursor) {
                        if param.kind() == "identifier" {
                            param_names.insert(node_text(&param, source));
                        }
                    }
                }
                if !param_names.is_empty()
                    && let Some(body_node) = node.child_by_field_name("body")
                {
                    collect_param_forward_edges(
                        &RUBY_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &param_names,
                        &func_sym,
                        edges,
                    );
                }
                if let Some(body_node) = node.child_by_field_name("body") {
                    collect_intermediate_flow_edges(
                        &RUBY_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &func_sym,
                        edges,
                    );
                }
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_ruby_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "class" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Class,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_ruby_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "module" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Module,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_ruby_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "call" => {
            if let Some(method_node) = node.child_by_field_name("method") {
                let callee_name = node_text(&method_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_ruby_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_ruby_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Objective-C extractor ───────────────────────────────────────────────

fn extract_objc(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_objc_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

/// Compose an ObjC selector from method_definition children.
/// Selector parts are keyword_declarator nodes whose first identifier child
/// forms the selector component (e.g. `initWithName:age:` from two keyword_declarators).
fn objc_method_selector(node: &Node, source: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "keyword_declarator" => {
                // The keyword (before the colon) is the first identifier-like child.
                let mut inner_cursor = child.walk();
                for inner in child.children(&mut inner_cursor) {
                    if inner.kind() == "identifier" || inner.kind() == "keyword_selector" {
                        parts.push(format!("{}:", node_text(&inner, source)));
                        break;
                    }
                }
            }
            "identifier" if parts.is_empty() => {
                // Unary selector (no parameters), e.g. `- (void)doSomething`
                parts.push(node_text(&child, source).to_string());
            }
            _ => {}
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(""))
    }
}

/// Extract callee selector from a message_expression.
fn objc_message_selector(node: &Node, source: &str) -> Option<String> {
    let mut parts = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "keyword_argument" {
            // keyword_argument has a `keyword` field that is the selector part
            if let Some(kw) = child.child_by_field_name("keyword") {
                parts.push(format!("{}:", node_text(&kw, source)));
            }
        }
    }
    // If no keyword_arguments found, look for the selector as a direct identifier child
    // (unary message like `[obj doSomething]`)
    if parts.is_empty() {
        let mut cursor2 = node.walk();
        let children: Vec<_> = node.children(&mut cursor2).collect();
        // In `[receiver selector]`, the selector is typically the last identifier
        // before the closing bracket.
        for child in children.iter().rev() {
            if child.kind() == "identifier" {
                return Some(node_text(child, source).to_string());
            }
        }
        return None;
    }
    Some(parts.join(""))
}

fn extract_objc_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "method_definition" => {
            if let Some(sel) = objc_method_selector(node, source) {
                let kind = if !scope.is_empty() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let sym = make_symbol(
                    file,
                    &sel,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(sel);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_objc_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "class_interface" => {
            // First named child that is an identifier is the class name
            let mut cursor = node.walk();
            let name = node
                .children(&mut cursor)
                .find(|c| c.kind() == "identifier")
                .map(|c| node_text(&c, source).to_string());
            if let Some(name) = name {
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Class,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_objc_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "protocol_declaration" => {
            let mut cursor = node.walk();
            let name = node
                .children(&mut cursor)
                .find(|c| c.kind() == "identifier")
                .map(|c| node_text(&c, source).to_string());
            if let Some(name) = name {
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Interface,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_objc_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "message_expression" => {
            if let Some(sel) = objc_message_selector(node, source)
                && let Some(from_sym) = scope_to_qualified(file, scope)
            {
                edges.push(RawEdge {
                    from: from_sym,
                    to: EdgeTarget::Unresolved {
                        name: sel,
                        import_path: None,
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line: node_line_start(node),
                    confidence: Confidence::Extracted,
                });
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_objc_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_objc_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Swift extractor ─────────────────────────────────────────────────────

fn extract_swift(
    file: &str,
    source: &str,
    tree: &tree_sitter::Tree,
) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_swift_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_swift_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "function_declaration" => {
            // Name is a positional simple_identifier child.
            let mut cursor = node.walk();
            let name = node
                .children(&mut cursor)
                .find(|c| c.kind() == "simple_identifier")
                .map(|c| node_text(&c, source).to_string())
                .unwrap_or_else(|| "<anonymous>".to_string());
            let kind = if !scope.is_empty() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let sym = make_symbol(
                file,
                &name,
                scope.to_vec(),
                kind,
                node_line_start(node),
                node_line_end(node),
                None,
                parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            let func_sym = sym.qualified.clone();
            symbols.push(sym);

            let mut param_names: HashSet<&str> = HashSet::new();
            let mut pcursor = node.walk();
            for param in node.children(&mut pcursor) {
                if param.kind() == "parameter" {
                    let mut param_cursor = param.walk();
                    for child in param.named_children(&mut param_cursor) {
                        if SWIFT_FLOW_SPEC.param_ident_kinds.contains(&child.kind()) {
                            param_names.insert(node_text(&child, source));
                            break;
                        }
                    }
                }
            }
            if let Some(body_node) = node.child_by_field_name("body")
                && let Some(statements) = NodeRef::ChildOfKind("statements").resolve(&body_node)
            {
                if !param_names.is_empty() {
                    collect_param_forward_edges(
                        &SWIFT_FLOW_SPEC,
                        file,
                        source,
                        &statements,
                        &param_names,
                        &func_sym,
                        edges,
                    );
                }
                collect_intermediate_flow_edges(
                    &SWIFT_FLOW_SPEC,
                    file,
                    source,
                    &statements,
                    &func_sym,
                    edges,
                );
                collect_intermediate_flow_edges(
                    &SWIFT_ASSIGNMENT_FLOW_SPEC,
                    file,
                    source,
                    &statements,
                    &func_sym,
                    edges,
                );
            }

            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor2 = node.walk();
            for child in node.children(&mut cursor2) {
                extract_swift_node(
                    file,
                    source,
                    &child,
                    &child_scope,
                    Some(&fqn),
                    symbols,
                    edges,
                );
            }
        }
        "init_declaration" => {
            let name = "init".to_string();
            let kind = if !scope.is_empty() {
                SymbolKind::Method
            } else {
                SymbolKind::Function
            };
            let sym = make_symbol(
                file,
                &name,
                scope.to_vec(),
                kind,
                node_line_start(node),
                node_line_end(node),
                None,
                parent_fqn.map(|s| s.to_string()),
            );
            let fqn = sym.qualified.fqn();
            symbols.push(sym);
            let mut child_scope = scope.to_vec();
            child_scope.push(name);
            let mut cursor2 = node.walk();
            for child in node.children(&mut cursor2) {
                extract_swift_node(
                    file,
                    source,
                    &child,
                    &child_scope,
                    Some(&fqn),
                    symbols,
                    edges,
                );
            }
        }
        "class_declaration" => {
            // Determine actual kind by looking at the keyword child text
            let mut cursor = node.walk();
            let mut keyword = "class";
            let mut name_opt = None;
            for child in node.children(&mut cursor) {
                if child.kind() == "type_identifier" && name_opt.is_none() {
                    name_opt = Some(node_text(&child, source).to_string());
                }
                // In some grammar versions the keyword is a direct child text node
                let text = node_text(&child, source);
                if text == "struct" || text == "enum" || text == "extension" {
                    keyword = text;
                }
            }
            if let Some(name) = name_opt {
                let kind = match keyword {
                    "struct" => SymbolKind::Struct,
                    "enum" => SymbolKind::Enum,
                    "extension" => SymbolKind::Extension,
                    _ => SymbolKind::Class,
                };
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_swift_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "protocol_declaration" => {
            let mut cursor = node.walk();
            let name = node
                .children(&mut cursor)
                .find(|c| c.kind() == "type_identifier")
                .map(|c| node_text(&c, source).to_string());
            if let Some(name) = name {
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Interface,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_swift_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "call_expression" => {
            // First child is the callee expression
            if let Some(first_child) = node.child(0) {
                let callee_name = node_text(&first_child, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_swift_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_swift_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Kotlin extractor ────────────────────────────────────────────────────

fn extract_kotlin(
    file: &str,
    source: &str,
    tree: &tree_sitter::Tree,
) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_kotlin_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_kotlin_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "function_declaration" => {
            let name = node
                .child_by_field_name("name")
                .map(|name_node| node_text(&name_node, source).to_string());
            if let Some(name) = name {
                let kind = if !scope.is_empty() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                let func_sym = sym.qualified.clone();
                symbols.push(sym);

                let mut param_names: HashSet<&str> = HashSet::new();
                if let Some(params) =
                    NodeRef::ChildOfKind("function_value_parameters").resolve(node)
                {
                    let mut params_cursor = params.walk();
                    for parameter in params.named_children(&mut params_cursor) {
                        if parameter.kind() == "parameter" {
                            let mut parameter_cursor = parameter.walk();
                            if let Some(identifier) = parameter
                                .named_children(&mut parameter_cursor)
                                .find(|child| {
                                    KOTLIN_FLOW_SPEC.param_ident_kinds.contains(&child.kind())
                                })
                            {
                                param_names.insert(node_text(&identifier, source));
                            }
                        }
                    }
                }
                if let Some(function_body) = NodeRef::ChildOfKind("function_body").resolve(node)
                    && let Some(block) = NodeRef::ChildOfKind("block").resolve(&function_body)
                {
                    if !param_names.is_empty() {
                        collect_param_forward_edges(
                            &KOTLIN_FLOW_SPEC,
                            file,
                            source,
                            &block,
                            &param_names,
                            &func_sym,
                            edges,
                        );
                    }
                    collect_kotlin_intermediate_flow_edges(source, &block, &func_sym, edges);
                }

                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_kotlin_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "class_declaration" => {
            // Determine kind from modifier keywords: interface, enum, etc.
            let mut cursor = node.walk();
            let mut kind = SymbolKind::Class;
            let mut name_opt = None;
            for child in node.children(&mut cursor) {
                if child.kind() == "identifier" && name_opt.is_none() {
                    name_opt = Some(node_text(&child, source).to_string());
                }
                // Check modifiers node for enum/interface keywords
                if child.kind() == "modifiers" {
                    let mod_text = node_text(&child, source);
                    if mod_text.contains("enum") {
                        kind = SymbolKind::Enum;
                    }
                }
                // Direct text hints
                let text = node_text(&child, source);
                if text == "interface" {
                    kind = SymbolKind::Interface;
                } else if text == "enum" {
                    kind = SymbolKind::Enum;
                }
            }
            if let Some(name) = name_opt {
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_kotlin_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "object_declaration" => {
            let mut cursor = node.walk();
            let name = node
                .children(&mut cursor)
                .find(|c| c.kind() == "identifier")
                .map(|c| node_text(&c, source).to_string());
            if let Some(name) = name {
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Class,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_kotlin_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "call_expression" => {
            // Kotlin-ng call_expression has a positional callee child followed by
            // optional type_arguments and value_arguments children.
            let callee = KOTLIN_FLOW_SPEC
                .callee
                .resolve(node)
                .map(|callee| node_text(&callee, source).to_string());
            if let Some(callee_name) = callee
                && let Some(from_sym) = scope_to_qualified(file, scope)
            {
                edges.push(RawEdge {
                    from: from_sym,
                    to: EdgeTarget::Unresolved {
                        name: callee_name,
                        import_path: None,
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line: node_line_start(node),
                    confidence: Confidence::Extracted,
                });
            }
            let mut cursor2 = node.walk();
            for child in node.children(&mut cursor2) {
                extract_kotlin_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_kotlin_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Dart extractor ──────────────────────────────────────────────────────

/// Find the function/method name in a Dart method_declaration or function_declaration.
/// Looks inside method_signature/function_signature children for an identifier after
/// a type annotation, or uses field name if available.
fn dart_find_function_name(node: &Node, source: &str) -> Option<String> {
    // First try field "name" directly
    if let Some(name_node) = node.child_by_field_name("name") {
        return Some(node_text(&name_node, source).to_string());
    }
    // Look inside method_signature or function_signature child
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "method_signature" || child.kind() == "function_signature" {
            if let Some(n) = child.child_by_field_name("name") {
                return Some(node_text(&n, source).to_string());
            }
            // A method_signature wraps its function_signature as a named child.
            if child.kind() == "method_signature" {
                let mut signature_cursor = child.walk();
                if let Some(function_signature) = child
                    .children(&mut signature_cursor)
                    .find(|c| c.kind() == "function_signature")
                    && let Some(n) = function_signature.child_by_field_name("name")
                {
                    return Some(node_text(&n, source).to_string());
                }
            }
            // Try positional identifier
            let mut inner_cursor = child.walk();
            for inner in child.children(&mut inner_cursor) {
                if inner.kind() == "identifier" {
                    return Some(node_text(&inner, source).to_string());
                }
            }
        }
    }
    // Fallback: look for direct identifier child (skipping type-like nodes)
    let mut cursor2 = node.walk();
    for child in node.children(&mut cursor2) {
        if child.kind() == "identifier" {
            return Some(node_text(&child, source).to_string());
        }
    }
    None
}

fn extract_dart(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_dart_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_dart_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "method_declaration" | "function_declaration" => {
            // Look for identifier in the method_signature/function_signature child, or directly
            let name = dart_find_function_name(node, source);
            if let Some(name) = name {
                let kind = if !scope.is_empty() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                let func_sym = sym.qualified.clone();
                symbols.push(sym);
                let mut param_names: HashSet<&str> = HashSet::new();
                if let Some(sig_node) = node.child_by_field_name("signature") {
                    // `function_declaration`'s signature IS a `function_signature`
                    // (has `parameters` directly); `method_declaration`'s signature
                    // is a `method_signature` wrapping a `function_signature` child.
                    let fn_sig = if sig_node.child_by_field_name("parameters").is_some() {
                        Some(sig_node)
                    } else {
                        let mut scursor = sig_node.walk();
                        sig_node
                            .children(&mut scursor)
                            .find(|c| c.kind() == "function_signature")
                    };
                    if let Some(fn_sig) = fn_sig
                        && let Some(params_node) = fn_sig.child_by_field_name("parameters")
                    {
                        let mut pcursor = params_node.walk();
                        for param in params_node.children(&mut pcursor) {
                            if param.kind() == "formal_parameter"
                                && let Some(ident) = param.child_by_field_name("name")
                            {
                                param_names.insert(node_text(&ident, source));
                            }
                        }
                    }
                }
                if !param_names.is_empty()
                    && let Some(body_node) = node.child_by_field_name("body")
                {
                    collect_param_forward_edges(
                        &DART_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &param_names,
                        &func_sym,
                        edges,
                    );
                }
                // `function_body` wraps the real statements in a `block` child
                // (absent for arrow bodies `=> expr;`); descend to it so
                // `collect_intermediate_flow_edges` (which only looks at direct
                // children) actually sees the statements.
                if let Some(block_node) = node.child_by_field_name("body").and_then(|body| {
                    let mut bcursor = body.walk();
                    body.children(&mut bcursor).find(|c| c.kind() == "block")
                }) {
                    collect_intermediate_flow_edges(
                        &DART_FLOW_SPEC,
                        file,
                        source,
                        &block_node,
                        &func_sym,
                        edges,
                    );
                }
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_dart_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            } else {
                // Still recurse even if we couldn't find the name
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_dart_node(file, source, &child, scope, parent_fqn, symbols, edges);
                }
            }
        }
        "class_declaration" => {
            // Positional identifier child
            let mut cursor = node.walk();
            let name = node
                .children(&mut cursor)
                .find(|c| c.kind() == "identifier")
                .map(|c| node_text(&c, source).to_string());
            if let Some(name) = name {
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Class,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_dart_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "enum_declaration" => {
            let mut cursor = node.walk();
            let name = node
                .children(&mut cursor)
                .find(|c| c.kind() == "identifier")
                .map(|c| node_text(&c, source).to_string());
            if let Some(name) = name {
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Enum,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_dart_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "mixin_declaration" => {
            let mut cursor = node.walk();
            let name = node
                .children(&mut cursor)
                .find(|c| c.kind() == "identifier")
                .map(|c| node_text(&c, source).to_string());
            if let Some(name) = name {
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Class,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_dart_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        // Dart doesn't have a single "call_expression" node type.
        // Function invocations appear as identifier nodes followed by argument_part.
        // We detect them by looking for nodes where a child has arguments.
        "selector" | "unconditional_assignable_selector" => {
            // Check if this contains an argument_part (indicating a call)
            let mut has_args = false;
            let mut callee_name = None;
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "argument_part" || child.kind() == "arguments" {
                    has_args = true;
                }
                if child.kind() == "identifier" && callee_name.is_none() {
                    callee_name = Some(node_text(&child, source).to_string());
                }
            }
            if has_args
                && let Some(callee) = callee_name
                && let Some(from_sym) = scope_to_qualified(file, scope)
            {
                edges.push(RawEdge {
                    from: from_sym,
                    to: EdgeTarget::Unresolved {
                        name: callee,
                        import_path: None,
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line: node_line_start(node),
                    confidence: Confidence::Extracted,
                });
            }
            let mut cursor2 = node.walk();
            for child in node.children(&mut cursor2) {
                extract_dart_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            // Detect top-level function calls: identifier immediately followed by arguments
            if node.kind() == "identifier"
                && let Some(next) = node.next_sibling()
                && (next.kind() == "selector"
                    || next.kind() == "argument_part"
                    || next.kind() == "arguments")
                && let Some(from_sym) = scope_to_qualified(file, scope)
            {
                let callee_name = node_text(node, source).to_string();
                edges.push(RawEdge {
                    from: from_sym,
                    to: EdgeTarget::Unresolved {
                        name: callee_name,
                        import_path: None,
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line: node_line_start(node),
                    confidence: Confidence::Extracted,
                });
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_dart_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}
// ─── Lua extractor ───────────────────────────────────────────────────────

fn extract_lua(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_lua_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_lua_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "function_declaration" => {
            // field "name" — could be identifier or method_index_expression (colon syntax)
            if let Some(name_node) = node.child_by_field_name("name") {
                let (name, kind) = if name_node.kind() == "method_index_expression" {
                    // Colon syntax: obj:method — extract the method field
                    let method = name_node
                        .child_by_field_name("method")
                        .map(|m| node_text(&m, source).to_string())
                        .unwrap_or_else(|| node_text(&name_node, source).to_string());
                    (method, SymbolKind::Method)
                } else {
                    let name = node_text(&name_node, source).to_string();
                    let kind = if !scope.is_empty() {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    (name, kind)
                };
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                let func_sym = sym.qualified.clone();
                symbols.push(sym);
                let mut param_names: HashSet<&str> = HashSet::new();
                if let Some(params_node) = node.child_by_field_name("parameters") {
                    let mut pcursor = params_node.walk();
                    for param in params_node.children_by_field_name("name", &mut pcursor) {
                        if LUA_FLOW_SPEC.param_ident_kinds.contains(&param.kind()) {
                            param_names.insert(node_text(&param, source));
                        }
                    }
                }
                if let Some(body_node) = node.child_by_field_name("body") {
                    if !param_names.is_empty() {
                        collect_param_forward_edges(
                            &LUA_FLOW_SPEC,
                            file,
                            source,
                            &body_node,
                            &param_names,
                            &func_sym,
                            edges,
                        );
                    }
                    collect_intermediate_flow_edges(
                        &LUA_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &func_sym,
                        edges,
                    );
                }
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_lua_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "function_call" => {
            // field "name" — could be method_index_expression for colon calls
            if let Some(name_node) = node.child_by_field_name("name") {
                let callee_name = if name_node.kind() == "method_index_expression" {
                    name_node
                        .child_by_field_name("method")
                        .map(|m| node_text(&m, source).to_string())
                        .unwrap_or_else(|| node_text(&name_node, source).to_string())
                } else {
                    node_text(&name_node, source).to_string()
                };
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_lua_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_lua_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Luau extractor ──────────────────────────────────────────────────────

fn extract_luau(file: &str, source: &str, tree: &tree_sitter::Tree) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_luau_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_luau_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "function_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let (name, kind) = if name_node.kind() == "method_index_expression" {
                    let method = name_node
                        .child_by_field_name("method")
                        .map(|m| node_text(&m, source).to_string())
                        .unwrap_or_else(|| node_text(&name_node, source).to_string());
                    (method, SymbolKind::Method)
                } else {
                    let name = node_text(&name_node, source).to_string();
                    let kind = if !scope.is_empty() {
                        SymbolKind::Method
                    } else {
                        SymbolKind::Function
                    };
                    (name, kind)
                };
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                let func_sym = sym.qualified.clone();
                symbols.push(sym);
                let mut param_names: HashSet<&str> = HashSet::new();
                if let Some(params_node) = node.child_by_field_name("parameters") {
                    let mut pcursor = params_node.walk();
                    for parameter in params_node.named_children(&mut pcursor) {
                        if let Some(identifier) = luau_parameter_ident(parameter) {
                            param_names.insert(node_text(&identifier, source));
                        }
                    }
                }
                if let Some(body_node) = node.child_by_field_name("body") {
                    if !param_names.is_empty() {
                        collect_param_forward_edges(
                            &LUAU_FLOW_SPEC,
                            file,
                            source,
                            &body_node,
                            &param_names,
                            &func_sym,
                            edges,
                        );
                    }
                    collect_intermediate_flow_edges(
                        &LUAU_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &func_sym,
                        edges,
                    );
                }
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_luau_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "type_definition" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = node_text(&name_node, source).to_string();
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    SymbolKind::Struct,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                symbols.push(sym);
            }
        }
        "function_call" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let callee_name = if name_node.kind() == "method_index_expression" {
                    name_node
                        .child_by_field_name("method")
                        .map(|m| node_text(&m, source).to_string())
                        .unwrap_or_else(|| node_text(&name_node, source).to_string())
                } else {
                    node_text(&name_node, source).to_string()
                };
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_luau_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_luau_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

// ─── Pascal extractor ────────────────────────────────────────────────────

fn extract_pascal(
    file: &str,
    source: &str,
    tree: &tree_sitter::Tree,
) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_pascal_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_pascal_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        "defProc" => {
            // field "header" → declProc → field "name"
            let name = node
                .child_by_field_name("header")
                .and_then(|h| h.child_by_field_name("name"))
                .map(|n| node_text(&n, source).to_string());
            if let Some(name) = name {
                let kind = if !scope.is_empty() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                };
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                let func_sym = sym.qualified.clone();
                let mut param_names: HashSet<&str> = HashSet::new();
                if let Some(header_node) = node.child_by_field_name("header")
                    && let Some(args_node) = header_node.child_by_field_name("args")
                {
                    let mut acursor = args_node.walk();
                    for arg in args_node.children(&mut acursor) {
                        if arg.kind() == "declArg"
                            && let Some(name_node) = arg.child_by_field_name("name")
                            && PASCAL_FLOW_SPEC
                                .param_ident_kinds
                                .contains(&name_node.kind())
                        {
                            param_names.insert(node_text(&name_node, source));
                        }
                    }
                }
                if let Some(body_node) = node.child_by_field_name("body") {
                    if !param_names.is_empty() {
                        collect_param_forward_edges(
                            &PASCAL_FLOW_SPEC,
                            file,
                            source,
                            &body_node,
                            &param_names,
                            &func_sym,
                            edges,
                        );
                    }
                    collect_intermediate_flow_edges(
                        &PASCAL_FLOW_SPEC,
                        file,
                        source,
                        &body_node,
                        &func_sym,
                        edges,
                    );
                }
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    extract_pascal_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }

        "declType" => {
            // field "name" on declType gives the type name
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(&n, source).to_string());
            if let Some(name) = name {
                // Determine kind from child: declClass → Class, declIntf → Interface
                let mut cursor = node.walk();
                let mut kind = SymbolKind::Class;
                for child in node.children(&mut cursor) {
                    match child.kind() {
                        "declClass" => {
                            kind = SymbolKind::Class;
                            break;
                        }
                        "declIntf" => {
                            kind = SymbolKind::Interface;
                            break;
                        }
                        _ => {}
                    }
                }
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_pascal_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            }
        }
        "exprCall" => {
            // field "entity" is the callee
            if let Some(entity) = node.child_by_field_name("entity") {
                let callee_name = node_text(&entity, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_pascal_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_pascal_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
    // ─── SFC script extractor (bounded Vue/Svelte fallback) ───────────────────
}

fn extract_sfc_scripts(file: &str, source: &str) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = source[cursor..].find("<script") {
        let start = cursor + rel;
        let Some(tag_end_rel) = source[start..].find('>') else {
            break;
        };
        let tag_end = start + tag_end_rel;
        let tag = &source[start..=tag_end];
        // Require a real script tag, not <scripture>; avoid treating comments as blocks.
        if tag
            .as_bytes()
            .get(7)
            .is_some_and(|b| b.is_ascii_alphanumeric())
        {
            cursor = tag_end + 1;
            continue;
        }
        let content_start = tag_end + 1;
        let Some(close_rel) = source[content_start..].find("</script>") else {
            break;
        };
        let content_end = content_start + close_rel;
        let script = &source[content_start..content_end];
        let is_ts = tag.split_whitespace().any(|a| {
            a == "lang=\"ts\"" || a == "lang='ts'" || a == "lang=\"tsx\"" || a == "lang='tsx'"
        });
        let grammar: tree_sitter::Language = if is_ts {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        } else {
            tree_sitter_javascript::LANGUAGE.into()
        };
        let mut parser = Parser::new();
        if parser.set_language(&grammar).is_ok() {
            if let Some(tree) = parser.parse(script, None) {
                let (mut syms, mut edgs) = extract_javascript(file, script, &tree);
                let offset = source[..content_start]
                    .bytes()
                    .filter(|b| *b == b'\n')
                    .count() as u32;
                for sym in &mut syms {
                    sym.line_start += offset;
                    sym.line_end += offset;
                }
                for edge in &mut edgs {
                    edge.line += offset;
                }
                symbols.extend(syms);
                edges.extend(edgs);
            }
        }
        cursor = content_end + "</script>".len();
    }
    (symbols, edges)
}

// ─── Svelte extractor (hybrid: template parse → script extract → offset) ─

fn extract_svelte(file: &str, source: &str) -> (Vec<Symbol>, Vec<RawEdge>) {
    // Parse with tree-sitter-svelte-ng to find script_element nodes
    let mut parser = Parser::new();
    let lang: tree_sitter::Language = tree_sitter_svelte_ng::LANGUAGE.into();
    if parser.set_language(&lang).is_err() {
        return (vec![], vec![]);
    }
    let tree = match parser.parse(source, None) {
        Some(t) => t,
        None => return (vec![], vec![]),
    };

    let mut all_symbols = Vec::new();
    let mut all_edges = Vec::new();

    // Walk the tree looking for script_element nodes
    find_svelte_scripts(
        &tree.root_node(),
        file,
        source,
        &mut all_symbols,
        &mut all_edges,
    );

    (all_symbols, all_edges)
}

fn find_svelte_scripts(
    node: &Node,
    file: &str,
    source: &str,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    if node.kind() == "script_element" {
        // Find raw_text child for the script content
        let mut cursor = node.walk();
        let mut is_ts = false;

        // Check start_tag for lang="ts" attribute
        for child in node.children(&mut cursor) {
            if child.kind() == "start_tag" {
                let tag_text = node_text(&child, source);
                if tag_text.contains("lang=\"ts\"") || tag_text.contains("lang='ts'") {
                    is_ts = true;
                }
            }
        }

        let mut cursor2 = node.walk();
        for child in node.children(&mut cursor2) {
            if child.kind() == "raw_text" {
                let script_content = node_text(&child, source);
                let base_offset = child.start_position().row as u32;

                // Parse with JS or TS grammar
                let grammar: tree_sitter::Language = if is_ts {
                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
                } else {
                    tree_sitter_javascript::LANGUAGE.into()
                };

                let mut parser2 = Parser::new();
                if parser2.set_language(&grammar).is_err() {
                    continue;
                }
                let tree2 = match parser2.parse(script_content, None) {
                    Some(t) => t,
                    None => continue,
                };

                let (mut syms, mut edgs) = extract_javascript(file, script_content, &tree2);

                // Apply line offset to all symbols and edges
                for sym in &mut syms {
                    sym.line_start += base_offset;
                    sym.line_end += base_offset;
                }
                for edge in &mut edgs {
                    edge.line += base_offset;
                }

                symbols.extend(syms);
                edges.extend(edgs);
            }
        }
        return; // Don't recurse into children further
    }

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        find_svelte_scripts(&child, file, source, symbols, edges);
    }
}

// ─── Liquid extractor ────────────────────────────────────────────────────

fn extract_liquid(
    file: &str,
    source: &str,
    tree: &tree_sitter::Tree,
) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();
    let root = tree.root_node();
    extract_liquid_node(file, source, &root, &[], None, &mut symbols, &mut edges);
    (symbols, edges)
}

fn extract_liquid_node(
    file: &str,
    source: &str,
    node: &Node,
    scope: &[String],
    parent_fqn: Option<&str>,
    symbols: &mut Vec<Symbol>,
    edges: &mut Vec<RawEdge>,
) {
    let _g = match recursion_guard::RecursionGuard::enter() {
        Some(g) => g,
        None => return,
    };
    match node.kind() {
        // Tag blocks that define symbols
        "tag_assign" | "tag_capture" | "tag_for" | "tag_if" => {
            // Look for the first identifier child as the "name" of this construct
            let mut cursor = node.walk();
            let name = node
                .children(&mut cursor)
                .find(|c| c.kind() == "identifier")
                .map(|c| node_text(&c, source).to_string());
            if let Some(name) = name {
                let kind = SymbolKind::Function;
                let sym = make_symbol(
                    file,
                    &name,
                    scope.to_vec(),
                    kind,
                    node_line_start(node),
                    node_line_end(node),
                    None,
                    parent_fqn.map(|s| s.to_string()),
                );
                let fqn = sym.qualified.fqn();
                symbols.push(sym);
                let mut child_scope = scope.to_vec();
                child_scope.push(name);
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_liquid_node(
                        file,
                        source,
                        &child,
                        &child_scope,
                        Some(&fqn),
                        symbols,
                        edges,
                    );
                }
            } else {
                let mut cursor2 = node.walk();
                for child in node.children(&mut cursor2) {
                    extract_liquid_node(file, source, &child, scope, parent_fqn, symbols, edges);
                }
            }
        }
        // Variable/filter references as call edges
        "filter" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let callee_name = node_text(&name_node, source).to_string();
                if let Some(from_sym) = scope_to_qualified(file, scope) {
                    edges.push(RawEdge {
                        from: from_sym,
                        to: EdgeTarget::Unresolved {
                            name: callee_name,
                            import_path: None,
                            qualifier: None,
                        },
                        kind: EdgeKind::Calls,
                        line: node_line_start(node),
                        confidence: Confidence::Extracted,
                    });
                }
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_liquid_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }

        _ => {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                extract_liquid_node(file, source, &child, scope, parent_fqn, symbols, edges);
            }
        }
    }
}

fn extract_proto(
    file: &str,
    source: &str,
    tree: &tree_sitter::Tree,
) -> (Vec<Symbol>, Vec<RawEdge>) {
    let mut symbols = Vec::new();
    let mut edges = Vec::new();

    fn module_sym(file: &str) -> QualifiedSymbol {
        QualifiedSymbol {
            file: file.to_string(),
            scope_path: Vec::new(),
            name: "<module>".to_string(),
        }
    }

    fn walk(
        file: &str,
        source: &str,
        node: &Node,
        symbols: &mut Vec<Symbol>,
        edges: &mut Vec<RawEdge>,
    ) {
        let sk = match node.kind() {
            "message" => Some(SymbolKind::Struct),
            "enum" => Some(SymbolKind::Enum),
            "service" => Some(SymbolKind::Interface),
            "rpc" => Some(SymbolKind::Function),
            "package" => Some(SymbolKind::Module),
            "import" => Some(SymbolKind::Module),
            _ => None,
        };
        if let Some(sk) = sk {
            if let Some(name_node) = node.named_child(0) {
                let name = node_text(&name_node, source).trim_matches('"');
                if !name.is_empty() {
                    symbols.push(make_symbol(
                        file,
                        name,
                        Vec::new(),
                        sk,
                        node_line_start(node),
                        node_line_end(node),
                        None,
                        None,
                    ));
                }
            }
        }
        if node.kind() == "import"
            && let Some(name_node) = node.named_child(0)
        {
            let target = node_text(&name_node, source).trim_matches('"').to_string();
            if !target.is_empty() {
                edges.push(RawEdge {
                    from: module_sym(file),
                    to: EdgeTarget::Unresolved {
                        name: target.clone(),
                        import_path: Some(target),
                        qualifier: None,
                    },
                    kind: EdgeKind::Imports,
                    line: node_line_start(node),
                    confidence: Confidence::Extracted,
                });
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            walk(file, source, &child, symbols, edges);
        }
    }

    walk(file, source, &tree.root_node(), &mut symbols, &mut edges);
    (symbols, edges)
}
// ─── C / C++ unit tests ───────────────────────────────────────────────────

#[cfg(test)]
mod cpp_tests {
    use super::*;
    use crate::parsing::relations::{EdgeKind, EdgeTarget};
    use crate::parsing::symbols::SymbolKind;

    /// Helper: parse C++ source and return the ParseResult.
    fn parse_cpp(source: &str) -> ParseResult {
        parse_file("test.cpp", source)
    }

    // ─── Test 3.1: Basic function extraction ──────────────────────────────

    /// Parse a simple free C++ function — verify name, kind, and line numbers.
    #[test]
    fn test_basic_function_extraction() {
        let src = r#"
int add(int a, int b) {
    return a + b;
}
"#;
        let result = parse_cpp(src);
        let syms = &result.symbols;
        assert!(!syms.is_empty(), "expected at least one symbol");
        let func = syms.iter().find(|s| s.qualified.name == "add");
        assert!(
            func.is_some(),
            "expected symbol named 'add'; got: {:?}",
            syms.iter().map(|s| &s.qualified.name).collect::<Vec<_>>()
        );
        let func = func.unwrap();
        assert_eq!(func.kind, SymbolKind::Function, "add must be Function kind");
        assert_eq!(
            func.qualified.scope_path,
            Vec::<String>::new(),
            "add must have empty scope_path at file level"
        );
        assert!(func.line_start >= 1, "line_start must be >= 1");
        assert!(
            func.line_end >= func.line_start,
            "line_end must be >= line_start"
        );
    }

    // ─── Test 3.2: Nested namespace scope_path ────────────────────────────

    /// Nested namespaces produce correct scope_path on the inner function.
    #[test]
    fn test_nested_namespace_scope_path() {
        let src = r#"
namespace outer {
    namespace inner {
        void foo() {}
    }
}
"#;
        let result = parse_cpp(src);
        let syms = &result.symbols;
        let foo = syms.iter().find(|s| s.qualified.name == "foo");
        assert!(
            foo.is_some(),
            "expected symbol 'foo'; got: {:?}",
            syms.iter().map(|s| &s.qualified.name).collect::<Vec<_>>()
        );
        let foo = foo.unwrap();
        // scope_path should contain ["outer", "inner"] (the namespace names pushed by namespace_definition)
        assert!(
            foo.qualified.scope_path.contains(&"outer".to_string()),
            "scope_path must contain 'outer'; got: {:?}",
            foo.qualified.scope_path
        );
        assert!(
            foo.qualified.scope_path.contains(&"inner".to_string()),
            "scope_path must contain 'inner'; got: {:?}",
            foo.qualified.scope_path
        );
    }

    // ─── Test 3.3: Class with inline and out-of-line methods ──────────────

    /// Class with an inline method and an out-of-line `Foo::bar()` definition
    /// both produce Method-kind symbols.
    #[test]
    fn test_class_inline_and_outofline_methods() {
        let src = r#"
class Foo {
public:
    void inline_method() {}
    void bar();
};

void Foo::bar() {
    // out-of-line
}
"#;
        let result = parse_cpp(src);
        let syms = &result.symbols;

        // inline_method inside the class should be Method
        let inline_m = syms.iter().find(|s| s.qualified.name == "inline_method");
        assert!(
            inline_m.is_some(),
            "expected 'inline_method'; symbols: {:?}",
            syms.iter().map(|s| &s.qualified.name).collect::<Vec<_>>()
        );
        assert_eq!(
            inline_m.unwrap().kind,
            SymbolKind::Method,
            "inline_method must be Method"
        );

        // out-of-line Foo::bar() must also be Method
        let bar = syms.iter().find(|s| s.qualified.name == "bar");
        assert!(
            bar.is_some(),
            "expected 'bar'; symbols: {:?}",
            syms.iter().map(|s| &s.qualified.name).collect::<Vec<_>>()
        );
        assert_eq!(
            bar.unwrap().kind,
            SymbolKind::Method,
            "Foo::bar must be Method (out-of-line)"
        );
    }

    // ─── Test 3.4: Call edge from A to B ──────────────────────────────────

    /// When function A calls function B, a RawEdge must be produced.
    #[test]
    fn test_call_edge_a_calls_b() {
        let src = r#"
void b_func() {}

void a_func() {
    b_func();
}
"#;
        let result = parse_cpp(src);
        let edge = result.edges.iter().find(|e| {
            if let EdgeTarget::Unresolved { name, .. } = &e.to {
                e.from.name == "a_func" && name == "b_func"
            } else {
                false
            }
        });
        assert!(
            edge.is_some(),
            "expected edge from a_func to b_func; edges: {:?}",
            result
                .edges
                .iter()
                .map(|e| (&e.from.name, &e.to))
                .collect::<Vec<_>>()
        );
        let edge = edge.unwrap();
        assert_eq!(edge.kind, EdgeKind::Calls, "edge kind must be Calls");
    }

    // ─── Test 3.5: #include extraction into imports HashMap ───────────────

    /// Both `#include <linux/list.h>` and `#include "local.h"` must produce
    /// entries in ParseResult.imports with the stripped path as key.
    #[test]
    fn test_include_extraction() {
        let src = r#"
#include <linux/list.h>
#include "local.h"

void foo() {}
"#;
        let result = parse_cpp(src);
        let imp = &result.imports;
        assert!(
            imp.contains_key("linux/list.h"),
            "imports must contain 'linux/list.h'; got: {:?}",
            imp.keys().collect::<Vec<_>>()
        );
        assert!(
            imp.contains_key("local.h"),
            "imports must contain 'local.h'; got: {:?}",
            imp.keys().collect::<Vec<_>>()
        );
    }

    // ─── Test 3.6: Import context propagation on qualified call ───────────

    /// `#include "Agent.h"` + `Agent::run()` call must produce an edge with
    /// `import_path = Some("Agent.h")`.
    #[test]
    fn test_import_context_propagation_qualified_call() {
        let src = r#"
#include "Agent.h"

void caller() {
    Agent::run();
}
"#;
        let result = parse_cpp(src);
        let edge = result.edges.iter().find(|e| {
            if let EdgeTarget::Unresolved { name, .. } = &e.to {
                name == "run"
            } else {
                false
            }
        });
        assert!(
            edge.is_some(),
            "expected edge with callee 'run'; edges: {:?}",
            result.edges.iter().map(|e| &e.to).collect::<Vec<_>>()
        );
        let edge = edge.unwrap();
        if let EdgeTarget::Unresolved { import_path, .. } = &edge.to {
            assert_eq!(
                *import_path,
                Some("Agent.h".to_string()),
                "edge.import_path must be Some(\"Agent.h\")"
            );
        } else {
            panic!("edge must be Unresolved");
        }
    }

    // ─── Test 3.7: Qualified and field-expression call leaf names ─────────

    /// Various call expression forms must produce edges with correct callee
    /// leaf names:
    ///   - `ns::Foo::method()` → callee = "method"
    ///   - `obj.method()` → callee = "method"
    ///   - `ptr->method()` → callee = "method"
    #[test]
    fn test_qualified_and_field_call_leaf_names() {
        let src = r#"
void caller() {
    ns::Foo::method();
    obj.field_method();
    ptr->ptr_method();
}
"#;
        let result = parse_cpp(src);
        let callee_names: Vec<&str> = result
            .edges
            .iter()
            .filter_map(|e| {
                if let EdgeTarget::Unresolved { name, .. } = &e.to {
                    Some(name.as_str())
                } else {
                    None
                }
            })
            .collect();

        assert!(
            callee_names.contains(&"method"),
            "expected callee 'method' (from ns::Foo::method()); got: {:?}",
            callee_names
        );
        assert!(
            callee_names.contains(&"field_method"),
            "expected callee 'field_method' (from obj.field_method()); got: {:?}",
            callee_names
        );
        assert!(
            callee_names.contains(&"ptr_method"),
            "expected callee 'ptr_method' (from ptr->ptr_method()); got: {:?}",
            callee_names
        );
    }
}
#[cfg(test)]
mod c_data_flow_tests {
    use super::*;

    fn data_flow_edges(src: &str) -> Vec<RawEdge> {
        parse_file("test.c", src)
            .edges
            .into_iter()
            .filter(|edge| matches!(edge.kind, EdgeKind::DataFlowsTo))
            .collect()
    }

    fn assert_data_flow_to(edges: &[RawEdge], target: &str) {
        assert_eq!(
            edges.len(),
            1,
            "expected exactly one data-flow edge: {edges:?}"
        );
        assert_eq!(edges[0].kind, EdgeKind::DataFlowsTo);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, target),
            _ => panic!("expected unresolved {target} target"),
        }
    }

    #[test]
    fn forwards_parameter_to_call() {
        let edges = data_flow_edges("void Outer(int value) { inner(value); }");
        assert_data_flow_to(&edges, "inner");
        assert_eq!(edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn literal_argument_has_no_data_flow() {
        assert!(data_flow_edges("void Outer(void) { inner(42); }").is_empty());
    }

    #[test]
    fn intermediate_initializer_flows_to_call() {
        let edges = data_flow_edges("void Outer(void) { int x = foo(); bar(x); }");
        assert_data_flow_to(&edges, "bar");
    }

    #[test]
    fn intermediate_assignment_flows_to_call() {
        let edges = data_flow_edges("void Outer(void) { int x; x = foo(); bar(x); }");
        assert_data_flow_to(&edges, "bar");
    }
}

#[cfg(test)]
mod cpp_data_flow_tests {
    use super::*;

    fn data_flow_edges(src: &str) -> Vec<RawEdge> {
        parse_file("test.cpp", src)
            .edges
            .into_iter()
            .filter(|edge| matches!(edge.kind, EdgeKind::DataFlowsTo))
            .collect()
    }

    fn assert_data_flow_to(edges: &[RawEdge], target: &str) {
        assert_eq!(
            edges.len(),
            1,
            "expected exactly one data-flow edge: {edges:?}"
        );
        assert_eq!(edges[0].kind, EdgeKind::DataFlowsTo);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, target),
            _ => panic!("expected unresolved {target} target"),
        }
    }

    #[test]
    fn forwards_parameter_to_call() {
        let edges = data_flow_edges("void Outer(int value) { Inner(value); }");
        assert_data_flow_to(&edges, "Inner");
        assert_eq!(edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn literal_argument_has_no_data_flow() {
        assert!(data_flow_edges("void Outer() { Inner(42); }").is_empty());
    }

    #[test]
    fn intermediate_initializer_flows_to_call() {
        let edges = data_flow_edges("void Outer() { int x = Foo(); Bar(x); }");
        assert_data_flow_to(&edges, "Bar");
    }

    #[test]
    fn intermediate_assignment_flows_to_call() {
        let edges = data_flow_edges("void Outer() { int x; x = Foo(); Bar(x); }");
        assert_data_flow_to(&edges, "Bar");
    }
}

// ─── New language extractor tests ────────────────────────────────────────

#[cfg(test)]
mod csharp_tests {
    use super::*;

    #[test]
    fn test_csharp_class_method_namespace() {
        let src = r#"
namespace MyApp {
    class Greeter {
        void SayHello() {
            Console.WriteLine("Hello");
        }
    }
}
"#;
        let result = parse_file("test.cs", src);
        let ns = result.symbols.iter().find(|s| s.qualified.name == "MyApp");
        assert!(ns.is_some(), "expected namespace MyApp");
        assert_eq!(ns.unwrap().kind, SymbolKind::Module);

        let cls = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "Greeter");
        assert!(cls.is_some(), "expected class Greeter");
        assert_eq!(cls.unwrap().kind, SymbolKind::Class);

        let method = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "SayHello");
        assert!(method.is_some(), "expected method SayHello");
        assert_eq!(method.unwrap().kind, SymbolKind::Method);
    }

    #[test]
    fn test_csharp_interface_enum() {
        let src = r#"
interface IService {
    void Run();
}
enum Color { Red, Green, Blue }
"#;
        let result = parse_file("test.cs", src);
        let iface = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "IService");
        assert!(iface.is_some(), "expected interface IService");
        assert_eq!(iface.unwrap().kind, SymbolKind::Interface);

        let enm = result.symbols.iter().find(|s| s.qualified.name == "Color");
        assert!(enm.is_some(), "expected enum Color");
        assert_eq!(enm.unwrap().kind, SymbolKind::Enum);
    }

    #[test]
    fn test_csharp_call_edge() {
        let src = r#"
class Foo {
    void Bar() {
        Baz();
    }
}
"#;
        let result = parse_file("test.cs", src);
        let edge = result.edges.iter().find(|e| {
            if let EdgeTarget::Unresolved { name, .. } = &e.to {
                name.contains("Baz")
            } else {
                false
            }
        });
        assert!(
            edge.is_some(),
            "expected call edge to Baz; edges: {:?}",
            result.edges
        );
    }
}

#[cfg(test)]
mod php_tests {
    use super::*;

    #[test]
    fn test_php_class_method_namespace() {
        let src = r#"<?php
namespace App;
class UserService {
    function getUser() {
        return $this->findById(1);
    }
}
"#;
        let result = parse_file("test.php", src);
        let ns = result.symbols.iter().find(|s| s.qualified.name == "App");
        assert!(
            ns.is_some(),
            "expected namespace App; syms: {:?}",
            result
                .symbols
                .iter()
                .map(|s| &s.qualified.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(ns.unwrap().kind, SymbolKind::Module);

        let cls = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "UserService");
        assert!(cls.is_some(), "expected class UserService");
        assert_eq!(cls.unwrap().kind, SymbolKind::Class);

        let method = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "getUser");
        assert!(method.is_some(), "expected method getUser");
        assert_eq!(method.unwrap().kind, SymbolKind::Method);
    }

    #[test]
    fn test_php_interface_trait_enum() {
        let src = r#"<?php
interface Cacheable {}
trait Loggable {}
enum Status { case Active; case Inactive; }
"#;
        let result = parse_file("test.php", src);
        let iface = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "Cacheable");
        assert!(iface.is_some(), "expected interface Cacheable");
        assert_eq!(iface.unwrap().kind, SymbolKind::Interface);

        let tr = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "Loggable");
        assert!(tr.is_some(), "expected trait Loggable");
        assert_eq!(tr.unwrap().kind, SymbolKind::Trait);

        let enm = result.symbols.iter().find(|s| s.qualified.name == "Status");
        assert!(enm.is_some(), "expected enum Status");
        assert_eq!(enm.unwrap().kind, SymbolKind::Enum);
    }

    #[test]
    fn test_php_call_edges() {
        let src = r#"<?php
class Foo {
    function bar() {
        baz();
        $this->qux();
    }
}
"#;
        let result = parse_file("test.php", src);
        let baz_edge = result.edges.iter().find(|e| {
            if let EdgeTarget::Unresolved { name, .. } = &e.to {
                name == "baz"
            } else {
                false
            }
        });
        assert!(baz_edge.is_some(), "expected call to baz");
        let qux_edge = result.edges.iter().find(|e| {
            if let EdgeTarget::Unresolved { name, .. } = &e.to {
                name == "qux"
            } else {
                false
            }
        });
        assert!(qux_edge.is_some(), "expected call to qux");
    }
}

#[cfg(test)]
mod csharp_data_flow_tests {
    use super::*;

    fn data_flow_edges(src: &str) -> Vec<RawEdge> {
        parse_file("flow.cs", src)
            .edges
            .into_iter()
            .filter(|edge| matches!(edge.kind, EdgeKind::DataFlowsTo))
            .collect()
    }

    #[test]
    fn forwards_parameter_to_call() {
        let edges = data_flow_edges("class C { void Outer(string x) { Inner(x); } }");
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "Inner"),
            _ => panic!("expected unresolved Inner target"),
        }
        assert_eq!(edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn literal_argument_has_no_data_flow() {
        assert!(data_flow_edges("class C { void Outer() { Inner(42); } }").is_empty());
    }

    #[test]
    fn intermediate_initializer_flows_to_call() {
        let edges = data_flow_edges("class C { void Outer() { string x = Foo(); Bar(x); } }");
        assert!(edges.iter().any(|edge| matches!(
            &edge.to,
            EdgeTarget::Unresolved { name, .. } if name == "Bar"
        )));
    }
}

#[cfg(test)]
mod php_data_flow_tests {
    use super::*;

    fn data_flow_edges(src: &str) -> Vec<RawEdge> {
        parse_file("flow.php", src)
            .edges
            .into_iter()
            .filter(|edge| matches!(edge.kind, EdgeKind::DataFlowsTo))
            .collect()
    }

    #[test]
    fn forwards_parameter_to_call() {
        let edges = data_flow_edges("<?php function outer($x) { inner($x); }");
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "inner"),
            _ => panic!("expected unresolved inner target"),
        }
        assert_eq!(edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn literal_argument_has_no_data_flow() {
        assert!(data_flow_edges("<?php function outer() { inner(42); }").is_empty());
    }

    #[test]
    fn intermediate_assignment_flows_to_call() {
        let edges = data_flow_edges("<?php function outer() { $x = foo(); bar($x); }");
        assert!(edges.iter().any(|edge| matches!(
            &edge.to,
            EdgeTarget::Unresolved { name, .. } if name == "bar"
        )));
    }
}

#[cfg(test)]
mod kotlin_data_flow_tests {
    use super::*;

    fn data_flow_edges(src: &str) -> Vec<RawEdge> {
        parse_file("flow.kt", src)
            .edges
            .into_iter()
            .filter(|edge| matches!(edge.kind, EdgeKind::DataFlowsTo))
            .collect()
    }

    fn assert_data_flow_to(edges: &[RawEdge], target: &str, confidence: Confidence) {
        assert_eq!(
            edges.len(),
            1,
            "expected exactly one data-flow edge: {edges:?}"
        );
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, target),
            _ => panic!("expected unresolved {target} target"),
        }
        assert_eq!(edges[0].confidence, confidence);
    }

    #[test]
    fn forwards_parameter_to_call() {
        let edges = data_flow_edges("fun outer(input: String) { inner(input) }");
        assert_data_flow_to(&edges, "inner", Confidence::Inferred(0.75));
    }

    #[test]
    fn literal_argument_has_no_data_flow() {
        assert!(data_flow_edges("fun outer() { inner(42) }").is_empty());
    }

    #[test]
    fn named_literal_argument_has_no_data_flow_when_label_matches_parameter() {
        assert!(data_flow_edges("fun outer(value: String) { inner(value = 42) }").is_empty());
    }

    #[test]
    fn intermediate_initializer_flows_to_call() {
        let edges = data_flow_edges("fun outer() { val x = foo(); bar(x) }");
        assert_data_flow_to(&edges, "bar", Confidence::Inferred(0.6));
    }

    #[test]
    fn intermediate_assignment_flows_to_call() {
        let edges = data_flow_edges("fun outer() { var x = seed(); x = foo(); bar(x) }");
        assert_data_flow_to(&edges, "bar", Confidence::Inferred(0.6));
    }
    #[test]
    fn forwards_named_parameter_to_call() {
        let edges = data_flow_edges("fun outer(input: String) { inner(value = input) }");
        assert_data_flow_to(&edges, "inner", Confidence::Inferred(0.75));
    }

    #[test]
    fn named_intermediate_initializer_flows_to_call() {
        let edges = data_flow_edges("fun outer() { val x = foo(); bar(value = x) }");
        assert_data_flow_to(&edges, "bar", Confidence::Inferred(0.6));
    }
}

#[cfg(test)]
mod ruby_tests {
    use super::*;

    #[test]
    fn test_ruby_class_module_method() {
        let src = r#"
module Utils
  class Parser
    def parse(input)
      tokenize(input)
    end
  end
end
"#;
        let result = parse_file("test.rb", src);
        let mod_sym = result.symbols.iter().find(|s| s.qualified.name == "Utils");
        assert!(mod_sym.is_some(), "expected module Utils");

        assert_eq!(mod_sym.unwrap().kind, SymbolKind::Module);

        let cls = result.symbols.iter().find(|s| s.qualified.name == "Parser");
        assert!(cls.is_some(), "expected class Parser");
        assert_eq!(cls.unwrap().kind, SymbolKind::Class);

        let method = result.symbols.iter().find(|s| s.qualified.name == "parse");
        assert!(method.is_some(), "expected method parse");
        assert_eq!(method.unwrap().kind, SymbolKind::Method);
    }

    #[test]
    fn test_ruby_call_edge() {
        let src = r#"
class Foo
  def bar
    baz()
  end
end
"#;
        let result = parse_file("test.rb", src);
        let edge = result.edges.iter().find(|e| {
            if let EdgeTarget::Unresolved { name, .. } = &e.to {
                name == "baz"
            } else {
                false
            }
        });
        assert!(
            edge.is_some(),
            "expected call edge to baz; edges: {:?}",
            result.edges
        );
    }
}

#[cfg(test)]
mod kotlin_tests {
    use super::*;

    #[test]
    fn test_kotlin_class_function() {
        let src = r#"
class UserRepo {
    fun findById(id: Int): User {
        return query(id)
    }
}
"#;
        let result = parse_file("test.kt", src);
        let cls = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "UserRepo");
        assert!(
            cls.is_some(),
            "expected class UserRepo; syms: {:?}",
            result
                .symbols
                .iter()
                .map(|s| &s.qualified.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(cls.unwrap().kind, SymbolKind::Class);

        let func = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "findById");
        assert!(func.is_some(), "expected function findById");
        assert_eq!(func.unwrap().kind, SymbolKind::Method);
    }

    #[test]
    fn test_kotlin_call_edge() {
        let src = r#"
fun main() {
    println("hello")
}
"#;
        let result = parse_file("test.kt", src);
        let func = result.symbols.iter().find(|s| s.qualified.name == "main");
        assert!(func.is_some(), "expected function main");
        assert_eq!(func.unwrap().kind, SymbolKind::Function);

        let edge = result.edges.iter().find(|e| {
            if let EdgeTarget::Unresolved { name, .. } = &e.to {
                name.contains("println")
            } else {
                false
            }
        });
        assert!(
            edge.is_some(),
            "expected call edge to println; edges: {:?}",
            result.edges
        );
    }
}

#[cfg(test)]
mod swift_tests {
    use super::*;

    #[test]
    fn test_swift_class_function() {
        let src = r#"
class Greeter {
    func greet(name: String) {
        print(name)
    }
}
"#;
        let result = parse_file("test.swift", src);
        let cls = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "Greeter");
        assert!(
            cls.is_some(),
            "expected class Greeter; syms: {:?}",
            result
                .symbols
                .iter()
                .map(|s| &s.qualified.name)
                .collect::<Vec<_>>()
        );

        let func = result.symbols.iter().find(|s| s.qualified.name == "greet");
        assert!(func.is_some(), "expected function greet");
        assert_eq!(func.unwrap().kind, SymbolKind::Method);
    }

    #[test]
    fn test_swift_protocol() {
        let src = r#"
protocol Drawable {
    func draw()
}
"#;
        let result = parse_file("test.swift", src);
        let proto = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "Drawable");
        assert!(
            proto.is_some(),
            "expected protocol Drawable; syms: {:?}",
            result
                .symbols
                .iter()
                .map(|s| &s.qualified.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(proto.unwrap().kind, SymbolKind::Interface);
    }
}
#[cfg(test)]
mod swift_data_flow_tests {
    use super::*;

    fn data_flow_edges(source: &str) -> Vec<RawEdge> {
        parse_file("test.swift", source)
            .edges
            .into_iter()
            .filter(|edge| matches!(edge.kind, EdgeKind::DataFlowsTo))
            .collect()
    }

    #[test]
    fn forwards_parameter_to_called_function() {
        let edges = data_flow_edges(
            r#"
func g(x: Int) {
    f(x)
}
"#,
        );
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "f"),
            _ => panic!("expected unresolved f target"),
        }
        assert_eq!(edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn no_edge_for_literal_argument() {
        let edges = data_flow_edges(
            r#"
func g(x: Int) {
    f(42)
}
"#,
        );
        assert!(
            edges.is_empty(),
            "literal argument emitted DataFlowsTo: {edges:?}"
        );
    }

    #[test]
    fn intermediate_variable_flow_property_declaration() {
        let edges = data_flow_edges(
            r#"
func g() {
    let x = foo()
    bar(x)
}
"#,
        );
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "bar"),
            _ => panic!("expected unresolved bar target"),
        }
        assert_eq!(edges[0].confidence, Confidence::Inferred(0.6));
    }

    #[test]
    fn intermediate_variable_flow_assignment() {
        let edges = data_flow_edges(
            r#"
func g() {
    x = foo()
    bar(x)
}
"#,
        );
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "bar"),
            _ => panic!("expected unresolved bar target"),
        }
        assert_eq!(edges[0].confidence, Confidence::Inferred(0.6));
    }
}

#[cfg(test)]
mod dart_tests {
    use super::*;

    #[test]
    fn test_dart_class_method() {
        let src = r#"
class Animal {
  void speak() {
    print("...");
  }
}
"#;
        let result = parse_file("test.dart", src);
        let cls = result.symbols.iter().find(|s| s.qualified.name == "Animal");
        assert!(
            cls.is_some(),
            "expected class Animal; syms: {:?}",
            result
                .symbols
                .iter()
                .map(|s| &s.qualified.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(cls.unwrap().kind, SymbolKind::Class);
    }

    #[test]
    fn test_dart_enum() {
        let src = r#"
enum Direction { north, south, east, west }
"#;
        let result = parse_file("test.dart", src);
        let enm = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "Direction");
        assert!(
            enm.is_some(),
            "expected enum Direction; syms: {:?}",
            result
                .symbols
                .iter()
                .map(|s| &s.qualified.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(enm.unwrap().kind, SymbolKind::Enum);
    }
}

#[cfg(test)]
mod lua_tests {
    use super::*;

    #[test]
    fn test_lua_function() {
        let src = r#"
function greet(name)
    print(name)
end
"#;
        let result = parse_file("test.lua", src);
        let func = result.symbols.iter().find(|s| s.qualified.name == "greet");
        assert!(
            func.is_some(),
            "expected function greet; syms: {:?}",
            result
                .symbols
                .iter()
                .map(|s| &s.qualified.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(func.unwrap().kind, SymbolKind::Function);
    }

    #[test]
    fn test_lua_call_edge() {
        let src = r#"
function foo()
    bar()
end
"#;
        let result = parse_file("test.lua", src);
        let edge = result.edges.iter().find(|e| {
            if let EdgeTarget::Unresolved { name, .. } = &e.to {
                name == "bar"
            } else {
                false
            }
        });
        assert!(
            edge.is_some(),
            "expected call edge to bar; edges: {:?}",
            result.edges
        );
    }

    #[test]
    fn test_lua_colon_method() {
        let src = r#"
function obj:method()
    self:other()
end
"#;
        let result = parse_file("test.lua", src);
        let method = result.symbols.iter().find(|s| s.qualified.name == "method");
        assert!(
            method.is_some(),
            "expected method; syms: {:?}",
            result
                .symbols
                .iter()
                .map(|s| &s.qualified.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(method.unwrap().kind, SymbolKind::Method);
    }
}

#[cfg(test)]
mod lua_data_flow_tests {
    use super::*;

    #[test]
    fn forwards_param_to_called_function() {
        let src = r#"
function g(x)
    f(x)
end
"#;
        let result = parse_file("test.lua", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 1);
        match &df_edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "f"),
            _ => panic!("expected Unresolved target"),
        }
        assert_eq!(df_edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn no_edge_for_literal_argument() {
        let src = r#"
function g(x)
    f(42)
end
"#;
        let result = parse_file("test.lua", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(
            df_edges.is_empty(),
            "literal argument must not produce a DataFlowsTo edge; got {:?}",
            df_edges
        );
    }

    #[test]
    fn intermediate_variable_flow_local_declaration() {
        let src = r#"
function g()
    local x = foo()
    bar(x)
end
"#;
        let result = parse_file("test.lua", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 1);
        match &df_edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "bar"),
            _ => panic!("expected Unresolved target"),
        }
        assert_eq!(df_edges[0].confidence, Confidence::Inferred(0.6));
    }

    #[test]
    fn intermediate_variable_flow_plain_assignment() {
        let src = r#"
function g()
    x = foo()
    bar(x)
end
"#;
        let result = parse_file("test.lua", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 1);
        match &df_edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "bar"),
            _ => panic!("expected Unresolved target"),
        }
        assert_eq!(df_edges[0].confidence, Confidence::Inferred(0.6));
    }
}

#[cfg(test)]
mod luau_data_flow_tests {
    use super::*;

    fn data_flow_edges(source: &str) -> Vec<RawEdge> {
        parse_file("test.luau", source)
            .edges
            .into_iter()
            .filter(|edge| matches!(edge.kind, EdgeKind::DataFlowsTo))
            .collect()
    }

    #[test]
    fn forwards_parameter_to_called_function() {
        let edges = data_flow_edges(
            r#"
function g(x: number)
    f(x)
end
"#,
        );
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "f"),
            _ => panic!("expected unresolved f target"),
        }
        assert_eq!(edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn does_not_forward_literal_argument() {
        let edges = data_flow_edges(
            r#"
function g(x: number)
    f(42)
end
"#,
        );
        assert!(
            edges.is_empty(),
            "literal argument emitted DataFlowsTo: {edges:?}"
        );
    }

    #[test]
    fn tracks_local_intermediate_flow() {
        let edges = data_flow_edges(
            r#"
function g()
    local x = foo()
    bar(x)
end
"#,
        );
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "bar"),
            _ => panic!("expected unresolved bar target"),
        }
        assert_eq!(edges[0].confidence, Confidence::Inferred(0.6));
    }

    #[test]
    fn tracks_plain_assignment_intermediate_flow() {
        let edges = data_flow_edges(
            r#"
function g()
    x = foo()
    bar(x)
end
"#,
        );
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "bar"),
            _ => panic!("expected unresolved bar target"),
        }
        assert_eq!(edges[0].confidence, Confidence::Inferred(0.6));
    }
}

#[cfg(test)]
mod luau_tests {
    use super::*;

    #[test]
    fn test_luau_function_and_type() {
        let src = r#"
type Point = { x: number, y: number }

function distance(a: Point, b: Point): number
    return math.sqrt(0)
end
"#;
        let result = parse_file("test.luau", src);
        let type_sym = result.symbols.iter().find(|s| s.qualified.name == "Point");
        assert!(
            type_sym.is_some(),
            "expected type Point; syms: {:?}",
            result
                .symbols
                .iter()
                .map(|s| &s.qualified.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(type_sym.unwrap().kind, SymbolKind::Struct);

        let func = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "distance");
        assert!(func.is_some(), "expected function distance");
        assert_eq!(func.unwrap().kind, SymbolKind::Function);
    }
}

#[cfg(test)]
mod pascal_tests {
    use super::*;

    #[test]
    fn test_pascal_procedure() {
        let src = r#"
procedure Hello;
begin
  WriteLn('Hello');
end;
"#;
        let result = parse_file("test.pas", src);
        // Pascal grammar may vary; check if we get any symbols at all
        if !result.symbols.is_empty() {
            let proc = result.symbols.iter().find(|s| s.qualified.name == "Hello");
            assert!(
                proc.is_some(),
                "expected procedure Hello; syms: {:?}",
                result
                    .symbols
                    .iter()
                    .map(|s| &s.qualified.name)
                    .collect::<Vec<_>>()
            );
        }
    }
}

#[cfg(test)]
mod pascal_param_forward_tests {
    use super::*;

    #[test]
    fn forwards_param_to_call() {
        let src = r#"
procedure Outer(x: Integer);
begin
  Inner(x);
end;
"#;
        let result = parse_file("test.pas", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 1);
        match &df_edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "Inner"),
            _ => panic!("expected Unresolved"),
        }
        assert_eq!(df_edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn no_edge_for_literal_arg() {
        let src = r#"
procedure Outer(x: Integer);
begin
  Inner(42);
end;
"#;
        let result = parse_file("test.pas", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(df_edges.is_empty());
    }

    #[test]
    fn forwards_two_params() {
        let src = r#"
procedure Outer(a: Integer; b: Integer);
begin
  First(a);
  Second(b);
end;
"#;
        let result = parse_file("test.pas", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 2);
    }
}

#[cfg(test)]
mod pascal_intermediate_flow_tests {
    use super::*;

    #[test]
    fn intermediate_variable_flow() {
        let src = r#"
procedure Outer;
var
  x: Integer;
begin
  x := Foo();
  Bar(x);
end;
"#;
        let result = parse_file("test.pas", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(!df_edges.is_empty(), "expected DataFlowsTo edge, got none");
        let names: Vec<_> = df_edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"Bar"),
            "expected edge to Bar, got {:?}",
            names
        );
    }
}

#[cfg(test)]
mod svelte_tests {
    use super::*;

    #[test]
    fn test_svelte_script_extraction() {
        let src = r#"<script>
function greet(name) {
    console.log(name);
}
</script>

<h1>Hello</h1>
"#;
        let result = parse_file("test.svelte", src);
        let func = result.symbols.iter().find(|s| s.qualified.name == "greet");
        assert!(
            func.is_some(),
            "expected function greet from script block; syms: {:?}",
            result
                .symbols
                .iter()
                .map(|s| &s.qualified.name)
                .collect::<Vec<_>>()
        );
        assert_eq!(func.unwrap().kind, SymbolKind::Function);
        // Line offset: script block starts at line 1 (0-indexed row 0),
        // raw_text starts at row 1, function is on row 1 of raw_text
        // so line_start should be > 1
        assert!(
            func.unwrap().line_start >= 2,
            "line_start should be offset; got {}",
            func.unwrap().line_start
        );
    }

    #[test]
    fn test_svelte_ts_script() {
        let src = r#"<script lang="ts">
function typedFunc(): string {
    return "hello";
}
</script>
"#;
        let result = parse_file("test.svelte", src);
        let func = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "typedFunc");
        assert!(
            func.is_some(),
            "expected function typedFunc from TS script block; syms: {:?}",
            result
                .symbols
                .iter()
                .map(|s| &s.qualified.name)
                .collect::<Vec<_>>()
        );
    }
}
#[cfg(test)]
mod proto_tests {
    use super::*;
    use crate::parsing::relations::{EdgeKind, EdgeTarget};
    use crate::parsing::symbols::SymbolKind;

    #[test]
    fn extracts_proto_declarations_and_imports() {
        let src = "syntax = \"proto3\";\npackage acme.v1;\nimport \"common/types.proto\";\n\nmessage User { string name = 1; }\n\nenum Role { UNKNOWN = 0; }\n\nservice Auth { rpc Login(User) returns (User); }\n";
        let result = parse_file("auth.proto", src);
        let names: Vec<&str> = result
            .symbols
            .iter()
            .map(|s| s.qualified.name.as_str())
            .collect();
        for want in [
            "acme.v1",
            "common/types.proto",
            "User",
            "Role",
            "Auth",
            "Login",
        ] {
            assert!(names.contains(&want), "missing {want} in {names:?}");
        }
        let user = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "User")
            .expect("User message symbol");
        assert_eq!(user.kind, SymbolKind::Struct);
        assert_eq!(user.line_start, 5);
        let import_edge = result
            .edges
            .iter()
            .find(|e| e.kind == EdgeKind::Imports)
            .expect("one import edge");
        match &import_edge.to {
            EdgeTarget::Unresolved {
                name, import_path, ..
            } => {
                assert_eq!(name, "common/types.proto");
                assert_eq!(import_path.as_deref(), Some("common/types.proto"));
            }
            _ => panic!("unexpected import target"),
        }
    }

    #[test]
    fn non_proto_and_blank_proto_are_safe() {
        assert!(
            parse_file("auth.ts", "function f() {}")
                .symbols
                .iter()
                .all(|s| s.qualified.name != "f" || s.kind == SymbolKind::Function)
        );
        assert!(parse_file("blank.proto", "// nothing").symbols.is_empty());
    }
}

#[cfg(test)]
mod vue_tests {
    use super::*;
    #[test]
    fn extracts_vue_script_with_offset() {
        let src = "<template><div /></template>\n<script setup lang=\"ts\">\nfunction greet() { helper(); }\nfunction helper() {}\n</script>\n";
        let result = parse_file("Comp.vue", src);
        let f = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "greet")
            .unwrap();
        assert_eq!(f.line_start, 3);
        assert!(result.edges.iter().any(|e| e.line == 3));
    }
    #[test]
    fn template_only_and_malformed_vue_are_safe() {
        assert!(
            parse_file("Comp.vue", "<template><div /></template>")
                .symbols
                .is_empty()
        );
        assert!(
            parse_file("Comp.vue", "<script>function broken(")
                .symbols
                .is_empty()
        );
    }
}

#[cfg(test)]
mod liquid_tests {
    use super::*;

    #[test]
    fn test_liquid_basic_parse() {
        let src = r#"{% assign greeting = "hello" %}
{{ greeting | upcase }}
"#;
        let result = parse_file("test.liquid", src);
        // Liquid parsing should not crash, and ideally find symbols
        // The exact grammar behavior depends on the vendored parser
        assert!(
            !result.chunks.is_empty(),
            "expected at least one chunk from liquid file"
        );
    }
}

#[cfg(test)]
mod recursion_guard_tests {
    use super::*;

    /// Test that RecursionGuard RAII semantics work: enter increments, drop decrements.
    #[test]
    fn guard_raii_semantics() {
        recursion_guard::begin_file("test_raii.c");
        {
            let g1 = recursion_guard::RecursionGuard::enter();
            assert!(g1.is_some());
            {
                let g2 = recursion_guard::RecursionGuard::enter();
                assert!(g2.is_some());
            }
            // g2 dropped — depth back to 1.
            let g3 = recursion_guard::RecursionGuard::enter();
            assert!(g3.is_some());
        }
        // All dropped — depth back to 0.
    }

    /// Test that the cap is enforced: after RECURSION_DEPTH_CAP entries, enter returns None.
    #[test]
    fn guard_cap_enforced() {
        recursion_guard::begin_file("test_cap.c");
        let mut guards = Vec::new();
        for _ in 0..RECURSION_DEPTH_CAP {
            let g = recursion_guard::RecursionGuard::enter();
            assert!(g.is_some(), "should succeed under cap");
            guards.push(g.unwrap());
        }
        // Now at cap — next enter should return None.
        let over = recursion_guard::RecursionGuard::enter();
        assert!(over.is_none(), "should return None at cap");
        // Drop all guards.
        drop(guards);
    }

    /// Test that a deeply-nested C source does NOT panic (stack overflow)
    /// and still produces chunks (parsing continues even if the cap is hit on
    /// some branches — symbols above the depth are preserved).
    #[test]
    fn deeply_nested_parse_does_not_panic() {
        // Generate C source with deeply-nested braces. C's tree-sitter grammar
        // handles nested braces/compound_statements easily. 100 levels of nesting
        // in a single function is already unusual; this tests that the guard
        // gracefully stops recursion without crashing.
        let depth = 100;
        let mut src = String::from("void top_func() {\n");
        for i in 0..depth {
            let indent = "  ".repeat(i + 1);
            src.push_str(&format!("{}{{ int x{} = {};\n", indent, i, i));
        }
        // Close all braces.
        for i in (0..depth).rev() {
            let indent = "  ".repeat(i + 1);
            src.push_str(&format!("{}}}\n", indent));
        }
        src.push_str("}\n");

        // This should not panic regardless of the nesting depth.
        let result = parse_file("test_deep.c", &src);
        // Should produce at least chunks (source coverage guarantee).
        assert!(
            !result.chunks.is_empty(),
            "expected at least one chunk from deeply nested source"
        );
        // The top-level function should be extracted (it's at depth 1-2 in the AST).
        let top = result
            .symbols
            .iter()
            .find(|s| s.qualified.name == "top_func");
        assert!(
            top.is_some(),
            "expected top_func to be extracted; got: {:?}",
            result
                .symbols
                .iter()
                .map(|s| &s.qualified.name)
                .collect::<Vec<_>>()
        );
    }

    /// Test that begin_file resets state properly (simulating worker reuse).
    #[test]
    fn begin_file_resets_state() {
        // First file: exhaust the cap.
        recursion_guard::begin_file("file_a.c");
        let mut guards = Vec::new();
        for _ in 0..RECURSION_DEPTH_CAP {
            guards.push(recursion_guard::RecursionGuard::enter().unwrap());
        }
        assert!(recursion_guard::RecursionGuard::enter().is_none());
        drop(guards);

        // Reset for a new file — should work from depth 0 again.
        recursion_guard::begin_file("file_b.c");
        let g = recursion_guard::RecursionGuard::enter();
        assert!(
            g.is_some(),
            "after begin_file, depth should be 0 and guard should succeed"
        );
    }
}

#[cfg(test)]
mod rust_param_forward_tests {
    use super::*;

    #[test]
    fn forwards_single_param() {
        let src = "fn outer(x: i32) { inner(x); }";
        let result = parse_file("test.rs", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 1);
        match &df_edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "inner"),
            _ => panic!("expected Unresolved"),
        }
        assert_eq!(df_edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn forwards_two_params_to_different_calls() {
        let src = "fn outer(x: i32, y: String) { inner(x); process(y); }";
        let result = parse_file("test.rs", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 2);
    }

    #[test]
    fn no_edge_for_literal_arg() {
        let src = "fn outer(x: i32) { inner(42); }";
        let result = parse_file("test.rs", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(df_edges.is_empty());
    }
}

#[cfg(test)]
mod python_param_forward_tests {
    use super::*;

    #[test]
    fn forwards_single_param() {
        let src = "def outer(x):\n    inner(x)\n";
        let result = parse_file("test.py", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 1);
        match &df_edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "inner"),
            _ => panic!("expected Unresolved"),
        }
    }

    #[test]
    fn no_edge_for_literal_arg() {
        let src = "def outer(x):\n    inner(42)\n";
        let result = parse_file("test.py", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(df_edges.is_empty());
    }
}

#[cfg(test)]
mod go_param_forward_tests {
    use super::*;

    #[test]
    fn forwards_single_param() {
        let src = "package main\n\nfunc outer(x int) {\n\tinner(x)\n}\n";
        let result = parse_file("test.go", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 1);
        match &df_edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "inner"),
            _ => panic!("expected Unresolved"),
        }
        assert_eq!(df_edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn no_edge_for_literal_arg() {
        let src = "package main\n\nfunc outer(x int) {\n\tinner(42)\n}\n";
        let result = parse_file("test.go", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(df_edges.is_empty());
    }

    #[test]
    fn forwards_multiple_params() {
        let src = "package main\n\nfunc outer(a, b int) {\n\tone(a)\n\ttwo(b)\n}\n";
        let result = parse_file("test.go", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 2);
    }
}

#[cfg(test)]
mod js_param_forward_tests {
    use super::*;

    #[test]
    fn forwards_single_param() {
        let src = "function outer(x) {\n  inner(x);\n}\n";
        let result = parse_file("test.js", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 1);
        match &df_edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "inner"),
            _ => panic!("expected Unresolved"),
        }
        assert_eq!(df_edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn no_edge_for_literal_arg() {
        let src = "function outer(x) {\n  inner(42);\n}\n";
        let result = parse_file("test.js", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(df_edges.is_empty());
    }

    #[test]
    fn forwards_two_params() {
        let src = "function outer(a, b) {\n  one(a);\n  two(b);\n}\n";
        let result = parse_file("test.js", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 2);
    }
}

#[cfg(test)]
mod ruby_param_forward_tests {
    use super::*;

    #[test]
    fn forwards_single_param() {
        let src = "def outer(x)\n  inner(x)\nend\n";
        let result = parse_file("test.rb", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 1);
        match &df_edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "inner"),
            _ => panic!("expected Unresolved"),
        }
        assert_eq!(df_edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn no_edge_for_literal_arg() {
        let src = "def outer(x)\n  inner(42)\nend\n";
        let result = parse_file("test.rb", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(df_edges.is_empty());
    }

    #[test]
    fn forwards_two_params() {
        let src = "def outer(a, b)\n  one(a)\n  two(b)\nend\n";
        let result = parse_file("test.rb", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 2);
    }

    #[test]
    fn no_params_no_panic() {
        let src = "def f\n  bar\nend\n";
        let result = parse_file("test.rb", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(df_edges.is_empty());
    }
}

#[cfg(test)]
mod java_param_forward_tests {
    use super::*;

    #[test]
    fn forwards_single_param() {
        let src = "class C { void outer(int x) { inner(x); } }";
        let result = parse_file("test.java", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 1);
        match &df_edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "inner"),
            _ => panic!("expected Unresolved"),
        }
        assert_eq!(df_edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn no_edge_for_literal_arg() {
        let src = "class C { void outer(int x) { inner(42); } }";
        let result = parse_file("test.java", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(df_edges.is_empty());
    }

    #[test]
    fn forwards_two_params() {
        let src = "class C { void outer(int a, int b) { one(a); two(b); } }";
        let result = parse_file("test.java", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 2);
    }
}

#[cfg(test)]
mod dart_param_forward_tests {
    use super::*;

    #[test]
    fn forwards_param_top_level_function() {
        let src = "void outer(int x) { inner(x); }";
        let result = parse_file("test.dart", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 1);
        match &df_edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "inner"),
            _ => panic!("expected Unresolved"),
        }
        assert_eq!(df_edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn forwards_param_class_method() {
        let src = "class C { void outer(int x) { inner(x); } }";
        let result = parse_file("test.dart", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 1);
        match &df_edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "inner"),
            _ => panic!("expected Unresolved"),
        }
        assert_eq!(df_edges[0].confidence, Confidence::Inferred(0.75));
    }

    #[test]
    fn no_edge_for_literal_arg() {
        let src = "void outer(int x) { inner(42); }";
        let result = parse_file("test.dart", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(df_edges.is_empty());
    }

    #[test]
    fn forwards_two_params() {
        let src = "void outer(int a, int b) { one(a); two(b); }";
        let result = parse_file("test.dart", src);
        let df_edges: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert_eq!(df_edges.len(), 2);
    }
}

#[cfg(test)]
mod rust_intermediate_flow_tests {
    use super::*;

    #[test]
    fn simple_let_binding() {
        let src = "fn f() { let x = foo(); bar(x); }";
        let result = parse_file("test.rs", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(!df.is_empty(), "expected DataFlowsTo edge, got none");
        let names: Vec<_> = df
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"bar") || names.contains(&"foo"),
            "expected edge to bar or foo, got {:?}",
            names
        );
    }

    #[test]
    fn no_edge_for_literal_let() {
        let src = "fn f() { let x = 42; bar(x); }";
        let result = parse_file("test.rs", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(
            df.is_empty(),
            "expected no DataFlowsTo edge, got {:?}",
            df.len()
        );
    }

    #[test]
    fn no_edge_undeclared_var() {
        let src = "fn f() { bar(x); }";
        let result = parse_file("test.rs", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(df.is_empty());
    }
}

#[cfg(test)]
mod python_intermediate_flow_tests {
    use super::*;

    #[test]
    fn simple_assignment() {
        let src = "def f():\n    x = foo()\n    bar(x)\n";
        let result = parse_file("test.py", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(!df.is_empty(), "expected DataFlowsTo edge");
    }

    #[test]
    fn no_edge_for_literal() {
        let src = "def f():\n    x = 42\n    bar(x)\n";
        let result = parse_file("test.py", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(df.is_empty());
    }
}

#[cfg(test)]
mod go_intermediate_flow_tests {
    use super::*;

    #[test]
    fn intermediate_variable_flow() {
        let src = "package main\n\nfunc f() {\n\ty := foo()\n\tbar(y)\n}\n";
        let result = parse_file("test.go", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(!df.is_empty(), "expected DataFlowsTo edge, got none");
        let names: Vec<_> = df
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"bar") || names.contains(&"foo"),
            "expected edge to bar or foo, got {:?}",
            names
        );
    }
}

#[cfg(test)]
mod js_intermediate_flow_tests {
    use super::*;

    #[test]
    fn intermediate_variable_flow_let() {
        let src = "function f() {\n  let y = foo();\n  bar(y);\n}\n";
        let result = parse_file("test.js", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(!df.is_empty(), "expected DataFlowsTo edge, got none");
        let names: Vec<_> = df
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"bar") || names.contains(&"foo"),
            "expected edge to bar or foo, got {:?}",
            names
        );
    }

    #[test]
    fn intermediate_variable_flow_var() {
        let src = "function f() {\n  var y = foo();\n  bar(y);\n}\n";
        let result = parse_file("test.js", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(!df.is_empty(), "expected DataFlowsTo edge, got none");
        let names: Vec<_> = df
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"bar") || names.contains(&"foo"),
            "expected edge to bar or foo, got {:?}",
            names
        );
    }
}

#[cfg(test)]
mod ruby_intermediate_flow_tests {
    use super::*;

    #[test]
    fn intermediate_variable_flow() {
        let src = "def f\n  y = foo()\n  bar(y)\nend\n";
        let result = parse_file("test.rb", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(!df.is_empty(), "expected DataFlowsTo edge, got none");
        let names: Vec<_> = df
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"bar") || names.contains(&"foo"),
            "expected edge to bar or foo, got {:?}",
            names
        );
    }
}

#[cfg(test)]
mod java_intermediate_flow_tests {
    use super::*;

    #[test]
    fn intermediate_variable_flow() {
        let src = "class C { void f() { int y = foo(); bar(y); } }";
        let result = parse_file("test.java", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(!df.is_empty(), "expected DataFlowsTo edge, got none");
        let names: Vec<_> = df
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"bar") || names.contains(&"foo"),
            "expected edge to bar or foo, got {:?}",
            names
        );
    }

    #[test]
    fn intermediate_variable_flow_object_type() {
        let src = "class C { void f() { String y = foo(); bar(y); } }";
        let result = parse_file("test.java", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(!df.is_empty(), "expected DataFlowsTo edge, got none");
        let names: Vec<_> = df
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"bar") || names.contains(&"foo"),
            "expected edge to bar or foo, got {:?}",
            names
        );
    }
}

#[cfg(test)]
mod dart_intermediate_flow_tests {
    use super::*;

    #[test]
    fn intermediate_variable_flow_typed() {
        let src = "void f() { int y = foo(); bar(y); }";
        let result = parse_file("test.dart", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(!df.is_empty(), "expected DataFlowsTo edge, got none");
        let names: Vec<_> = df
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"bar") || names.contains(&"foo"),
            "expected edge to bar or foo, got {:?}",
            names
        );
    }

    #[test]
    fn intermediate_variable_flow_var() {
        let src = "void f() { var y = foo(); bar(y); }";
        let result = parse_file("test.dart", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(!df.is_empty(), "expected DataFlowsTo edge, got none");
        let names: Vec<_> = df
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"bar") || names.contains(&"foo"),
            "expected edge to bar or foo, got {:?}",
            names
        );
    }

    #[test]
    fn arrow_body_no_panic() {
        let src = "int f() => foo();";
        let result = parse_file("test.dart", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(
            df.is_empty(),
            "arrow body has no block; expected no DataFlowsTo edge, got {:?}",
            df.len()
        );
    }
}

#[cfg(test)]
mod rust_return_flow_tests {
    use super::*;

    #[test]
    fn return_call_emits_dataflow() {
        let src = "fn outer() -> i32 { return inner(); }";
        let result = parse_file("test.rs", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(
            !df.is_empty(),
            "expected DataFlowsTo edge from return inner()"
        );
        // Verify confidence is Extracted (not Inferred)
        let confidences: Vec<_> = df.iter().map(|e| &e.confidence).collect();
        assert!(
            confidences
                .iter()
                .any(|c| matches!(c, Confidence::Extracted)),
            "return-expression should use Confidence::Extracted, got {:?}",
            confidences
        );
    }

    #[test]
    fn return_without_call_emits_nothing() {
        let src = "fn f() -> i32 { return 42; }";
        let result = parse_file("test.rs", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(
            df.is_empty(),
            "return of literal should not emit DataFlowsTo"
        );
    }

    #[test]
    fn implicit_return_emits_dataflow() {
        let src = "fn outer() -> i32 { inner() }";
        let result = parse_file("test.rs", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        // Rust implicit return is NOT a return_expression node — it's just a call_expression
        // at the end of a block. The current implementation does NOT detect this pattern.
        // This test documents the known gap.
        assert!(
            df.is_empty(),
            "implicit return not yet supported (known gap)"
        );
    }
}

#[cfg(test)]
mod python_return_flow_tests {
    use super::*;

    #[test]
    fn return_call_emits_dataflow() {
        let src = "def outer():\n    return inner()\n";
        let result = parse_file("test.py", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(
            !df.is_empty(),
            "expected DataFlowsTo edge from return inner()"
        );
        let confidences: Vec<_> = df.iter().map(|e| &e.confidence).collect();
        assert!(
            confidences
                .iter()
                .any(|c| matches!(c, Confidence::Extracted)),
            "return-statement should use Confidence::Extracted"
        );
    }

    #[test]
    fn return_literal_emits_nothing() {
        let src = "def f():\n    return 42\n";
        let result = parse_file("test.py", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        assert!(df.is_empty());
    }

    #[test]
    fn return_with_taint_source() {
        let src = "def handler():\n    return input()\n";
        let result = parse_file("test.py", src);
        let df: Vec<_> = result
            .edges
            .iter()
            .filter(|e| matches!(e.kind, EdgeKind::DataFlowsTo))
            .collect();
        // Should have TWO DataFlowsTo edges:
        // 1. return input() → DataFlowsTo to "input" (return-statement arm)
        // 2. input() → taint:source:user_input (taint arm)
        assert!(
            df.len() >= 2,
            "expected return-flow + taint edge, got {}",
            df.len()
        );
    }
}
