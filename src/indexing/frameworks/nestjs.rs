//! NestJS framework resolver: detects NestJS and extracts controller method edges.
//!
//! Detection: `package.json` contains `@nestjs/core` or `@nestjs/common`.
//! Edge extraction: HTTP method decorators (`@Get`, `@Post`, etc.) followed by a
//! method name produce a Calls edge to that controller method.

use regex::Regex;
use std::sync::LazyLock;

use crate::indexing::frameworks::{DetectionContext, FrameworkResolver};
use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol};

pub struct NestJsResolver;

/// Matches NestJS HTTP method decorators: `@Get(...)`, `@Post(...)`, etc.
static ROUTE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"@(?:Get|Post|Put|Delete|Patch|All|Options|Head)\s*\([^)]*\)").unwrap()
});

/// Matches a TypeScript method name after optional visibility/async modifiers.
static METHOD_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?:(?:public|private|protected|readonly|override|abstract)\s+)*(?:async\s+)?([a-zA-Z_]\w*)\s*\(",
    )
    .unwrap()
});

impl FrameworkResolver for NestJsResolver {
    fn name(&self) -> &str {
        "nestjs"
    }

    fn detect(&self, ctx: &DetectionContext) -> bool {
        for file in ctx.file_set.iter() {
            if file.ends_with("package.json")
                && !file.contains("node_modules")
                && let Some(content) = (ctx.read_file)(file)
                && (content.contains("\"@nestjs/core\"") || content.contains("\"@nestjs/common\""))
            {
                return true;
            }
        }
        false
    }

    fn extract_edges(&self, file_path: &str, source: &str, symbols: &[Symbol]) -> Vec<RawEdge> {
        if !file_path.ends_with(".ts") {
            return vec![];
        }

        let from_qualified = find_module_symbol(symbols, file_path);
        let mut edges = Vec::new();

        for cap in ROUTE_RE.find_iter(source) {
            let line = source[..cap.start()].chars().filter(|&c| c == '\n').count() as u32 + 1;
            let after = &source[cap.end()..];
            let lookahead = after.len().min(200);
            let stop = after[..lookahead].find('@').unwrap_or(lookahead);
            if let Some(m) = METHOD_NAME_RE.captures(&after[..stop]) {
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
    fn detect_nestjs_from_package_json() {
        let mut file_set = HashSet::new();
        file_set.insert("package.json".to_string());
        let ctx = DetectionContext {
            file_set: &file_set,
            read_file: &|_| {
                Some(
                    r#"{"dependencies": {"@nestjs/core": "^10.0.0", "@nestjs/common": "^10.0.0"}}"#
                        .to_string(),
                )
            },
        };
        assert!(NestJsResolver.detect(&ctx));
    }

    #[test]
    fn extract_controller_method_edges() {
        let source = "  @Get('/users')\n  getUsers() {}\n  @Post('/users')\n  createUser() {}\n";
        let edges = NestJsResolver.extract_edges("users.controller.ts", source, &[]);
        let names: Vec<_> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"getUsers"),
            "expected getUsers, got {:?}",
            names
        );
        assert!(
            names.contains(&"createUser"),
            "expected createUser, got {:?}",
            names
        );
    }

    #[test]
    fn skip_non_ts_files() {
        let edges = NestJsResolver.extract_edges("file.js", "@Get('/x')\ngetX() {}", &[]);
        assert!(edges.is_empty());
    }
}
