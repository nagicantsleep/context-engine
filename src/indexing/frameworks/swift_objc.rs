//! Swift/Objective-C cross-language resolver: detects mixed Swift+ObjC projects
//! and extracts interop edges from `@objc` annotations and ObjC message sends.
//!
//! Detection: file set contains ≥1 `.swift` AND (≥1 `.m` OR ≥1 `.h`), OR
//!            contains a file matching `*-Bridging-Header.h`.
//! Edge extraction:
//!   - Swift `@objc` annotations → Calls edges with qualifier "objc"
//!   - ObjC message sends in `.m` files → Calls edges
//!   - ObjC `#import` in `.m` files → Calls edges to header stem

use regex::Regex;
use std::sync::LazyLock;

use crate::indexing::frameworks::{DetectionContext, FrameworkResolver};
use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol, SymbolKind};

pub struct SwiftObjcResolver;

/// Matches `@objc` Swift declarations: `@objc [modifiers] func/var/class/enum/struct Name`
static SWIFT_OBJC_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"@objc\s+(?:public\s+|open\s+|internal\s+|fileprivate\s+|private\s+)*(?:func|var|class|enum|struct)\s+(\w+)",
    )
    .unwrap()
});

/// Matches `@objc("AliasName")` rename annotations
static SWIFT_OBJC_ALIAS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@objc\("(\w+)"\)"#).unwrap());

/// Matches ObjC message sends: `[Receiver methodName` or `[Receiver methodName:`
static OBJC_MSG_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[(\w+)\s+(\w+)\s*[\]:]").unwrap());

/// Matches ObjC local imports: `#import "file.h"`
static OBJC_IMPORT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"#import\s+"(\S+\.h)""#).unwrap());

impl FrameworkResolver for SwiftObjcResolver {
    fn name(&self) -> &str {
        "swift_objc"
    }

    fn detect(&self, ctx: &DetectionContext) -> bool {
        let has_swift = ctx.file_set.iter().any(|f| f.ends_with(".swift"));
        if !has_swift {
            return false;
        }
        // Bridging header alone is sufficient indicator
        if ctx
            .file_set
            .iter()
            .any(|f| f.ends_with("-Bridging-Header.h"))
        {
            return true;
        }
        // Otherwise need at least one .m or .h file alongside .swift
        ctx.file_set
            .iter()
            .any(|f| f.ends_with(".m") || f.ends_with(".h"))
    }

