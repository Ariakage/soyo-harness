//! Structural symbol extraction and language detection for local project indexing (`F-17`).

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Module,
    Struct,
    Enum,
    Trait,
    Interface,
    Class,
    Function,
    Method,
    Const,
    Static,
    TypeAlias,
    Variable,
    Heading,
}

impl SymbolKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Struct => "struct",
            Self::Enum => "enum",
            Self::Trait => "trait",
            Self::Interface => "interface",
            Self::Class => "class",
            Self::Function => "function",
            Self::Method => "method",
            Self::Const => "const",
            Self::Static => "static",
            Self::TypeAlias => "type_alias",
            Self::Variable => "variable",
            Self::Heading => "heading",
        }
    }

    pub fn from_lsp_kind(kind: u64) -> Self {
        match kind {
            2..=4 => Self::Module,
            5 => Self::Class,
            6 | 9 => Self::Method,
            10 => Self::Enum,
            11 => Self::Interface,
            12 => Self::Function,
            13 => Self::Variable,
            14 => Self::Const,
            23 => Self::Struct,
            25 | 26 => Self::TypeAlias,
            _ => Self::Function,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedSymbol {
    pub path: PathBuf,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container: Option<String>,
    pub kind: SymbolKind,
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    pub language: String,
    pub file_sha256: String,
}

/// Map a workspace file extension to its canonical LSP `languageId`.
pub fn language_id(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "py" | "pyi" => "python",
        "go" => "go",
        "md" | "markdown" => "markdown",
        "toml" => "toml",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        _ => return None,
    })
}

fn clean_ident(raw: &str) -> Option<String> {
    let trimmed = raw
        .trim_start_matches('&')
        .trim_start_matches("mut ")
        .trim();
    let ident: String = trimmed
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '$')
        .collect();
    if ident.is_empty() || ident.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        return None;
    }
    Some(ident)
}

fn strip_rust_visibility(mut s: &str) -> &str {
    s = s.trim_start();
    if let Some(rest) = s.strip_prefix("pub") {
        let rest = rest.trim_start();
        if let Some(after_paren) = rest.strip_prefix('(') {
            if let Some(end) = after_paren.find(')') {
                s = after_paren[end + 1..].trim_start();
            }
        } else if s.as_bytes().get(3).is_some_and(|b| b.is_ascii_whitespace()) {
            s = rest;
        }
    }
    for kw in [
        "default ",
        "async ",
        "unsafe ",
        "const ",
        "extern \"C\" ",
        "extern ",
    ] {
        if let Some(rest) = s.strip_prefix(kw) {
            if kw == "const " && !rest.trim_start().starts_with("fn ") {
                break;
            }
            s = rest.trim_start();
        }
    }
    s
}

fn extract_rust(path: &Path, text: &str, sha256: &str, max_symbols: usize) -> Vec<IndexedSymbol> {
    let mut out = Vec::new();
    let mut brace_depth = 0usize;
    let mut container_stack: Vec<(usize, String)> = Vec::new();

    for (idx, line) in text.lines().enumerate() {
        let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        let trimmed = line.trim();
        let opens = trimmed.chars().filter(|&c| c == '{').count();
        let closes = trimmed.chars().filter(|&c| c == '}').count();

        if !trimmed.starts_with("//") && !trimmed.starts_with("/*") && !trimmed.starts_with('*') {
            let normalized = strip_rust_visibility(trimmed);
            let mut matched: Option<(SymbolKind, String)> = None;
            let mut pushes_container = false;

            if let Some(rest) = normalized.strip_prefix("impl") {
                if rest.starts_with('<') || rest.starts_with(char::is_whitespace) {
                    let after_gen = skip_generics(rest.trim_start());
                    let target = after_gen
                        .split(" for ")
                        .last()
                        .unwrap_or(after_gen)
                        .trim_start();
                    if let Some(name) = clean_ident(target) {
                        container_stack.push((brace_depth + opens.saturating_sub(closes), name));
                        pushes_container = true;
                    }
                }
            } else if let Some(rest) = normalized.strip_prefix("fn ") {
                if let Some(name) = clean_ident(rest) {
                    let kind = if container_stack.is_empty() {
                        SymbolKind::Function
                    } else {
                        SymbolKind::Method
                    };
                    matched = Some((kind, name));
                }
            } else if let Some(rest) = normalized.strip_prefix("struct ") {
                matched = clean_ident(rest).map(|n| (SymbolKind::Struct, n));
            } else if let Some(rest) = normalized.strip_prefix("enum ") {
                matched = clean_ident(rest).map(|n| (SymbolKind::Enum, n));
            } else if let Some(rest) = normalized.strip_prefix("trait ") {
                if let Some(name) = clean_ident(rest) {
                    if opens > closes {
                        container_stack.push((brace_depth + opens - closes, name.clone()));
                        pushes_container = true;
                    }
                    matched = Some((SymbolKind::Trait, name));
                }
            } else if let Some(rest) = normalized.strip_prefix("mod ") {
                if let Some(name) = clean_ident(rest) {
                    if opens > closes {
                        container_stack.push((brace_depth + opens - closes, name.clone()));
                        pushes_container = true;
                    }
                    matched = Some((SymbolKind::Module, name));
                }
            } else if let Some(rest) = normalized.strip_prefix("type ") {
                matched = clean_ident(rest).map(|n| (SymbolKind::TypeAlias, n));
            } else if let Some(rest) = normalized.strip_prefix("const ") {
                matched = clean_ident(rest).map(|n| (SymbolKind::Const, n));
            } else if let Some(rest) = normalized.strip_prefix("static ") {
                let rest = rest
                    .trim_start()
                    .strip_prefix("mut ")
                    .unwrap_or(rest)
                    .trim_start();
                matched = clean_ident(rest).map(|n| (SymbolKind::Static, n));
            }

            if let Some((kind, name)) = matched {
                let container = if pushes_container {
                    None
                } else {
                    container_stack.last().map(|(_, c)| c.clone())
                };
                out.push(IndexedSymbol {
                    path: path.to_path_buf(),
                    name,
                    container,
                    kind,
                    start_line: line_no,
                    start_col: u32::try_from(line.find(trimmed).unwrap_or(0) + 1).unwrap_or(1),
                    end_line: line_no,
                    end_col: u32::try_from(line.len().max(1)).unwrap_or(1),
                    signature: Some(trimmed.chars().take(160).collect()),
                    language: "rust".into(),
                    file_sha256: sha256.to_owned(),
                });
                if out.len() >= max_symbols {
                    break;
                }
            }
        }

        brace_depth = brace_depth.saturating_add(opens).saturating_sub(closes);
        while container_stack
            .last()
            .is_some_and(|(depth, _)| brace_depth < *depth)
        {
            container_stack.pop();
        }
    }
    out
}

