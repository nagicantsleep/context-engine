//! SvelteKit resolver: detects SvelteKit projects and extracts route edges.
//!
//! Detection requires a `package.json` containing the `@sveltejs/kit` package.
//! Only files under `src/routes` are considered. `load` and `actions` exports
//! produce calls edges; `+page.svelte` produces a page component edge.

use regex::Regex;
use std::sync::LazyLock;

use crate::indexing::frameworks::{DetectionContext, FrameworkResolver};
use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol};

pub struct SvelteKitResolver;

static LOAD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?m)export\s+(?:async\s+)?(?:function\s+load\b|const\s+load\s*=)|export\s*\{\s*load\b",
    )
    .unwrap()
});
static ACTIONS_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)export\s+(?:const\s+)?actions\s*=|export\s*\{\s*actions\b").unwrap()
});

impl FrameworkResolver for SvelteKitResolver {
    fn name(&self) -> &str {
        "sveltekit"
    }

    fn detect(&self, ctx: &DetectionContext) -> bool {
        ctx.file_set.iter().any(|file| {
            file.ends_with("package.json")
                && !file.contains("node_modules")
                && (ctx.read_file)(file)
                    .is_some_and(|content| content.contains("\"@sveltejs/kit\""))
        })
    }

    fn extract_edges(&self, file_path: &str, source: &str, symbols: &[Symbol]) -> Vec<RawEdge> {
        if !file_path.starts_with("src/routes/")
            || !(file_path.ends_with(".ts")
                || file_path.ends_with(".js")
                || file_path.ends_with(".svelte"))
        {
            return vec![];
        }
        let from = symbols
            .iter()
            .find(|s| s.qualified.file == file_path)
            .map(|s| s.qualified.clone())
            .unwrap_or_else(|| QualifiedSymbol {
                file: file_path.to_string(),
                scope_path: vec![],
                name: "<module>".to_string(),
            });
        let mut edges = Vec::new();
        let mut add = |name: &str, start: usize| {
            edges.push(RawEdge {
                from: from.clone(),
                to: EdgeTarget::Unresolved {
                    name: name.to_string(),
                    import_path: None,
                    qualifier: None,
                },
                kind: EdgeKind::Calls,
                line: source[..start].bytes().filter(|&b| b == b'\n').count() as u32 + 1,
                confidence: Confidence::Extracted,
            });
        };
        for m in LOAD_RE.find_iter(source) {
            add("load", m.start());
        }
        for m in ACTIONS_RE.find_iter(source) {
            add("actions", m.start());
        }
        if file_path.ends_with("/+page.svelte") {
            add("page", 0);
        }
        edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn detects_sveltekit_package_only() {
        let mut files = HashSet::new();
        files.insert("package.json".to_string());
        let ctx = DetectionContext {
            file_set: &files,
            read_file: &|_| Some(r#"{"dependencies":{"@sveltejs/kit":"^2"}}"#.to_string()),
        };
        assert!(SvelteKitResolver.detect(&ctx));
        let ctx = DetectionContext {
            file_set: &files,
            read_file: &|_| Some(r#"{"dependencies":{"svelte":"^4"}}"#.to_string()),
        };
        assert!(!SvelteKitResolver.detect(&ctx));
        files.clear();
        files.insert("node_modules/package.json".to_string());
        let ctx = DetectionContext {
            file_set: &files,
            read_file: &|_| Some(r#"{"@sveltejs/kit":"^2"}"#.to_string()),
        };
        assert!(!SvelteKitResolver.detect(&ctx));
    }

    #[test]
    fn extracts_load_actions_and_page_edges() {
        let source = "export const load = async () => ({})\nexport const actions = { default: async () => ({}) }";
        let edges = SvelteKitResolver.extract_edges("src/routes/blog/+page.ts", source, &[]);
        let names: Vec<_> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(names, vec!["load", "actions"]);
        assert_eq!(
            SvelteKitResolver
                .extract_edges("src/lib/+page.ts", source, &[])
                .len(),
            0
        );
        assert_eq!(
            SvelteKitResolver
                .extract_edges("src/routes/+page.svelte", "<h1>Hello</h1>", &[])
                .len(),
            1
        );
    }
}
