//! Next.js framework resolver: detects Next.js and extracts file-system route edges.
//!
//! Detection: `package.json` contains `next` outside `node_modules`.
//! Edge extraction: files under `app/` (`page`/`route` files) and `pages/` are
//! route endpoints; each emits an edge to its verified route path.

use crate::indexing::frameworks::{DetectionContext, FrameworkResolver};

use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol};

pub struct NextjsResolver;

impl FrameworkResolver for NextjsResolver {
    fn name(&self) -> &str {
        "nextjs"
    }

    fn detect(&self, ctx: &DetectionContext) -> bool {
        ctx.file_set.iter().any(|file| {
            file.ends_with("package.json")
                && !file.contains("node_modules")
                && (ctx.read_file)(file).is_some_and(|content| {
                    content.contains("\"next\"") || content.contains("'next'")
                })
        })
    }

    fn extract_edges(&self, file_path: &str, _source: &str, symbols: &[Symbol]) -> Vec<RawEdge> {
        let Some(route) = route_for_file(file_path) else {
            return vec![];
        };
        let from = symbols
            .iter()
            .find(|symbol| symbol.qualified.file == file_path)
            .map(|symbol| symbol.qualified.clone())
            .unwrap_or_else(|| QualifiedSymbol {
                file: file_path.to_string(),
                scope_path: vec![],
                name: "<module>".to_string(),
            });
        vec![RawEdge {
            from,
            to: EdgeTarget::Unresolved {
                name: route,
                import_path: Some(file_path.to_string()),
                qualifier: None,
            },
            kind: EdgeKind::Calls,
            line: 1,
            confidence: Confidence::Extracted,
        }]
    }
}

fn route_for_file(file_path: &str) -> Option<String> {
    let normalized = file_path.replace('\\', "/");
    if normalized.contains("node_modules/") {
        return None;
    }
    let (root, rest) = if let Some(rest) = normalized.strip_prefix("app/") {
        ("app", rest)
    } else if let Some(rest) = normalized.strip_prefix("pages/") {
        ("pages", rest)
    } else {
        return None;
    };
    let (stem, ext) = rest.rsplit_once('.')?;
    if !matches!(ext, "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs") {
        return None;
    }
    let mut route = stem.to_string();
    if root == "app" {
        route = if matches!(route.as_str(), "page" | "route") {
            String::new()
        } else {
            route
                .strip_suffix("/page")
                .or_else(|| route.strip_suffix("/route"))?
                .to_string()
        };
    } else if route == "index" {
        route.clear();
    } else if route.ends_with("/index") {
        route.truncate(route.len() - "/index".len());
    }
    Some(format!("/{}", route.trim_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn detects_next_dependency_only_in_repository_manifest() {
        let mut files = HashSet::new();
        files.insert("package.json".to_string());
        let ctx = DetectionContext {
            file_set: &files,
            read_file: &|_| Some(r#"{"dependencies":{"next":"14"}}"#.into()),
        };
        assert!(NextjsResolver.detect(&ctx));
    }

    #[test]
    fn rejects_missing_next_and_node_modules_manifest() {
        let mut files = HashSet::new();
        files.insert("package.json".to_string());
        let ctx = DetectionContext {
            file_set: &files,
            read_file: &|_| Some(r#"{"dependencies":{"react":"18"}}"#.into()),
        };
        assert!(!NextjsResolver.detect(&ctx));
        files.clear();
        files.insert("node_modules/next/package.json".to_string());
        let ctx = DetectionContext {
            file_set: &files,
            read_file: &|_| Some(r#"{"name":"next"}"#.into()),
        };
        assert!(!NextjsResolver.detect(&ctx));
    }

    #[test]
    fn extracts_app_and_pages_routes_but_not_non_routes() {
        assert_eq!(
            route_for_file("app/blog/[slug]/page.tsx"),
            Some("/blog/[slug]".into())
        );
        assert_eq!(
            route_for_file("pages/api/users.ts"),
            Some("/api/users".into())
        );
        assert!(
            NextjsResolver
                .extract_edges("src/component.tsx", "", &[])
                .is_empty()
        );
        assert!(
            NextjsResolver
                .extract_edges("app/blog/layout.tsx", "", &[])
                .is_empty()
        );
        for file in [
            "app/loading.tsx",
            "app/error.tsx",
            "app/template.tsx",
            "app/not-found.tsx",
            "app/default.tsx",
        ] {
            assert!(route_for_file(file).is_none(), "{file} is not a route");
        }
    }
}
