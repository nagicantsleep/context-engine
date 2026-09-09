//! Angular framework resolver: detects Angular and extracts decorator/module edges.
//!
//! Detection: `package.json` contains `@angular/core` or `@angular/common`.
//! Edge extraction: Angular `@Component` and `@NgModule` decorators followed by
//! a class name produce Calls edges from the file module to that class.

use regex::Regex;
use std::sync::LazyLock;

use crate::indexing::frameworks::{DetectionContext, FrameworkResolver};
use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol};

pub struct AngularResolver;

static DECORATOR_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@(?:Component|NgModule)\s*(?:\([^)]*\))?").unwrap());
static CLASS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bclass\s+([A-Za-z_$][\w$]*)").unwrap());

impl FrameworkResolver for AngularResolver {
    fn name(&self) -> &str {
        "angular"
    }

    fn detect(&self, ctx: &DetectionContext) -> bool {
        ctx.file_set.iter().any(|file| {
            file.ends_with("package.json")
                && !file.contains("node_modules")
                && (ctx.read_file)(file).is_some_and(|content| {
                    content.contains("\"@angular/core\"") || content.contains("\"@angular/common\"")
                })
        })
    }

    fn extract_edges(&self, file_path: &str, source: &str, _symbols: &[Symbol]) -> Vec<RawEdge> {
        if !file_path.ends_with(".ts") {
            return vec![];
        }

        let from = QualifiedSymbol {
            file: file_path.to_string(),
            scope_path: vec![],
            name: "<module>".to_string(),
        };
        DECORATOR_RE
            .find_iter(source)
            .filter_map(|decorator| {
                let remainder = &source[decorator.end()..];
                let class_match = CLASS_RE.find(remainder)?;
                let name = CLASS_RE.captures(class_match.as_str())?[1].to_string();
                let line = source[..decorator.start()]
                    .chars()
                    .filter(|&c| c == '\n')
                    .count() as u32
                    + 1;
                Some(RawEdge {
                    from: from.clone(),
                    to: EdgeTarget::Unresolved {
                        name,
                        import_path: None,
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Extracted,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn detect_angular_from_package_json() {
        let files = HashSet::from(["package.json".to_string()]);
        let ctx = DetectionContext {
            file_set: &files,
            read_file: &|_| Some(r#"{"dependencies":{"@angular/core":"^18.0.0"}}"#.to_string()),
        };
        assert!(AngularResolver.detect(&ctx));
    }

    #[test]
    fn no_angular_without_dependency() {
        let files = HashSet::from(["package.json".to_string()]);
        let ctx = DetectionContext {
            file_set: &files,
            read_file: &|_| Some(r#"{"dependencies":{"rxjs":"^7.0.0"}}"#.to_string()),
        };
        assert!(!AngularResolver.detect(&ctx));
    }

    #[test]
    fn extract_decorator_class_edges_only_from_typescript() {
        let source = "@Component({selector: 'app-root'})\nexport class AppComponent {}\n\n@NgModule({})\nclass AppModule {}";
        let edges = AngularResolver.extract_edges("src/app.ts", source, &[]);
        let names: Vec<_> = edges
            .iter()
            .filter_map(|edge| match &edge.to {
                EdgeTarget::Unresolved { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, vec!["AppComponent", "AppModule"]);
        assert!(
            AngularResolver
                .extract_edges("src/app.js", source, &[])
                .is_empty()
        );
    }
}
