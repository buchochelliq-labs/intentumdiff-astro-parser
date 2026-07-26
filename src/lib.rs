//! Astro component parser plugin — full-parse mode.
//!
//! Handles `.astro` single-file component files.
//!
//! Astro components have an optional *frontmatter* fence (`---`), which
//! contains TypeScript/JavaScript imports and server-side logic, followed by
//! an HTML/JSX-like template body.
//!
//! Semantic nodes produced:
//!   astro_component    — root; label = filename stem
//!   frontmatter_block  — code between opening and closing `---` fences
//!   template_body      — everything after the closing `---` fence

use intentdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}
struct AstroParser;

// ---------------------------------------------------------------------------
// Content hash (djb2)
// ---------------------------------------------------------------------------

fn content_hash(s: &str) -> String {
    let mut h: u64 = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    format!("{:016x}", h)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

struct ParsedBlock {
    start_line: u32,
    end_line: u32,
    content_start_line: u32,
    content: String,
    hash: String,
}

struct ParsedAstro {
    frontmatter: Option<ParsedBlock>,
    template: Option<ParsedBlock>,
}

fn parse_astro(source: &str) -> ParsedAstro {
    let lines: Vec<&str> = source.lines().collect();
    let total = lines.len();

    // The frontmatter fence must start on line 0 (possibly after BOM or empty line).
    let first_nonempty = lines
        .iter()
        .enumerate()
        .find(|(_, l)| !l.trim().is_empty())
        .map(|(i, _)| i);

    let (fence_open, after_open) = if let Some(idx) = first_nonempty {
        if lines[idx].trim() == "---" {
            (Some(idx), idx + 1)
        } else {
            (None, 0)
        }
    } else {
        (None, 0)
    };

    if let Some(fence_open_line) = fence_open {
        // Find the closing `---`
        let fence_close = lines[after_open..]
            .iter()
            .enumerate()
            .find(|(_, l)| l.trim() == "---")
            .map(|(i, _)| after_open + i);

        if let Some(fence_close_line) = fence_close {
            let fm_content: String = lines[after_open..fence_close_line].join("\n");
            let fm_hash = content_hash(&fm_content);
            let template_start = fence_close_line + 1;
            let template_content: String = lines[template_start..].join("\n");
            let tpl_hash = content_hash(&template_content);
            return ParsedAstro {
                frontmatter: Some(ParsedBlock {
                    start_line: fence_open_line as u32,
                    end_line: fence_close_line as u32,
                    content_start_line: after_open as u32,
                    content: fm_content,
                    hash: fm_hash,
                }),
                template: if template_start < total {
                    Some(ParsedBlock {
                        start_line: template_start as u32,
                        end_line: (total - 1) as u32,
                        content_start_line: template_start as u32,
                        content: template_content,
                        hash: tpl_hash,
                    })
                } else {
                    None
                },
            };
        } else {
            // Unclosed frontmatter — treat entire file as frontmatter
            let fm_content = source.to_string();
            let fm_hash = content_hash(&fm_content);
            return ParsedAstro {
                frontmatter: Some(ParsedBlock {
                    start_line: fence_open_line as u32,
                    end_line: (total.saturating_sub(1)) as u32,
                    content_start_line: fence_open_line as u32,
                    content: fm_content,
                    hash: fm_hash,
                }),
                template: None,
            };
        }
    }

    // No frontmatter — entire file is template
    let tpl_hash = content_hash(source);
    ParsedAstro {
        frontmatter: None,
        template: if total > 0 {
            Some(ParsedBlock {
                start_line: 0,
                end_line: (total - 1) as u32,
                content_start_line: 0,
                content: source.to_string(),
                hash: tpl_hash,
            })
        } else {
            None
        },
    }
}

fn make_leaf(id: &str, node_type: &str, label: impl Into<String>, line: u32) -> SemanticNode {
    SemanticNodeBuilder::new(id, node_type, label.into(), line, 0, line, 0, "").build()
}

fn is_identifier_like(value: &str) -> bool {
    let mut chars = value.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$' || c == '-')
}

fn normalize_statement(line: &str) -> String {
    line.trim()
        .trim_end_matches(',')
        .trim_end_matches(';')
        .trim()
        .to_string()
}

fn declaration_name(line: &str) -> Option<&str> {
    for keyword in ["const ", "let ", "var "] {
        if let Some(rest) = line.strip_prefix(keyword) {
            let name = rest
                .split(|c: char| c.is_whitespace() || c == '=' || c == ':' || c == ';')
                .next()
                .unwrap_or("");
            if is_identifier_like(name) {
                return Some(name);
            }
        }
    }
    None
}

fn extract_frontmatter_children(
    content: &str,
    start_line: u32,
    id_prefix: &str,
) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, raw_line) in content.lines().enumerate() {
        let line = normalize_statement(raw_line);
        if line.is_empty() {
            continue;
        }
        let absolute_line = start_line + line_idx as u32;
        let child_id = format!("{}.{}", id_prefix, children.len());
        let node = if line.starts_with("import ") {
            Some(make_leaf(
                &child_id,
                "import_statement",
                line,
                absolute_line,
            ))
        } else {
            declaration_name(&line).map(|name| {
                let mut decl = make_leaf(
                    &child_id,
                    "variable_declaration",
                    name.to_string(),
                    absolute_line,
                );
                // #46: the declaration's RHS is review content — carry it as a child so a
                // frontmatter value edit (const title = "My Site" -> "...X") surfaces
                // instead of hashing name-only.
                if let Some((_, rhs)) = line.split_once('=') {
                    let rhs = rhs.trim().trim_end_matches(';').trim();
                    if !rhs.is_empty() {
                        decl.children = vec![make_leaf(
                            &format!("{child_id}.0"),
                            "declaration_value",
                            rhs.to_string(),
                            absolute_line,
                        )];
                    }
                }
                decl
            })
        };
        if let Some(node) = node {
            children.push(node);
        }
    }
    children
}

