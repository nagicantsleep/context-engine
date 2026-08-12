//! Flutter framework resolver: detects Flutter projects and extracts
//! call edges from Dart files.
//!
//! Detection: `pubspec.yaml` contains `flutter:` as a top-level key or
//! lists `flutter` as an SDK dependency.
//! Edge extraction: Navigator routes, GoRouter routes, widget build trees,
//! and state-management patterns (Provider, Riverpod, BLoC).

use regex::Regex;
use std::sync::LazyLock;

use crate::indexing::frameworks::{DetectionContext, FrameworkResolver};
use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol, SymbolKind};

pub struct FlutterResolver;

// Navigator.push with MaterialPageRoute builder → screen widget class
static NAV_PUSH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"Navigator\.push\s*\(\s*\w+\s*,\s*MaterialPageRoute\s*\(\s*builder:\s*\([^)]*\)\s*=>\s*([A-Z][A-Za-z0-9]*)\s*\(").unwrap()
});

// Navigator.pushNamed → route string
static NAV_PUSH_NAMED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"Navigator\.pushNamed\s*\(\s*\w+\s*,\s*(?:'([^']*)'|"([^"]*)")"#).unwrap()
});

// GoRoute path string (defined for completeness; path strings are skipped
// in favour of the builder-target widget — see GOROUTER_BUILDER_RE)
#[allow(dead_code)]
static GOROUTER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"GoRoute\s*\([^)]*path:\s*(?:'([^']*)'|"([^"]*)")"#).unwrap());

// GoRoute builder → widget class
static GOROUTER_BUILDER_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"GoRoute\s*\([^)]*builder:\s*\([^)]*\)\s*=>\s*([A-Z][A-Za-z0-9]*)\s*\(").unwrap()
});

// Widget named args → child widget class (inside build methods)
static WIDGET_CHILD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:child|children|body|home|routes|pages|bottom|drawer|appBar|leading|trailing|title|actions|floatingActionButton|persistentFooterButtons|sliver|delegate):\s*([A-Z][A-Za-z0-9]*)\s*\(").unwrap()
});

// State management: Provider, Riverpod, BLoC
static STATE_MGMT_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:Provider\.of\s*<([A-Z][A-Za-z0-9]*)>|context\.(?:read|watch)\s*<([A-Z][A-Za-z0-9]*)>|ref\.(?:read|watch)\s*\(\s*([a-z][A-Za-z0-9]*[Pp]rovider)\s*\)|BlocProvider\s*\([^)]*create:\s*\([^)]*\)\s*=>\s*([A-Z][A-Za-z0-9]*)\s*\(\s*\)|BlocBuilder\s*<\s*([A-Z][A-Za-z0-9]*)\s*,)").unwrap()
});

/// Find the deepest Method or Function symbol in `file_path` whose `line_start`
/// is <= `line`. Falls back to a `<module>` sentinel if none is found.
fn find_enclosing_method(symbols: &[Symbol], file_path: &str, line: u32) -> QualifiedSymbol {
    symbols
        .iter()
        .filter(|s| {
            s.qualified.file == file_path
                && matches!(s.kind, SymbolKind::Method | SymbolKind::Function)
                && s.line_start <= line
        })
        .max_by_key(|s| s.line_start)
        .map(|s| s.qualified.clone())
        .unwrap_or_else(|| QualifiedSymbol {
            file: file_path.to_string(),
            scope_path: vec![],
            name: "<module>".to_string(),
        })
}

impl FrameworkResolver for FlutterResolver {
    fn name(&self) -> &str {
        "flutter"
    }

    fn detect(&self, ctx: &DetectionContext) -> bool {
        for file in ctx.file_set.iter() {
            if file.ends_with("pubspec.yaml")
                && let Some(content) = (ctx.read_file)(file)
                && (content.contains("  flutter:")
                    || content.contains("\nflutter:")
                    || content.contains("flutter_test:")
                    || content.contains("sdk: flutter"))
            {
                return true;
            }
        }
        false
    }

