//! Rails framework resolver: detects Rails and extracts route → controller edges.
//!
//! Detection: `Gemfile` contains `gem 'rails'` / `gem "rails"`, or
//! a `config/routes.rb` file is present.
//! Edge extraction: `get '...', to: 'controller#action'` and
//! `resources :name` patterns in `config/routes.rb`.

use regex::Regex;
use std::sync::LazyLock;

use crate::indexing::frameworks::{DetectionContext, FrameworkResolver};
use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol};

pub struct RailsResolver;

/// Matches explicit `to:` routing: `get '/path', to: 'controller#action'`
static TO_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?:get|post|put|patch|delete|root)\s+(?:'[^']*'|"[^"]*")\s*,\s*to:\s*(?:'|")([a-z_]+)#([a-z_]+)(?:'|")"#,
    )
    .unwrap()
});

/// Matches `resources :name` and `resource :name`
static RESOURCES_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"resources?\s+:([a-z_]+)").unwrap());

/// Converts a snake_case controller name to a PascalCase controller class name.
/// e.g. `blog_posts` → `BlogPostsController`
fn to_controller_name(s: &str) -> String {
    let capitalized: String = s
        .split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
            }
        })
        .collect::<Vec<_>>()
        .join("");
    format!("{}Controller", capitalized)
}

impl FrameworkResolver for RailsResolver {
    fn name(&self) -> &str {
        "rails"
    }

    fn detect(&self, ctx: &DetectionContext) -> bool {
        for file in ctx.file_set.iter() {
            let fname = std::path::Path::new(file)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("");
            if fname == "Gemfile"
                && let Some(content) = (ctx.read_file)(file)
                && (content.contains("gem 'rails'") || content.contains("gem \"rails\""))
            {
                return true;
            }
            if file.ends_with("config/routes.rb") {
                return true;
            }
        }
        false
    }

    fn extract_edges(&self, file_path: &str, source: &str, symbols: &[Symbol]) -> Vec<RawEdge> {
        if !file_path.ends_with("config/routes.rb") {
            return vec![];
        }

        let from_qualified = find_module_symbol(symbols, file_path);
        let mut edges = Vec::new();

        for cap in TO_RE.captures_iter(source) {
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

        for cap in RESOURCES_RE.captures_iter(source) {
            let line = source[..cap.get(0).unwrap().start()]
                .chars()
                .filter(|&c| c == '\n')
                .count() as u32
                + 1;
            let controller = to_controller_name(&cap[1]);
            edges.push(RawEdge {
                from: from_qualified.clone(),
                to: EdgeTarget::Unresolved {
                    name: controller,
                    import_path: None,
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
    fn controller_name_conversion() {
        assert_eq!(to_controller_name("articles"), "ArticlesController");
        assert_eq!(to_controller_name("blog_posts"), "BlogPostsController");
    }

    #[test]
    fn detect_rails_from_gemfile() {
        let mut file_set = HashSet::new();
        file_set.insert("Gemfile".to_string());
        let ctx = DetectionContext {
            file_set: &file_set,
            read_file: &|_| {
                Some("source 'https://rubygems.org'\ngem 'rails', '~> 7.0'\n".to_string())
            },
        };
        assert!(RailsResolver.detect(&ctx));
    }

    #[test]
    fn detect_rails_from_routes_file() {
        let mut file_set = HashSet::new();
        file_set.insert("config/routes.rb".to_string());
        let ctx = DetectionContext {
            file_set: &file_set,
            read_file: &|_| None,
        };
        assert!(RailsResolver.detect(&ctx));
    }

    #[test]
    fn extract_to_syntax_routes() {
        let source = "get '/users', to: 'users#index'\npost '/users', to: 'users#create'\n";
        let edges = RailsResolver.extract_edges("config/routes.rb", source, &[]);
        let names: Vec<_> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(names.contains(&"index"), "got {:?}", names);
        assert!(names.contains(&"create"), "got {:?}", names);
    }

    #[test]
    fn extract_resources_routes() {
        let source = "resources :articles\n";
        let edges = RailsResolver.extract_edges("config/routes.rb", source, &[]);
        let names: Vec<_> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(names.contains(&"ArticlesController"), "got {:?}", names);
    }

    #[test]
    fn skip_non_routes_files() {
        let edges = RailsResolver.extract_edges("app/models/user.rb", "resources :users", &[]);
        assert!(edges.is_empty());
    }
}