    fn extract_edges(&self, file_path: &str, source: &str, symbols: &[Symbol]) -> Vec<RawEdge> {
        if file_path.ends_with(".h") {
            return vec![];
        }

        let mut edges = Vec::new();

        let containing = symbols
            .iter()
            .find(|s| {
                s.qualified.file == file_path
                    && (s.kind == SymbolKind::Class || s.kind == SymbolKind::Struct)
            })
            .map(|s| s.qualified.clone())
            .unwrap_or_else(|| QualifiedSymbol {
                file: file_path.to_string(),
                scope_path: vec![],
                name: "<module>".to_string(),
            });

        if file_path.ends_with(".swift") {
            // @objc("Alias") rename annotations
            for cap in SWIFT_OBJC_ALIAS_RE.captures_iter(source) {
                let alias = &cap[1];
                let line = source[..cap.get(0).unwrap().start()]
                    .chars()
                    .filter(|&c| c == '\n')
                    .count() as u32
                    + 1;
                edges.push(RawEdge {
                    from: containing.clone(),
                    to: EdgeTarget::Unresolved {
                        name: alias.to_string(),
                        import_path: None,
                        qualifier: Some("objc".to_string()),
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Inferred(0.8),
                });
            }

            // @objc [modifiers] func/var/class/enum/struct Name
            for cap in SWIFT_OBJC_RE.captures_iter(source) {
                let sym_name = &cap[1];
                let line = source[..cap.get(0).unwrap().start()]
                    .chars()
                    .filter(|&c| c == '\n')
                    .count() as u32
                    + 1;
                edges.push(RawEdge {
                    from: containing.clone(),
                    to: EdgeTarget::Unresolved {
                        name: sym_name.to_string(),
                        import_path: None,
                        qualifier: Some("objc".to_string()),
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Inferred(0.8),
                });
            }
        } else if file_path.ends_with(".m") {
            // ObjC message sends: [Receiver method]
            for cap in OBJC_MSG_RE.captures_iter(source) {
                let receiver = &cap[1];
                let method = &cap[2];
                let line = source[..cap.get(0).unwrap().start()]
                    .chars()
                    .filter(|&c| c == '\n')
                    .count() as u32
                    + 1;
                edges.push(RawEdge {
                    from: containing.clone(),
                    to: EdgeTarget::Unresolved {
                        name: method.to_string(),
                        import_path: None,
                        qualifier: Some(receiver.to_string()),
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Inferred(0.6),
                });
            }

            // #import "Header.h" → Calls to header stem
            for cap in OBJC_IMPORT_RE.captures_iter(source) {
                let header_path = &cap[1];
                // Strip directory and .h extension
                let stem = std::path::Path::new(header_path)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(header_path)
                    .to_string();
                let line = source[..cap.get(0).unwrap().start()]
                    .chars()
                    .filter(|&c| c == '\n')
                    .count() as u32
                    + 1;
                edges.push(RawEdge {
                    from: containing.clone(),
                    to: EdgeTarget::Unresolved {
                        name: stem,
                        import_path: None,
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Inferred(0.5),
                });
            }
        }

        edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn detect_requires_swift_and_objc_files() {
        // Only .swift — not enough
        let mut file_set = HashSet::new();
        file_set.insert("Foo.swift".to_string());
        let ctx = DetectionContext {
            file_set: &file_set,
            read_file: &|_| None,
        };
        assert!(!SwiftObjcResolver.detect(&ctx));

        // .swift + .m — detected
        file_set.insert("Bar.m".to_string());
        let ctx2 = DetectionContext {
            file_set: &file_set,
            read_file: &|_| None,
        };
        assert!(SwiftObjcResolver.detect(&ctx2));

        // bridging header alone (with .swift) is enough
        let mut file_set2 = HashSet::new();
        file_set2.insert("Foo.swift".to_string());
        file_set2.insert("MyApp-Bridging-Header.h".to_string());
        let ctx3 = DetectionContext {
            file_set: &file_set2,
            read_file: &|_| None,
        };
        assert!(SwiftObjcResolver.detect(&ctx3));
    }

    #[test]
    fn extract_objc_annotation_from_swift() {
        let source = r#"
class MyClass: NSObject {
    @objc public func doSomething() {}
    @objc("renamedMethod") func renamed() {}
    @objc private var myProp: Int = 0
}
"#;
        let symbols = vec![Symbol {
            qualified: QualifiedSymbol {
                file: "MyClass.swift".to_string(),
                scope_path: vec![],
                name: "MyClass".to_string(),
            },
            kind: SymbolKind::Class,
            line_start: 2,
            line_end: 7,
            signature: None,
            parent_fqn: None,
        }];
        let edges = SwiftObjcResolver.extract_edges("MyClass.swift", source, &symbols);
        let names: Vec<&str> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"doSomething"),
            "expected doSomething in {names:?}"
        );
        assert!(
            names.contains(&"renamedMethod"),
            "expected renamedMethod in {names:?}"
        );
        assert!(names.contains(&"myProp"), "expected myProp in {names:?}");
        // All edges use Inferred(0.8)
        assert!(
            edges
                .iter()
                .all(|e| e.confidence == Confidence::Inferred(0.8))
        );
    }

    #[test]
    fn extract_objc_message_send_from_m_file() {
        let source = r#"
#import "MySwiftClass.h"
- (void)doWork {
    [MySwiftClass sharedInstance];
    [self configure:options];
}
"#;
        let symbols: Vec<Symbol> = vec![];
        let edges = SwiftObjcResolver.extract_edges("MyClass.m", source, &symbols);

        // Should have an import edge and message-send edges
        let names: Vec<&str> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(names.contains(&"MySwiftClass"), "expected header stem");
        assert!(names.contains(&"sharedInstance"), "expected method call");

        // Import edges have Inferred(0.5)
        let import_edge = edges
            .iter()
            .find(|e| matches!(&e.to, EdgeTarget::Unresolved { name, .. } if name == "MySwiftClass" && e.confidence == Confidence::Inferred(0.5)));
        assert!(import_edge.is_some());
    }

    #[test]
    fn skip_h_files() {
        let source = r#"@interface Foo : NSObject
- (void)bar;
@end"#;
        let edges = SwiftObjcResolver.extract_edges("Foo.h", source, &[]);
        assert!(edges.is_empty());
    }
}