    fn extract_edges(&self, file_path: &str, source: &str, symbols: &[Symbol]) -> Vec<RawEdge> {
        if !file_path.ends_with(".dart") {
            return vec![];
        }

        let mut edges = Vec::new();

        // 1. Navigator.push with MaterialPageRoute builder → screen widget class
        for cap in NAV_PUSH_RE.captures_iter(source) {
            let line = source[..cap.get(0).unwrap().start()]
                .chars()
                .filter(|&c| c == '\n')
                .count() as u32
                + 1;
            let from = find_enclosing_method(symbols, file_path, line);
            edges.push(RawEdge {
                from,
                to: EdgeTarget::Unresolved {
                    name: cap[1].to_string(),
                    import_path: None,
                    qualifier: None,
                },
                kind: EdgeKind::Calls,
                line,
                confidence: Confidence::Extracted,
            });
        }

        // 2. Navigator.pushNamed → route string
        for cap in NAV_PUSH_NAMED_RE.captures_iter(source) {
            let route = if cap.get(1).map_or("", |m| m.as_str()).is_empty() {
                cap.get(2).map_or("", |m| m.as_str())
            } else {
                cap.get(1).map_or("", |m| m.as_str())
            };
            if route.is_empty() {
                continue;
            }
            let line = source[..cap.get(0).unwrap().start()]
                .chars()
                .filter(|&c| c == '\n')
                .count() as u32
                + 1;
            let from = find_enclosing_method(symbols, file_path, line);
            edges.push(RawEdge {
                from,
                to: EdgeTarget::Unresolved {
                    name: route.to_string(),
                    import_path: None,
                    qualifier: None,
                },
                kind: EdgeKind::Calls,
                line,
                confidence: Confidence::Extracted,
            });
        }

        // 3. GoRoute builder → widget class (path strings skipped per spec)
        for cap in GOROUTER_BUILDER_RE.captures_iter(source) {
            let line = source[..cap.get(0).unwrap().start()]
                .chars()
                .filter(|&c| c == '\n')
                .count() as u32
                + 1;
            let from = find_enclosing_method(symbols, file_path, line);
            edges.push(RawEdge {
                from,
                to: EdgeTarget::Unresolved {
                    name: cap[1].to_string(),
                    import_path: None,
                    qualifier: None,
                },
                kind: EdgeKind::Calls,
                line,
                confidence: Confidence::Extracted,
            });
        }

        // 4. Widget named-arg children (child:, body:, home:, ...)
        for cap in WIDGET_CHILD_RE.captures_iter(source) {
            let line = source[..cap.get(0).unwrap().start()]
                .chars()
                .filter(|&c| c == '\n')
                .count() as u32
                + 1;
            let from = find_enclosing_method(symbols, file_path, line);
            edges.push(RawEdge {
                from,
                to: EdgeTarget::Unresolved {
                    name: cap[1].to_string(),
                    import_path: None,
                    qualifier: None,
                },
                kind: EdgeKind::Calls,
                line,
                confidence: Confidence::Inferred(0.85),
            });
        }

        // 5. State management: Provider, Riverpod, BLoC
        for cap in STATE_MGMT_RE.captures_iter(source) {
            // Take whichever capture group is non-empty (groups 1-5)
            let name = (1..=5)
                .find_map(|i| cap.get(i).map(|m| m.as_str()).filter(|s| !s.is_empty()))
                .unwrap_or("");
            if name.is_empty() {
                continue;
            }
            let line = source[..cap.get(0).unwrap().start()]
                .chars()
                .filter(|&c| c == '\n')
                .count() as u32
                + 1;
            let from = find_enclosing_method(symbols, file_path, line);
            edges.push(RawEdge {
                from,
                to: EdgeTarget::Unresolved {
                    name: name.to_string(),
                    import_path: None,
                    qualifier: None,
                },
                kind: EdgeKind::Uses,
                line,
                confidence: Confidence::Inferred(0.9),
            });
        }

        edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn make_file_set(paths: &[&str]) -> HashSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    fn make_symbol(file: &str, name: &str, line_start: u32) -> Symbol {
        Symbol {
            qualified: QualifiedSymbol {
                file: file.to_string(),
                scope_path: vec![],
                name: name.to_string(),
            },
            kind: SymbolKind::Function,
            line_start,
            line_end: line_start + 10,
            signature: None,
            parent_fqn: None,
        }
    }

    #[test]
    fn find_enclosing_method_picks_closest_preceding_symbol() {
        let symbols = vec![
            make_symbol("lib/main.dart", "outerMethod", 5),
            make_symbol("lib/main.dart", "innerMethod", 20),
            make_symbol("lib/other.dart", "otherMethod", 15),
        ];
        let found = find_enclosing_method(&symbols, "lib/main.dart", 25);
        assert_eq!(found.name, "innerMethod");
    }

    #[test]
    fn find_enclosing_method_falls_back_to_module_sentinel() {
        let symbols = vec![make_symbol("lib/main.dart", "laterMethod", 30)];
        let found = find_enclosing_method(&symbols, "lib/main.dart", 5);
        assert_eq!(found.name, "<module>");
    }

    #[test]
    fn detect_flutter_from_pubspec() {
        let file_set = make_file_set(&["pubspec.yaml"]);
        let ctx = DetectionContext {
            file_set: &file_set,
            read_file: &|_| {
                Some("name: my_app\n  flutter:\n    uses-material-design: true\n".to_string())
            },
        };
        assert!(FlutterResolver.detect(&ctx));
    }

    #[test]
    fn no_flutter_without_flutter_key() {
        let file_set = make_file_set(&["pubspec.yaml"]);
        let ctx = DetectionContext {
            file_set: &file_set,
            read_file: &|_| Some("name: my_lib\nversion: 1.0.0\n".to_string()),
        };
        assert!(!FlutterResolver.detect(&ctx));
    }

    #[test]
    fn extract_navigator_push_edge() {
        let source = "Navigator.push(context, MaterialPageRoute(builder: (ctx) => HomeScreen()));";
        let edges = FlutterResolver.extract_edges("lib/main.dart", source, &[]);
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "HomeScreen"),
            _ => panic!("expected Unresolved"),
        }
        assert_eq!(edges[0].kind, EdgeKind::Calls);
        assert_eq!(edges[0].confidence, Confidence::Extracted);
    }

    #[test]
    fn extract_navigator_push_named_edge() {
        let source = "Navigator.pushNamed(context, '/home');";
        let edges = FlutterResolver.extract_edges("lib/main.dart", source, &[]);
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "/home"),
            _ => panic!("expected Unresolved"),
        }
    }

    #[test]
    fn extract_gorouter_builder_edge() {
        let source = r#"GoRoute(path: '/profile', builder: (ctx, state) => ProfileScreen())"#;
        let edges = FlutterResolver.extract_edges("lib/router.dart", source, &[]);
        assert!(!edges.is_empty());
        let names: Vec<&str> = edges
            .iter()
            .filter_map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"ProfileScreen"));
    }

    #[test]
    fn extract_widget_child_edge() {
        let source = "Scaffold(\n  body: MyWidget(\n    text: 'hello',\n  ),\n)";
        let edges = FlutterResolver.extract_edges("lib/screen.dart", source, &[]);
        assert!(!edges.is_empty());
        let names: Vec<&str> = edges
            .iter()
            .filter_map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"MyWidget"));
    }

    #[test]
    fn extract_provider_of_edge() {
        let source = "final vm = Provider.of<UserViewModel>(context);";
        let edges = FlutterResolver.extract_edges("lib/page.dart", source, &[]);
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "UserViewModel"),
            _ => panic!("expected Unresolved"),
        }
        assert_eq!(edges[0].kind, EdgeKind::Uses);
    }

    #[test]
    fn extract_riverpod_ref_watch() {
        let source = "final user = ref.watch(userProvider);";
        let edges = FlutterResolver.extract_edges("lib/page.dart", source, &[]);
        assert_eq!(edges.len(), 1);
        match &edges[0].to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "userProvider"),
            _ => panic!("expected Unresolved"),
        }
        assert_eq!(edges[0].kind, EdgeKind::Uses);
    }

    #[test]
    fn extract_bloc_provider_edge() {
        let source = "BlocProvider(create: (ctx) => AuthBloc())";
        let edges = FlutterResolver.extract_edges("lib/app.dart", source, &[]);
        assert!(!edges.is_empty());
        let names: Vec<&str> = edges
            .iter()
            .filter_map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"AuthBloc"));
    }

    #[test]
    fn skip_non_dart_file() {
        let source = "Navigator.push(context, MaterialPageRoute(builder: (ctx) => HomeScreen()));";
        let edges = FlutterResolver.extract_edges("main.kt", source, &[]);
        assert!(edges.is_empty());
    }
}