fn skip_generics(mut s: &str) -> &str {
    if !s.starts_with('<') {
        return s;
    }
    let mut depth = 0usize;
    for (idx, ch) in s.char_indices() {
        match ch {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    s = &s[idx + 1..];
                    return s.trim_start();
                }
            }
            _ => {}
        }
    }
    s
}

fn extract_ts_js(
    path: &Path,
    language: &str,
    text: &str,
    sha256: &str,
    max_symbols: usize,
) -> Vec<IndexedSymbol> {
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        let mut s = line.trim();
        if s.starts_with("//") || s.starts_with("/*") || s.starts_with('*') {
            continue;
        }
        for kw in [
            "export default ",
            "export ",
            "declare ",
            "async ",
            "abstract ",
        ] {
            if let Some(rest) = s.strip_prefix(kw) {
                s = rest.trim_start();
            }
        }
        let matched = if let Some(rest) = s.strip_prefix("function ") {
            clean_ident(rest.trim_start_matches('*')).map(|n| (SymbolKind::Function, n))
        } else if let Some(rest) = s.strip_prefix("class ") {
            clean_ident(rest).map(|n| (SymbolKind::Class, n))
        } else if let Some(rest) = s.strip_prefix("interface ") {
            clean_ident(rest).map(|n| (SymbolKind::Interface, n))
        } else if let Some(rest) = s.strip_prefix("type ") {
            clean_ident(rest).map(|n| (SymbolKind::TypeAlias, n))
        } else if let Some(rest) = s.strip_prefix("enum ") {
            clean_ident(rest).map(|n| (SymbolKind::Enum, n))
        } else if let Some(rest) = s.strip_prefix("const ") {
            clean_ident(rest).map(|n| (SymbolKind::Const, n))
        } else {
            None
        };
        if let Some((kind, name)) = matched {
            out.push(IndexedSymbol {
                path: path.to_path_buf(),
                name,
                container: None,
                kind,
                start_line: line_no,
                start_col: u32::try_from(line.find(line.trim()).unwrap_or(0) + 1).unwrap_or(1),
                end_line: line_no,
                end_col: u32::try_from(line.len().max(1)).unwrap_or(1),
                signature: Some(line.trim().chars().take(160).collect()),
                language: language.to_owned(),
                file_sha256: sha256.to_owned(),
            });
            if out.len() >= max_symbols {
                break;
            }
        }
    }
    out
}

fn extract_python(path: &Path, text: &str, sha256: &str, max_symbols: usize) -> Vec<IndexedSymbol> {
    let mut out = Vec::new();
    let mut class_indent: Option<(usize, String)> = None;
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len().saturating_sub(line.trim_start().len());
        if let Some((c_indent, _)) = &class_indent
            && indent <= *c_indent
        {
            class_indent = None;
        }
        let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        if let Some(rest) = trimmed.strip_prefix("class ")
            && let Some(name) = clean_ident(rest)
        {
            class_indent = Some((indent, name.clone()));
            out.push(IndexedSymbol {
                path: path.to_path_buf(),
                name,
                container: None,
                kind: SymbolKind::Class,
                start_line: line_no,
                start_col: u32::try_from(indent + 1).unwrap_or(1),
                end_line: line_no,
                end_col: u32::try_from(line.len().max(1)).unwrap_or(1),
                signature: Some(trimmed.chars().take(160).collect()),
                language: "python".into(),
                file_sha256: sha256.to_owned(),
            });
        } else if let Some(rest) = trimmed
            .strip_prefix("async def ")
            .or_else(|| trimmed.strip_prefix("def "))
            && let Some(name) = clean_ident(rest)
        {
            let container = class_indent.as_ref().map(|(_, c)| c.clone());
            out.push(IndexedSymbol {
                path: path.to_path_buf(),
                name,
                container: container.clone(),
                kind: if container.is_some() {
                    SymbolKind::Method
                } else {
                    SymbolKind::Function
                },
                start_line: line_no,
                start_col: u32::try_from(indent + 1).unwrap_or(1),
                end_line: line_no,
                end_col: u32::try_from(line.len().max(1)).unwrap_or(1),
                signature: Some(trimmed.chars().take(160).collect()),
                language: "python".into(),
                file_sha256: sha256.to_owned(),
            });
        }
        if out.len() >= max_symbols {
            break;
        }
    }
    out
}

