//! FastAPI framework resolver: detects FastAPI and extracts route handler edges.
//!
//! Detection: source files import from `fastapi`, or `requirements.txt`/`pyproject.toml`
//! list `fastapi` as a dependency.
//! Edge extraction: `@app.get(...)` / `@router.post(...)` decorators followed by a
//! `def` or `async def` produce a Calls edge to the handler function.

use regex::Regex;
use std::sync::LazyLock;

use crate::indexing::frameworks::{DetectionContext, FrameworkResolver};
use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol};

pub struct FastApiResolver;

/// Matches FastAPI route decorators: `@app.get(`, `@router.post(`, etc.
static ROUTE_DECORATOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"@(?:app|router)\s*\.\s*(?:get|post|put|delete|patch|head|options)\s*\(").unwrap()
});

/// Matches a function definition: `def foo(` or `async def foo(`
static DEF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:async\s+)?def\s+([a-zA-Z_]\w*)\s*\(").unwrap());

impl FrameworkResolver for FastApiResolver {
    fn name(&self) -> &str {
        "fastapi"
    }

    fn detect(&self, ctx: &DetectionContext) -> bool {
        for file in ctx.file_set.iter() {
            if file.ends_with(".py")
                && let Some(content) = (ctx.read_file)(file)
                && (content.contains("from fastapi") || content.contains("import fastapi"))
            {
                return true;
            }
            let fname = std::path::Path::new(file)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if (fname == "requirements.txt" || fname == "pyproject.toml")
                && let Some(content) = (ctx.read_file)(file)
                && content.contains("fastapi")
            {
                return true;
            }
        }
        false
    }

    fn extract_edges(&self, file_path: &str, source: &str, symbols: &[Symbol]) -> Vec<RawEdge> {
        if !file_path.ends_with(".py") {
            return vec![];
        }

        let from_qualified = find_module_symbol(symbols, file_path);
        let mut edges = Vec::new();

        for cap in ROUTE_DECORATOR_RE.find_iter(source) {
            let line = source[..cap.start()].chars().filter(|&c| c == '\n').count() as u32 + 1;
            let after = &source[cap.end()..];
            // Search for a def within 200 chars; stop at the next decorator.
            let lookahead = after.len().min(200);
            let stop = after[..lookahead].find('@').unwrap_or(lookahead);
            if let Some(m) = DEF_RE.captures(&after[..stop]) {
                edges.push(RawEdge {
                    from: from_qualified.clone(),
                    to: EdgeTarget::Unresolved {
                        name: m[1].to_string(),
                        import_path: None,
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Extracted,
                });
            }
        }

        edges
    }
}

fn find_module_symbol(symbols: &[Symbol], file_path: &str) -> QualifiedSymbol {
    symbols
        .iter()
        .find(|s| s.qualified.file == file_path)
        .map(|s| s.qualified.clone())
        .unwrap_or_else(|| QualifiedSymbol {
            file: file_path.to_string(),
            scope_path: vec![],
            name: "<module>".to_string(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn detect_fastapi_from_import() {
        let mut file_set = HashSet::new();
        file_set.insert("app.py".to_string());
        let ctx = DetectionContext {
            file_set: &file_set,
            read_file: &|_| Some("from fastapi import FastAPI".to_string()),
        };
        assert!(FastApiResolver.detect(&ctx));
    }

    #[test]
    fn detect_fastapi_from_requirements() {
        let mut file_set = HashSet::new();
        file_set.insert("requirements.txt".to_string());
        let ctx = DetectionContext {
            file_set: &file_set,
            read_file: &|_| Some("fastapi==0.110.0\nuvicorn\n".to_string()),
        };
        assert!(FastApiResolver.detect(&ctx));
    }

    #[test]
    fn extract_route_handler_edges() {
        let source = "@app.get(\"/users\")\nasync def get_users():\n    pass\n\n@router.post(\"/items\")\nasync def create_item():\n    pass\n";
        let edges = FastApiResolver.extract_edges("routes.py", source, &[]);
        let names: Vec<_> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"get_users"),
            "expected get_users, got {:?}",
            names
        );
        assert!(
            names.contains(&"create_item"),
            "expected create_item, got {:?}",
            names
        );
    }

    #[test]
    fn skip_non_python_files() {
        let edges = FastApiResolver.extract_edges(
            "routes.ts",
            "@app.get(\"/x\")\nasync def x(): pass",
            &[],
        );
        assert!(edges.is_empty());
    }
}
