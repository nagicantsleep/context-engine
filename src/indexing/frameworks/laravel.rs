//! Laravel framework resolver: detects Laravel and extracts route → controller edges.
//!
//! Detection: `composer.json` contains `laravel/framework` or `laravel/laravel`,
//! or an `artisan` file is present.
//! Edge extraction: `Route::get('...', [Controller::class, 'method'])` and
//! `Route::get('...', 'Controller@method')` patterns.

use regex::Regex;
use std::sync::LazyLock;

use crate::indexing::frameworks::{DetectionContext, FrameworkResolver};
use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol};

pub struct LaravelResolver;

/// Matches array-style route: `Route::get('/path', [FooController::class, 'method'])`
static ROUTE_ARRAY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"Route::\w+\s*\(\s*(?:'[^']*'|"[^"]*")\s*,\s*\[\s*([A-Z]\w*)::class\s*,\s*'([a-zA-Z_]\w*)'"#,
    )
    .unwrap()
});

/// Matches string-style route: `Route::get('/path', 'FooController@method')`
static ROUTE_STRING_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"Route::\w+\s*\(\s*(?:'[^']*'|"[^"]*")\s*,\s*'([A-Z]\w*)@([a-zA-Z_]\w*)'"#)
        .unwrap()
});

impl FrameworkResolver for LaravelResolver {
    fn name(&self) -> &str {
        "laravel"
    }

    fn detect(&self, ctx: &DetectionContext) -> bool {
        for file in ctx.file_set.iter() {
            let fname = std::path::Path::new(file)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if fname == "composer.json"
                && let Some(content) = (ctx.read_file)(file)
                && (content.contains("\"laravel/framework\"")
                    || content.contains("\"laravel/laravel\""))
            {
                return true;
            }
            if fname == "artisan" {
                return true;
            }
        }
        false
    }

    fn extract_edges(&self, file_path: &str, source: &str, symbols: &[Symbol]) -> Vec<RawEdge> {
        if !file_path.ends_with(".php") {
            return vec![];
        }

        let from_qualified = find_module_symbol(symbols, file_path);
        let mut edges = Vec::new();

        for cap in ROUTE_ARRAY_RE.captures_iter(source) {
            let line = source[..cap.get(0).unwrap().start()]
                .chars()
                .filter(|&c| c == '\n')
                .count() as u32
                + 1;
            edges.push(RawEdge {
                from: from_qualified.clone(),
                to: EdgeTarget::Unresolved {
                    name: cap[2].to_string(),
                    import_path: Some(cap[1].to_string()),
                    qualifier: None,
                },
                kind: EdgeKind::Calls,
                line,
                confidence: Confidence::Extracted,
            });
        }

        for cap in ROUTE_STRING_RE.captures_iter(source) {
            let line = source[..cap.get(0).unwrap().start()]
                .chars()
                .filter(|&c| c == '\n')
                .count() as u32
                + 1;
            edges.push(RawEdge {
                from: from_qualified.clone(),
                to: EdgeTarget::Unresolved {
                    name: cap[2].to_string(),
                    import_path: Some(cap[1].to_string()),
                    qualifier: None,
                },
                kind: EdgeKind::Calls,
                line,
                confidence: Confidence::Extracted,
            });
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
    fn detect_laravel_from_composer_json() {
        let mut file_set = HashSet::new();
        file_set.insert("composer.json".to_string());
        let ctx = DetectionContext {
            file_set: &file_set,
            read_file: &|_| Some(r#"{"require": {"laravel/framework": "^10.0"}}"#.to_string()),
        };
        assert!(LaravelResolver.detect(&ctx));
    }

    #[test]
    fn detect_laravel_from_artisan() {
        let mut file_set = HashSet::new();
        file_set.insert("artisan".to_string());
        let ctx = DetectionContext {
            file_set: &file_set,
            read_file: &|_| None,
        };
        assert!(LaravelResolver.detect(&ctx));
    }

    #[test]
    fn extract_array_syntax_routes() {
        let source = "Route::get('/users', [UserController::class, 'index']);\nRoute::post('/users', [UserController::class, 'store']);\n";
        let edges = LaravelResolver.extract_edges("routes/web.php", source, &[]);
        let names: Vec<_> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(names.contains(&"index"), "expected index, got {:?}", names);
        assert!(names.contains(&"store"), "expected store, got {:?}", names);
    }

    #[test]
    fn extract_string_syntax_routes() {
        let source = "Route::get('/posts', 'PostController@index');\nRoute::post('/posts', 'PostController@store');\n";
        let edges = LaravelResolver.extract_edges("routes/web.php", source, &[]);
        let names: Vec<_> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(names.contains(&"index"), "expected index, got {:?}", names);
        assert!(names.contains(&"store"), "expected store, got {:?}", names);
    }

    #[test]
    fn skip_non_php_files() {
        let edges = LaravelResolver.extract_edges(
            "routes/web.js",
            "Route::get('/x', [Foo::class, 'bar']);",
            &[],
        );
        assert!(edges.is_empty());
    }
}