fn parse_attr_labels(attrs: &str) -> Vec<String> {
    attrs
        .split_whitespace()
        .filter_map(|part| {
            let clean = part
                .trim()
                .trim_end_matches('>')
                .trim_end_matches('/')
                .trim();
            if clean.is_empty() {
                return None;
            }
            let (name, value) = clean.split_once('=').unwrap_or((clean, ""));
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            if value.is_empty() {
                Some(name.to_string())
            } else {
                let value = value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .trim_matches('{')
                    .trim_matches('}');
                Some(format!("{}={}", name, value))
            }
        })
        .collect()
}

fn parse_tag_segment(segment: &str) -> Option<(String, String)> {
    let trimmed = segment.trim();
    if trimmed.is_empty() || trimmed.starts_with('/') || trimmed.starts_with('!') {
        return None;
    }
    let tag_end = trimmed
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(trimmed.len());
    let tag = trimmed[..tag_end].trim();
    if tag.is_empty() {
        return None;
    }
    let attrs = trimmed[tag_end..]
        .trim()
        .trim_end_matches('/')
        .trim()
        .to_string();
    Some((tag.to_lowercase(), attrs))
}

fn extract_brace_labels(line: &str) -> Vec<String> {
    let mut labels = Vec::new();
    let mut rest = line;
    while let Some(start) = rest.find('{') {
        if rest[start..].starts_with("{{") {
            rest = &rest[start + 2..];
            continue;
        }
        let after = &rest[start + 1..];
        if let Some(end) = after.find('}') {
            let label = after[..end].trim();
            if !label.is_empty() && !label.starts_with('#') && !label.starts_with('/') {
                labels.push(label.to_string());
            }
            rest = &after[end + 1..];
        } else {
            break;
        }
    }
    labels
}

fn extract_template_children(content: &str, start_line: u32, id_prefix: &str) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        let absolute_line = start_line + line_idx as u32;
        let mut rest = line;
        while let Some(open_idx) = rest.find('<') {
            let after_open = &rest[open_idx + 1..];
            if let Some(close_idx) = after_open.find('>') {
                let segment = &after_open[..close_idx];
                if let Some((tag, attrs)) = parse_tag_segment(segment) {
                    let child_id = format!("{}.{}", id_prefix, children.len());
                    let attr_children: Vec<SemanticNode> = parse_attr_labels(&attrs)
                        .into_iter()
                        .enumerate()
                        .map(|(i, label)| {
                            make_leaf(
                                &format!("{}.{}", child_id, i),
                                "attribute",
                                label,
                                absolute_line,
                            )
                        })
                        .collect();
                    children.push(
                        SemanticNodeBuilder::new(
                            &child_id,
                            "element",
                            tag,
                            absolute_line,
                            0,
                            absolute_line,
                            0,
                            "",
                        )
                        .children(attr_children)
                        .build(),
                    );
                }
                rest = &after_open[close_idx + 1..];
            } else {
                break;
            }
        }
        for label in extract_brace_labels(line) {
            let child_id = format!("{}.{}", id_prefix, children.len());
            children.push(make_leaf(&child_id, "interpolation", label, absolute_line));
        }
    }
    children
}

