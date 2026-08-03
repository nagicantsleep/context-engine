//! Taint analysis catalog: detects known taint sources and sinks by
//! matching callee text against per-language catalogs.
//!
//! A taint source is a function/method that introduces untrusted data
//! (user input, env vars, network, file reads).
//! A taint sink is a function/method that is dangerous when receiving
//! untrusted data (SQL queries, command execution, file writes, eval).

use crate::parsing::relations::{Confidence, EdgeKind, EdgeTarget, RawEdge};
use crate::parsing::symbols::QualifiedSymbol;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Rust,
    Python,
    Js,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaintKind {
    Source,
    Sink,
}

struct TaintEntry {
    callee: &'static str,
    tag: &'static str, // canonical tag for sentinel name
    kind: TaintKind,
}

static RUST_CATALOG: &[TaintEntry] = &[
    // sources
    TaintEntry {
        callee: "std::env::var",
        tag: "env_var",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "env::var",
        tag: "env_var",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "std::env::args",
        tag: "env_args",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "env::args",
        tag: "env_args",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "std::io::stdin",
        tag: "stdin",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "io::stdin",
        tag: "stdin",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "reqwest::get",
        tag: "http_response",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "hyper::Body::to_bytes",
        tag: "http_body",
        kind: TaintKind::Source,
    },
    // sinks
    TaintEntry {
        callee: "std::process::Command::new",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "process::Command::new",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "tokio::process::Command::new",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "std::fs::write",
        tag: "file_write",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "fs::write",
        tag: "file_write",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "std::fs::remove_file",
        tag: "file_delete",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "fs::remove_file",
        tag: "file_delete",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "sqlx::query",
        tag: "sql_query",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "diesel::sql_query",
        tag: "sql_query",
        kind: TaintKind::Sink,
    },
    // SSRF sinks
    TaintEntry {
        callee: "reqwest::Client::get",
        tag: "ssrf",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "reqwest::Client::post",
        tag: "ssrf",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "hyper::client::Client::request",
        tag: "ssrf",
        kind: TaintKind::Sink,
    },
    // Path traversal sinks
    TaintEntry {
        callee: "std::fs::File::open",
        tag: "file_open",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "fs::File::open",
        tag: "file_open",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "tokio::fs::write",
        tag: "file_write",
        kind: TaintKind::Sink,
    },
    // Deserialization RCE
    TaintEntry {
        callee: "serde_json::from_str",
        tag: "deserialize",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "bincode::deserialize",
        tag: "deserialize",
        kind: TaintKind::Sink,
    },
    // Command injection (additional forms)
    TaintEntry {
        callee: "std::process::Command::arg",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "std::process::Command::args",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "process::Command::arg",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
];

static PYTHON_CATALOG: &[TaintEntry] = &[
    // sources
    TaintEntry {
        callee: "input",
        tag: "user_input",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "os.getenv",
        tag: "env_var",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "os.environ.get",
        tag: "env_var",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "sys.stdin.read",
        tag: "stdin",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "sys.stdin.readline",
        tag: "stdin",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "request.args.get",
        tag: "http_request",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "request.json.get",
        tag: "http_request",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "request.form.get",
        tag: "http_request",
        kind: TaintKind::Source,
    },
    // sinks
    TaintEntry {
        callee: "subprocess.run",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "subprocess.call",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "subprocess.Popen",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "os.system",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "cursor.execute",
        tag: "sql_query",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "db.execute",
        tag: "sql_query",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "eval",
        tag: "eval",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "exec",
        tag: "eval",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "open",
        tag: "file_write",
        kind: TaintKind::Sink,
    },
    // SSRF sinks
    TaintEntry {
        callee: "urllib.request.urlopen",
        tag: "ssrf",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "requests.get",
        tag: "ssrf",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "requests.post",
        tag: "ssrf",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "httpx.get",
        tag: "ssrf",
        kind: TaintKind::Sink,
    },
    // Deserialization RCE
    TaintEntry {
        callee: "pickle.loads",
        tag: "deserialize",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "pickle.load",
        tag: "deserialize",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "yaml.load",
        tag: "deserialize",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "yaml.unsafe_load",
        tag: "deserialize",
        kind: TaintKind::Sink,
    },
    // Template injection
    TaintEntry {
        callee: "jinja2.Template",
        tag: "template_inj",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "django.template.Template",
        tag: "template_inj",
        kind: TaintKind::Sink,
    },
    // Additional command exec
    TaintEntry {
        callee: "os.popen",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "subprocess.check_output",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    // SQL injection (additional forms)
    TaintEntry {
        callee: "sqlite3.Cursor.execute",
        tag: "sql_query",
        kind: TaintKind::Sink,
    },
];

static JS_CATALOG: &[TaintEntry] = &[
    // sources (call-based — member_expression sources handled separately in mod.rs)
    TaintEntry {
        callee: "localStorage.getItem",
        tag: "storage",
        kind: TaintKind::Source,
    },
    TaintEntry {
        callee: "sessionStorage.getItem",
        tag: "storage",
        kind: TaintKind::Source,
    },
    // sinks
    TaintEntry {
        callee: "eval",
        tag: "eval",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "document.write",
        tag: "dom_write",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "child_process.exec",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "child_process.spawn",
        tag: "command_exec",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "fs.writeFile",
        tag: "file_write",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "fs.writeFileSync",
        tag: "file_write",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "fs.unlinkSync",
        tag: "file_delete",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "db.query",
        tag: "sql_query",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "connection.query",
        tag: "sql_query",
        kind: TaintKind::Sink,
    },
    // SSRF sinks
    TaintEntry {
        callee: "fetch",
        tag: "ssrf",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "axios.get",
        tag: "ssrf",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "axios.post",
        tag: "ssrf",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "http.request",
        tag: "ssrf",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "https.request",
        tag: "ssrf",
        kind: TaintKind::Sink,
    },
    // Path traversal
    TaintEntry {
        callee: "fs.readFile",
        tag: "file_read",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "fs.readFileSync",
        tag: "file_read",
        kind: TaintKind::Sink,
    },
    // DOM XSS
    TaintEntry {
        callee: "element.innerHTML",
        tag: "dom_xss",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "element.outerHTML",
        tag: "dom_xss",
        kind: TaintKind::Sink,
    },
    // Code execution
    TaintEntry {
        callee: "Function",
        tag: "eval",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "setTimeout",
        tag: "eval",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "setInterval",
        tag: "eval",
        kind: TaintKind::Sink,
    },
    // Event handler injection
    TaintEntry {
        callee: "element.setAttribute",
        tag: "attr_inject",
        kind: TaintKind::Sink,
    },
    // Open redirect
    TaintEntry {
        callee: "location.href",
        tag: "redirect",
        kind: TaintKind::Sink,
    },
    TaintEntry {
        callee: "window.location.assign",
        tag: "redirect",
        kind: TaintKind::Sink,
    },
];

fn catalog_for(lang: Lang) -> &'static [TaintEntry] {
    match lang {
        Lang::Rust => RUST_CATALOG,
        Lang::Python => PYTHON_CATALOG,
        Lang::Js => JS_CATALOG,
    }
}

/// Check if `call_text` matches a taint source or sink in the catalog for `lang`.
/// Returns `Some(RawEdge)` with `to.name = "taint:source:<tag>"` or `"taint:sink:<tag>"`.
/// Returns `None` if no match.
pub fn emit_taint_edge(
    from_sym: &QualifiedSymbol,
    call_text: &str,
    lang: Lang,
    line: u32,
) -> Option<RawEdge> {
    for entry in catalog_for(lang) {
        if entry.callee == call_text {
            let sentinel = match entry.kind {
                TaintKind::Source => format!("taint:source:{}", entry.tag),
                TaintKind::Sink => format!("taint:sink:{}", entry.tag),
            };
            return Some(RawEdge {
                from: from_sym.clone(),
                to: EdgeTarget::Unresolved {
                    name: sentinel,
                    import_path: None,
                    qualifier: None,
                },
                kind: EdgeKind::DataFlowsTo,
                line,
                confidence: Confidence::Inferred(0.95),
            });
        }
    }
    None
}

/// JS member_expression sources: `req.body`, `req.query`, `req.params`,
/// `process.env`, `document.cookie`, `location.search`.
/// Returns `Some(RawEdge)` if the member text matches a known source.
pub fn emit_js_member_taint_edge(
    from_sym: &QualifiedSymbol,
    member_text: &str,
    line: u32,
) -> Option<RawEdge> {
    let tag = match member_text {
        "req.body" | "req.query" | "req.params" => "http_request",
        "process.env" => "env_var",
        "document.cookie" => "cookie",
        "location.search" | "location.href" => "url_param",
        "window.location.hash" | "window.name" | "document.referrer" => "url_param",
        "req.cookies" => "http_request",
        _ => return None,
    };
    Some(RawEdge {
        from: from_sym.clone(),
        to: EdgeTarget::Unresolved {
            name: format!("taint:source:{}", tag),
            import_path: None,
            qualifier: None,
        },
        kind: EdgeKind::DataFlowsTo,
        line,
        confidence: Confidence::Inferred(0.95),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parsing::symbols::QualifiedSymbol;

    fn test_sym() -> QualifiedSymbol {
        QualifiedSymbol {
            file: "test.rs".into(),
            scope_path: vec![],
            name: "f".into(),
        }
    }

    #[test]
    fn rust_env_var_is_source() {
        let edge = emit_taint_edge(&test_sym(), "std::env::var", Lang::Rust, 1);
        assert!(edge.is_some());
        let e = edge.unwrap();
        match &e.to {
            EdgeTarget::Unresolved { name, .. } => {
                assert_eq!(name, "taint:source:env_var");
            }
            _ => panic!("expected Unresolved"),
        }
    }

    #[test]
    fn rust_command_is_sink() {
        let edge = emit_taint_edge(&test_sym(), "std::process::Command::new", Lang::Rust, 1);
        assert!(edge.is_some());
        match &edge.unwrap().to {
            EdgeTarget::Unresolved { name, .. } => assert!(name.starts_with("taint:sink:")),
            _ => panic!(),
        }
    }

    #[test]
    fn python_eval_is_sink() {
        let edge = emit_taint_edge(&test_sym(), "eval", Lang::Python, 1);
        assert!(edge.is_some());
    }

    #[test]
    fn python_input_is_source() {
        let edge = emit_taint_edge(&test_sym(), "input", Lang::Python, 1);
        assert!(edge.is_some());
        match &edge.unwrap().to {
            EdgeTarget::Unresolved { name, .. } => assert!(name.starts_with("taint:source:")),
            _ => panic!(),
        }
    }

    #[test]
    fn js_eval_is_sink() {
        let edge = emit_taint_edge(&test_sym(), "eval", Lang::Js, 1);
        assert!(edge.is_some());
    }

    #[test]
    fn js_req_body_is_source() {
        let sym = QualifiedSymbol {
            file: "app.js".into(),
            scope_path: vec![],
            name: "handler".into(),
        };
        let edge = emit_js_member_taint_edge(&sym, "req.body", 5);
        assert!(edge.is_some());
        match &edge.unwrap().to {
            EdgeTarget::Unresolved { name, .. } => assert_eq!(name, "taint:source:http_request"),
            _ => panic!(),
        }
    }

    #[test]
    fn no_match_returns_none() {
        let edge = emit_taint_edge(&test_sym(), "println", Lang::Rust, 1);
        assert!(edge.is_none());
    }

    #[test]
    fn rust_ssrf_sink() {
        let edge = emit_taint_edge(&test_sym(), "reqwest::Client::get", Lang::Rust, 1);
        assert!(edge.is_some());
        match &edge.unwrap().to {
            EdgeTarget::Unresolved { name, .. } => assert!(name.contains("ssrf")),
            _ => panic!(),
        }
    }

    #[test]
    fn python_pickle_is_sink() {
        let edge = emit_taint_edge(&test_sym(), "pickle.loads", Lang::Python, 1);
        assert!(edge.is_some());
    }

    #[test]
    fn js_fetch_is_ssrf_sink() {
        let edge = emit_taint_edge(&test_sym(), "fetch", Lang::Js, 1);
        assert!(edge.is_some());
    }
}
