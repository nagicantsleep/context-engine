//! Expo framework resolver: detects Expo React Native projects and extracts
//! SDK import edges from JS/TS source files.
//!
//! Detection: `package.json` (not in `node_modules`) whose content contains
//!            `"expo"` or `"@expo/"`.
//! Edge extraction:
//!   - `from 'expo-pkg'` imports → Calls edge, import_path = "expo-pkg"
//!   - `from '@expo/pkg'` imports → Calls edge, import_path = "@expo/pkg"
//!   - Named `{ A, B } from 'expo'` imports → one Calls edge per name
//!   - Default `import Foo from 'expo'` → Calls edge

use regex::Regex;
use std::sync::LazyLock;

use crate::indexing::frameworks::{DetectionContext, FrameworkResolver};
use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::Symbol;
use crate::parsing::symbols::{QualifiedSymbol, SymbolKind};

pub struct ExpoResolver;

/// Matches `from 'expo-something'` or `from "expo-something"`
static EXPO_PKG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"from\s+['"]expo-([^'"]+)['"]"#).unwrap());

/// Matches `from '@expo/something'` or `from "@expo/something"`
static EXPO_SCOPED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"from\s+['"]@expo/([^'"]+)['"]"#).unwrap());

/// Matches named imports `{ A, B as C } from 'expo'`
static EXPO_NAMED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\{\s*([^}]+)\s*\}\s+from\s+['"]expo['"]"#).unwrap());

/// Matches default import `import Foo from 'expo'`
static EXPO_DEFAULT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"import\s+(\w+)\s+from\s+['"]expo['"]"#).unwrap());

impl FrameworkResolver for ExpoResolver {
    fn name(&self) -> &str {
        "expo"
    }

    fn detect(&self, ctx: &DetectionContext) -> bool {
        for file in ctx.file_set.iter() {
            if file.ends_with("package.json")
                && !file.contains("node_modules")
                && let Some(content) = (ctx.read_file)(file)
                && (content.contains("\"expo\"") || content.contains("\"@expo/"))
            {
                return true;
            }
        }
        false
    }

    fn extract_edges(&self, file_path: &str, source: &str, symbols: &[Symbol]) -> Vec<RawEdge> {
        if !file_path.ends_with(".tsx")
            && !file_path.ends_with(".ts")
            && !file_path.ends_with(".jsx")
            && !file_path.ends_with(".js")
        {
            return vec![];
        }

        let mut edges = Vec::new();

        // Find enclosing component (function or method in this file)
        let from_symbol = find_enclosing_component(symbols, file_path);
        let from_qualified = from_symbol
            .as_ref()
            .map(|s| s.qualified.clone())
            .unwrap_or_else(|| QualifiedSymbol {
                file: file_path.to_string(),
                scope_path: vec![],
                name: "<module>".to_string(),
            });

        // `from 'expo-pkg'` — package-level import
        for cap in EXPO_PKG_RE.captures_iter(source) {
            let pkg_suffix = &cap[1];
            let import_path = format!("expo-{}", pkg_suffix);
            let line = source[..cap.get(0).unwrap().start()]
                .chars()
                .filter(|&c| c == '\n')
                .count() as u32
                + 1;
            edges.push(RawEdge {
                from: from_qualified.clone(),
                to: EdgeTarget::Unresolved {
                    name: import_path.clone(),
                    import_path: Some(import_path),
                    qualifier: None,
                },
                kind: EdgeKind::Calls,
                line,
                confidence: Confidence::Inferred(0.9),
            });
        }

        // `from '@expo/pkg'` — scoped package import
        for cap in EXPO_SCOPED_RE.captures_iter(source) {
            let pkg_suffix = &cap[1];
            let import_path = format!("@expo/{}", pkg_suffix);
            let line = source[..cap.get(0).unwrap().start()]
                .chars()
                .filter(|&c| c == '\n')
                .count() as u32
                + 1;
            edges.push(RawEdge {
                from: from_qualified.clone(),
                to: EdgeTarget::Unresolved {
                    name: import_path.clone(),
                    import_path: Some(import_path),
                    qualifier: None,
                },
                kind: EdgeKind::Calls,
                line,
                confidence: Confidence::Inferred(0.9),
            });
        }

        // Named imports `{ A, B as C } from 'expo'`
        for cap in EXPO_NAMED_RE.captures_iter(source) {
            let specifiers = &cap[1];
            let line = source[..cap.get(0).unwrap().start()]
                .chars()
                .filter(|&c| c == '\n')
                .count() as u32
                + 1;
            for specifier in specifiers.split(',') {
                // Handle `Name as Alias` — use the original name (before `as`)
                let name = specifier
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                edges.push(RawEdge {
                    from: from_qualified.clone(),
                    to: EdgeTarget::Unresolved {
                        name,
                        import_path: Some("expo".to_string()),
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Inferred(0.85),
                });
            }
        }

        // Default import `import Foo from 'expo'`
        for cap in EXPO_DEFAULT_RE.captures_iter(source) {
            let name = &cap[1];
            let line = source[..cap.get(0).unwrap().start()]
                .chars()
                .filter(|&c| c == '\n')
                .count() as u32
                + 1;
            edges.push(RawEdge {
                from: from_qualified.clone(),
                to: EdgeTarget::Unresolved {
                    name: name.to_string(),
                    import_path: Some("expo".to_string()),
                    qualifier: None,
                },
                kind: EdgeKind::Calls,
                line,
                confidence: Confidence::Inferred(0.85),
            });
        }

        edges
    }
}

/// Find the first function/method symbol in the file (best-effort enclosing context).
fn find_enclosing_component(symbols: &[Symbol], file_path: &str) -> Option<Symbol> {
    symbols
        .iter()
        .find(|s| {
            s.qualified.file == file_path
                && matches!(s.kind, SymbolKind::Function | SymbolKind::Method)
        })
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn detect_expo_from_package_json() {
        // package.json with "expo" dependency
        let mut file_set = HashSet::new();
        file_set.insert("package.json".to_string());
        let ctx = DetectionContext {
            file_set: &file_set,
            read_file: &|_| {
                Some(
                    r#"{"dependencies": {"expo": "~49.0.0", "react-native": "0.72.0"}}"#
                        .to_string(),
                )
            },
        };
        assert!(ExpoResolver.detect(&ctx));

        // package.json with @expo/ scoped package
        let ctx2 = DetectionContext {
            file_set: &file_set,
            read_file: &|_| {
                Some(r#"{"dependencies": {"@expo/vector-icons": "^13.0.0"}}"#.to_string())
            },
        };
        assert!(ExpoResolver.detect(&ctx2));

        // No expo — not detected
        let ctx3 = DetectionContext {
            file_set: &file_set,
            read_file: &|_| Some(r#"{"dependencies": {"react": "^18.0.0"}}"#.to_string()),
        };
        assert!(!ExpoResolver.detect(&ctx3));

        // node_modules package.json should be ignored
        let mut file_set2 = HashSet::new();
        file_set2.insert("node_modules/expo/package.json".to_string());
        let ctx4 = DetectionContext {
            file_set: &file_set2,
            read_file: &|_| Some(r#"{"name": "expo"}"#.to_string()),
        };
        assert!(!ExpoResolver.detect(&ctx4));
    }

    #[test]
    fn extract_expo_named_imports() {
        let source = r#"
import { Camera, useCameraPermissions } from 'expo';
import { Audio as ExpoAudio } from 'expo';
"#;
        let edges = ExpoResolver.extract_edges("App.tsx", source, &[]);
        let names: Vec<&str> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(names.contains(&"Camera"), "expected Camera in {names:?}");
        assert!(
            names.contains(&"useCameraPermissions"),
            "expected useCameraPermissions in {names:?}"
        );
        assert!(
            names.contains(&"Audio"),
            "expected Audio (before `as`) in {names:?}"
        );

        // All should have import_path = "expo" and Inferred(0.85)
        for edge in &edges {
            assert_eq!(edge.confidence, Confidence::Inferred(0.85));
            if let EdgeTarget::Unresolved { import_path, .. } = &edge.to {
                assert_eq!(import_path.as_deref(), Some("expo"));
            }
        }
    }

    #[test]
    fn extract_expo_scoped_package_imports() {
        let source = r#"
import { useFont } from 'expo-font';
import something from '@expo/vector-icons';
"#;
        let edges = ExpoResolver.extract_edges("Screen.tsx", source, &[]);
        let names: Vec<&str> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"expo-font"),
            "expected expo-font in {names:?}"
        );
        assert!(
            names.contains(&"@expo/vector-icons"),
            "expected @expo/vector-icons in {names:?}"
        );

        // expo-font edge should have Inferred(0.9)
        let font_edge = edges
            .iter()
            .find(|e| matches!(&e.to, EdgeTarget::Unresolved { name, .. } if name == "expo-font"));
        assert!(font_edge.is_some());
        assert_eq!(font_edge.unwrap().confidence, Confidence::Inferred(0.9));
    }

    #[test]
    fn skip_non_js_ts_files() {
        let source = r#"import { Camera } from 'expo';"#;
        let edges = ExpoResolver.extract_edges("App.rs", source, &[]);
        assert!(edges.is_empty());

        let edges2 = ExpoResolver.extract_edges("README.md", source, &[]);
        assert!(edges2.is_empty());
    }
}