fn process_impl(source: &str, filename: &str) -> String {
    let stem = filename
        .rsplit(['/', '\\'])
        .next()
        .and_then(|f| f.rsplit('.').nth(1))
        .unwrap_or("component");

    let parsed = parse_astro(source);
    let end_line = source.lines().count().saturating_sub(1) as u32;

    let mut children: Vec<SemanticNode> = Vec::new();
    let mut hashes: Vec<String> = Vec::new();

    if let Some(block) = &parsed.frontmatter {
        hashes.push(block.hash.clone());
        let id = format!("0.{}", children.len());
        let block_children =
            extract_frontmatter_children(&block.content, block.content_start_line, &id);
        children.push(
            SemanticNodeBuilder::new(
                &id,
                "frontmatter_block",
                "frontmatter",
                block.start_line,
                0,
                block.end_line,
                0,
                block.hash.clone(),
            )
            .children(block_children)
            .build(),
        );
    }

    if let Some(block) = &parsed.template {
        hashes.push(block.hash.clone());
        let id = format!("0.{}", children.len());
        let block_children =
            extract_template_children(&block.content, block.content_start_line, &id);
        children.push(
            SemanticNodeBuilder::new(
                &id,
                "template_body",
                "template",
                block.start_line,
                0,
                block.end_line,
                0,
                block.hash.clone(),
            )
            .children(block_children)
            .build(),
        );
    }

    let root_hash = content_hash(&hashes.join("|"));

    let root = SemanticNodeBuilder::new(
        "0",
        "astro_component",
        stem.to_string(),
        0,
        0,
        end_line,
        0,
        root_hash,
    )
    .children(children)
    .build();

    match serde_json::to_string(&root) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for AstroParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }
    fn grammar_id() -> String {
        "astro".to_string()
    }
    fn detect_language(filename: String, _content: String) -> String {
        if filename.to_lowercase().ends_with(".astro") {
            return "astro".to_string();
        }
        String::new()
    }
    fn preprocess_source(source: String) -> String {
        source
    }
    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "---\nconst title = \"My Site\";\n---\n<html>\n  <head><title>{title}</title></head>\n  <body>\n    <h1>Hello World</h1>\n    <p>Welcome to my site.</p>\n  </body>\n</html>\n".to_string(),
            new: "---\nconst title = \"My Site\";\nconst year = new Date().getFullYear();\n---\n<html lang=\"en\">\n  <head>\n    <meta charset=\"UTF-8\" />\n    <title>{title}</title>\n  </head>\n  <body>\n    <header><h1>Hello World</h1></header>\n    <main><p>Welcome to my site. &copy; {year}</p></main>\n  </body>\n</html>\n".to_string(),
        }
    }
    fn process(input: String, _language: String, filename: String) -> String {
        process_impl(&input, &filename)
    }
    fn trivia_node_types() -> Vec<String> {
        vec![]
    }
    fn language_ids() -> Vec<String> {
        vec!["astro".to_string()]
    }
    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }
    fn priority() -> i32 {
        0
    }
}

export!(AstroParser);

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!AstroParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        assert!(AstroParser::language_ids().contains(&AstroParser::grammar_id()));
    }

    #[test]
    fn detect_language_astro() {
        assert_eq!(
            AstroParser::detect_language("Page.astro".to_string(), "".to_string()),
            "astro"
        );
    }

    #[test]
    fn detect_language_unknown() {
        assert_eq!(
            AstroParser::detect_language("main.ts".to_string(), "".to_string()),
            ""
        );
    }

    #[test]
    fn empty_source_valid_json() {
        let out = process_impl("", "Page.astro");
        serde_json::from_str::<serde_json::Value>(&out).expect("valid JSON");
    }

    #[test]
    fn frontmatter_and_template_extracted() {
        let src = "---\nimport Component from './Component.astro';\nconst title = 'Hello';\n---\n<h1>{title}</h1>";
        let parsed = parse_astro(src);
        assert!(parsed.frontmatter.is_some(), "should have frontmatter");
        assert!(parsed.template.is_some(), "should have template");
    }

    #[test]
    fn no_frontmatter() {
        let src = "<h1>Hello</h1>";
        let parsed = parse_astro(src);
        assert!(parsed.frontmatter.is_none(), "no frontmatter expected");
        assert!(parsed.template.is_some(), "template expected");
    }

    #[test]
    fn process_returns_valid_json_with_frontmatter() {
        let src = "---\nconst x = 1;\n---\n<p>x is {x}</p>";
        let out = process_impl(src, "Page.astro");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let children = v["children"].as_array().expect("children array");
        assert!(children
            .iter()
            .any(|c| c["node_type"] == "frontmatter_block"));
        assert!(children.iter().any(|c| c["node_type"] == "template_body"));
        assert!(children.iter().any(|c| c["children"]
            .as_array()
            .is_some_and(|nested| nested.iter().any(|n| n["label"] == "x"))));
    }

    #[test]
    fn example_extracts_component_children() {
        let example = AstroParser::example("astro".to_string());
        let out = process_impl(&example.new, "Page.astro");
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        let labels = collect_labels(&v);
        assert!(labels.iter().any(|label| label == "year"));
        assert!(labels.iter().any(|label| label == "meta"));
        assert!(labels.iter().any(|label| label == "header"));
        assert!(labels.iter().any(|label| label == "main"));
    }

    fn collect_labels(value: &serde_json::Value) -> Vec<String> {
        let mut labels = Vec::new();
        if let Some(label) = value["label"].as_str() {
            labels.push(label.to_string());
        }
        if let Some(children) = value["children"].as_array() {
            for child in children {
                labels.extend(collect_labels(child));
            }
        }
        labels
    }
}
