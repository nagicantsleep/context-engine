//! Java/Kotlin cross-language resolver: detects mixed Java+Kotlin projects
//! and extracts JVM interop edges from `@Jvm*` annotations and `*Kt` class usage.
//!
//! Detection: file set contains ≥1 `.java` AND ≥1 `.kt` file.
//! Edge extraction:
//!   - Kotlin `@JvmStatic`/`@JvmField`/`@JvmOverloads`/`@JvmName` → Calls edges
//!   - Java imports of generated `*Kt` classes → Calls edges
//!   - Java `Companion.method()` calls → Calls edges

use regex::Regex;
use std::sync::LazyLock;

use crate::indexing::frameworks::{DetectionContext, FrameworkResolver};
use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::{QualifiedSymbol, Symbol, SymbolKind};

pub struct JavaKotlinResolver;

/// Matches `@JvmStatic fun/val/var Name` in Kotlin
static KT_JVM_STATIC_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@JvmStatic\s+(?:fun|val|var)\s+(\w+)").unwrap());

/// Matches `@JvmField val/var Name` in Kotlin
static KT_JVM_FIELD_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@JvmField\s+(?:val|var)\s+(\w+)").unwrap());

/// Matches `@JvmOverloads fun Name` in Kotlin
static KT_JVM_OVERLOADS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"@JvmOverloads\s+fun\s+(\w+)").unwrap());

/// Matches `@JvmName("alias")` in Kotlin
static KT_JVM_NAME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"@JvmName\s*\(\s*"(\w+)"\s*\)"#).unwrap());

/// Matches Java `import com.example.FooKt;`
static JAVA_KT_IMPORT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"import\s+([\w.]+Kt)\s*;").unwrap());

/// Matches Java `SomeClass.Companion.method(` calls
static JAVA_COMPANION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"([\w.]+)Companion\.(\w+)\s*\(").unwrap());

impl FrameworkResolver for JavaKotlinResolver {
    fn name(&self) -> &str {
        "java_kotlin"
    }

    fn detect(&self, ctx: &DetectionContext) -> bool {
        let has_java = ctx.file_set.iter().any(|f| f.ends_with(".java"));
        let has_kotlin = ctx.file_set.iter().any(|f| f.ends_with(".kt"));
        has_java && has_kotlin
    }

