use crate::parsing::symbols::QualifiedSymbol;

/// Confidence of an inferred edge.
///
/// `Extracted` — directly observed in source (AST call, import, etc.).
/// `Inferred(p)` — heuristically inferred; `p` in `[0.0, 1.0]` is the
/// probability estimate (1.0 = highest confidence, 0.0 = lowest).
/// In BFS, `Extracted` is treated as weight multiplier 1.0 and always
/// takes precedence over an `Inferred` edge of equal numeric weight.
#[derive(Debug, Clone, PartialEq)]
pub enum Confidence {
    Extracted,
    Inferred(f32),
}

/// The target of an edge — either fully resolved to a `QualifiedSymbol`,
/// or unresolved (we know the name but not which file defines it).
#[derive(Debug, Clone)]
pub enum EdgeTarget {
    Resolved(QualifiedSymbol),
    Unresolved {
        name: String,
        import_path: Option<String>,
        qualifier: Option<String>,
    },
}

/// Which kind of relationship an edge represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeKind {
    Calls,
    DataFlowsTo,
    Uses,
    Imports,
    Contains,
    Implements,
}

/// A raw directed edge produced during parsing.
#[derive(Debug, Clone)]
pub struct RawEdge {
    pub from: QualifiedSymbol,
    pub to: EdgeTarget,
    pub kind: EdgeKind,
    /// Source line where the relationship occurs.
    pub line: u32,
    pub confidence: Confidence,
}