fn extract_go(path: &Path, text: &str, sha256: &str, max_symbols: usize) -> Vec<IndexedSymbol> {
    let mut out = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("//") {
            continue;
        }
        let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        let matched = if let Some(rest) = trimmed.strip_prefix("func ") {
            if let Some(after_recv) = rest.strip_prefix('(')
                && let Some(close) = after_recv.find(')')
            {
                let meth = after_recv[close + 1..].trim_start();
                let recv_ident = after_recv[..close]
                    .split_whitespace()
                    .last()
                    .and_then(|s| clean_ident(s.trim_start_matches('*')));
                clean_ident(meth).map(|n| (SymbolKind::Method, n, recv_ident))
            } else {
                clean_ident(rest).map(|n| (SymbolKind::Function, n, None))
            }
        } else if let Some(rest) = trimmed.strip_prefix("type ") {
            let kind = if trimmed.contains("interface") {
                SymbolKind::Interface
            } else if trimmed.contains("struct") {
                SymbolKind::Struct
            } else {
                SymbolKind::TypeAlias
            };
            clean_ident(rest).map(|n| (kind, n, None))
        } else {
            None
        };
        if let Some((kind, name, container)) = matched {
            out.push(IndexedSymbol {
                path: path.to_path_buf(),
                name,
                container,
                kind,
                start_line: line_no,
                start_col: 1,
                end_line: line_no,
                end_col: u32::try_from(line.len().max(1)).unwrap_or(1),
                signature: Some(trimmed.chars().take(160).collect()),
                language: "go".into(),
                file_sha256: sha256.to_owned(),
            });
            if out.len() >= max_symbols {
                break;
            }
        }
    }
    out
}

fn extract_markdown_toml(
    path: &Path,
    language: &str,
    text: &str,
    sha256: &str,
    max_symbols: usize,
) -> Vec<IndexedSymbol> {
    let mut out = Vec::new();
    let mut in_fence = false;
    for (idx, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        let line_no = u32::try_from(idx + 1).unwrap_or(u32::MAX);
        if language == "markdown" {
            if trimmed.starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if !in_fence && trimmed.starts_with('#') {
                let heading = trimmed.trim_start_matches('#').trim();
                if !heading.is_empty() {
                    out.push(IndexedSymbol {
                        path: path.to_path_buf(),
                        name: heading.chars().take(120).collect(),
                        container: None,
                        kind: SymbolKind::Heading,
                        start_line: line_no,
                        start_col: 1,
                        end_line: line_no,
                        end_col: u32::try_from(line.len().max(1)).unwrap_or(1),
                        signature: Some(trimmed.chars().take(160).collect()),
                        language: "markdown".into(),
                        file_sha256: sha256.to_owned(),
                    });
                }
            }
        } else if language == "toml"
            && let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']'))
        {
            let name = inner.trim_matches(['[', ']', ' ']);
            if !name.is_empty() {
                out.push(IndexedSymbol {
                    path: path.to_path_buf(),
                    name: name.to_owned(),
                    container: None,
                    kind: SymbolKind::Module,
                    start_line: line_no,
                    start_col: 1,
                    end_line: line_no,
                    end_col: u32::try_from(line.len().max(1)).unwrap_or(1),
                    signature: Some(trimmed.chars().take(160).collect()),
                    language: "toml".into(),
                    file_sha256: sha256.to_owned(),
                });
            }
        }
        if out.len() >= max_symbols {
            break;
        }
    }
    out
}

/// Extract structural symbols for one text file up to `max_symbols`.
pub fn extract_symbols(
    rel_path: &Path,
    language: &str,
    text: &str,
    sha256: &str,
    max_symbols: usize,
) -> Vec<IndexedSymbol> {
    match language {
        "rust" => extract_rust(rel_path, text, sha256, max_symbols),
        "typescript" | "typescriptreact" | "javascript" | "javascriptreact" => {
            extract_ts_js(rel_path, language, text, sha256, max_symbols)
        }
        "python" => extract_python(rel_path, text, sha256, max_symbols),
        "go" => extract_go(rel_path, text, sha256, max_symbols),
        "markdown" | "toml" => extract_markdown_toml(rel_path, language, text, sha256, max_symbols),
        _ => Vec::new(),
    }
}