    fn extract_edges(&self, file_path: &str, source: &str, symbols: &[Symbol]) -> Vec<RawEdge> {
        if !file_path.ends_with(".java") && !file_path.ends_with(".kt") {
            return vec![];
        }

        let mut edges = Vec::new();

        let containing = symbols
            .iter()
            .find(|s| s.qualified.file == file_path && s.kind == SymbolKind::Class)
            .map(|s| s.qualified.clone())
            .unwrap_or_else(|| QualifiedSymbol {
                file: file_path.to_string(),
                scope_path: vec![],
                name: "<module>".to_string(),
            });

        if file_path.ends_with(".kt") {
            // @JvmStatic
            for cap in KT_JVM_STATIC_RE.captures_iter(source) {
                let name = &cap[1];
                let line = source[..cap.get(0).unwrap().start()]
                    .chars()
                    .filter(|&c| c == '\n')
                    .count() as u32
                    + 1;
                edges.push(RawEdge {
                    from: containing.clone(),
                    to: EdgeTarget::Unresolved {
                        name: name.to_string(),
                        import_path: None,
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Inferred(0.8),
                });
            }

            // @JvmField
            for cap in KT_JVM_FIELD_RE.captures_iter(source) {
                let name = &cap[1];
                let line = source[..cap.get(0).unwrap().start()]
                    .chars()
                    .filter(|&c| c == '\n')
                    .count() as u32
                    + 1;
                edges.push(RawEdge {
                    from: containing.clone(),
                    to: EdgeTarget::Unresolved {
                        name: name.to_string(),
                        import_path: None,
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Inferred(0.8),
                });
            }

            // @JvmOverloads
            for cap in KT_JVM_OVERLOADS_RE.captures_iter(source) {
                let name = &cap[1];
                let line = source[..cap.get(0).unwrap().start()]
                    .chars()
                    .filter(|&c| c == '\n')
                    .count() as u32
                    + 1;
                edges.push(RawEdge {
                    from: containing.clone(),
                    to: EdgeTarget::Unresolved {
                        name: name.to_string(),
                        import_path: None,
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Inferred(0.75),
                });
            }

            // @JvmName("alias")
            for cap in KT_JVM_NAME_RE.captures_iter(source) {
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
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Inferred(0.8),
                });
            }
        } else if file_path.ends_with(".java") {
            // import com.example.FooKt;
            for cap in JAVA_KT_IMPORT_RE.captures_iter(source) {
                let full_import = &cap[1]; // e.g. "com.example.FooKt"
                // Stem is the last component: "FooKt"
                let stem = full_import
                    .rsplit('.')
                    .next()
                    .unwrap_or(full_import)
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
                        import_path: Some(full_import.to_string()),
                        qualifier: None,
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Inferred(0.75),
                });
            }

            // SomeClass.Companion.method(
            for cap in JAVA_COMPANION_RE.captures_iter(source) {
                let qualifier = format!("{}Companion", &cap[1]);
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
                        qualifier: Some(qualifier),
                    },
                    kind: EdgeKind::Calls,
                    line,
                    confidence: Confidence::Inferred(0.7),
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
    fn detect_requires_java_and_kotlin() {
        // Only Kotlin — not enough
        let mut file_set = HashSet::new();
        file_set.insert("Foo.kt".to_string());
        let ctx = DetectionContext {
            file_set: &file_set,
            read_file: &|_| None,
        };
        assert!(!JavaKotlinResolver.detect(&ctx));

        // Only Java — not enough
        let mut file_set2 = HashSet::new();
        file_set2.insert("Bar.java".to_string());
        let ctx2 = DetectionContext {
            file_set: &file_set2,
            read_file: &|_| None,
        };
        assert!(!JavaKotlinResolver.detect(&ctx2));

        // Both — detected
        let mut file_set3 = HashSet::new();
        file_set3.insert("Foo.kt".to_string());
        file_set3.insert("Bar.java".to_string());
        let ctx3 = DetectionContext {
            file_set: &file_set3,
            read_file: &|_| None,
        };
        assert!(JavaKotlinResolver.detect(&ctx3));
    }

    #[test]
    fn extract_jvmstatic_edge() {
        let source = r#"
class MyUtils {
    companion object {
        @JvmStatic fun computeSum(a: Int, b: Int): Int = a + b
        @JvmField val MAX_SIZE: Int = 100
        @JvmOverloads fun greet(name: String, greeting: String = "Hello") {}
        @JvmName("getItemCount")
        fun itemCount(): Int = 0
    }
}
"#;
        let symbols = vec![Symbol {
            qualified: QualifiedSymbol {
                file: "MyUtils.kt".to_string(),
                scope_path: vec![],
                name: "MyUtils".to_string(),
            },
            kind: SymbolKind::Class,
            line_start: 2,
            line_end: 10,
            signature: None,
            parent_fqn: None,
        }];
        let edges = JavaKotlinResolver.extract_edges("MyUtils.kt", source, &symbols);
        let names: Vec<&str> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"computeSum"),
            "expected computeSum in {names:?}"
        );
        assert!(
            names.contains(&"MAX_SIZE"),
            "expected MAX_SIZE in {names:?}"
        );
        assert!(names.contains(&"greet"), "expected greet in {names:?}");
        assert!(
            names.contains(&"getItemCount"),
            "expected getItemCount in {names:?}"
        );
    }

    #[test]
    fn extract_java_kotlin_import_edge() {
        let source = r#"
import com.example.StringUtilsKt;
import com.example.MathHelperKt;

public class JavaCaller {
    public void run() {
        StringUtilsKt.capitalize("hello");
        MyClass.Companion.getInstance();
    }
}
"#;
        let symbols = vec![Symbol {
            qualified: QualifiedSymbol {
                file: "JavaCaller.java".to_string(),
                scope_path: vec![],
                name: "JavaCaller".to_string(),
            },
            kind: SymbolKind::Class,
            line_start: 4,
            line_end: 10,
            signature: None,
            parent_fqn: None,
        }];
        let edges = JavaKotlinResolver.extract_edges("JavaCaller.java", source, &symbols);
        let names: Vec<&str> = edges
            .iter()
            .map(|e| match &e.to {
                EdgeTarget::Unresolved { name, .. } => name.as_str(),
                _ => "",
            })
            .collect();
        assert!(
            names.contains(&"StringUtilsKt"),
            "expected StringUtilsKt in {names:?}"
        );
        assert!(
            names.contains(&"MathHelperKt"),
            "expected MathHelperKt in {names:?}"
        );
        assert!(
            names.contains(&"getInstance"),
            "expected getInstance in {names:?}"
        );

        // Companion call should have qualifier
        let companion_edge = edges.iter().find(|e| {
            matches!(&e.to, EdgeTarget::Unresolved { name, qualifier, .. }
                if name == "getInstance" && qualifier.as_deref() == Some("MyClass.Companion"))
        });
        assert!(companion_edge.is_some());
    }
}
