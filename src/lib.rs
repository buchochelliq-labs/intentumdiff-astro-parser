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

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

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
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
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

fn make_leaf(
    id: &str, node_type: &str, label: impl Into<String>, line: u32,
    start_col: u32, end_col: u32,
) -> SemanticNode {
    SemanticNodeBuilder::new(
        id, node_type, label.into(), line, start_col, line, end_col, "",
    ).build()
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
        // Semantic labels may be trimmed, but the span stays in ORIGINAL
        // source byte columns, including any indentation.
        let indentation = raw_line.len() - raw_line.trim_start().len();
        let child_id = format!("{}.{}", id_prefix, children.len());
        let node = if line.starts_with("import ") {
            Some(make_leaf(
                &child_id, "import_statement", line.clone(), absolute_line,
                indentation as u32, (indentation + line.len()) as u32,
            ))
        } else {
            declaration_name(&line).map(|name| {
                let keyword_bytes = if line.starts_with("const ") {
                    "const ".len()
                } else if line.starts_with("let ") {
                    "let ".len()
                } else {
                    "var ".len()
                };
                let name_start = indentation + keyword_bytes;
                let mut decl = make_leaf(
                    &child_id, "variable_declaration", name.to_string(), absolute_line,
                    name_start as u32, (name_start + name.len()) as u32,
                );
                if let Some((lhs, rhs_raw)) = line.split_once('=') {
                    let rhs = rhs_raw.trim().trim_end_matches(';').trim();
                    if !rhs.is_empty() {
                        let rhs_start = indentation + lhs.len() + 1
                            + (rhs_raw.len() - rhs_raw.trim_start().len());
                        decl.children = vec![make_leaf(
                            &format!("{child_id}.0"), "declaration_value", rhs.to_string(),
                            absolute_line, rhs_start as u32,
                            (rhs_start + rhs.len()) as u32,
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

// Scan original UTF-8 source, not whitespace-separated fragments: Astro/JSX
// values can contain spaces, quotes and comparisons without creating attributes.
fn attribute_token_ranges(attrs: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = None;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut braces = 0_usize;

    for (idx, ch) in attrs.char_indices() {
        if ch.is_whitespace() && quote.is_none() && braces == 0 {
            if let Some(begin) = start.take() {
                ranges.push((begin, idx));
            }
            continue;
        }
        if start.is_none() {
            start = Some(idx);
        }
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '{' => braces += 1,
            '}' if braces > 0 => braces -= 1,
            _ => {}
        }
    }
    if let Some(begin) = start {
        ranges.push((begin, attrs.len()));
    }
    ranges
}

fn parse_attr_labels(attrs: &str) -> Vec<(String, usize, usize)> {
    attribute_token_ranges(attrs)
        .into_iter()
        .filter_map(|(start, end)| {
            let part = &attrs[start..end];
            let clean = part.trim().trim_end_matches('>').trim_end_matches('/').trim();
            if clean.is_empty() {
                return None;
            }
            let (name, value) = clean.split_once('=').unwrap_or((clean, ""));
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            let label = if value.is_empty() {
                name.to_string()
            } else {
                let value = value.trim().trim_matches('"').trim_matches('\'')
                    .trim_matches('{').trim_matches('}');
                format!("{}={}", name, value)
            };
            Some((label, start, end))
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

fn extract_brace_labels(line: &str) -> Vec<(String, usize, usize)> {
    let mut labels = Vec::new();
    let mut cursor = 0;
    while let Some(relative_start) = line[cursor..].find('{') {
        let opening = cursor + relative_start;
        if line[opening..].starts_with("{{") {
            cursor = opening + 2;
            continue;
        }
        let after = &line[opening + 1..];
        if let Some(end) = after.find('}') {
            let raw_label = &after[..end];
            let label = raw_label.trim();
            if !label.is_empty() && !label.starts_with('#') && !label.starts_with('/') {
                let start = opening + 1 + raw_label.len() - raw_label.trim_start().len();
                labels.push((label.to_string(), start, start + label.len()));
            }
            cursor = opening + 1 + end + 1;
        } else {
            break;
        }
    }
    labels
}


 // '>' ends a tag only outside quoted/JSX attribute values. All offsets are
 // byte offsets from char_indices so multibyte UTF-8 prefixes stay anchored.
fn find_tag_close(after_open: &str) -> Option<usize> {
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut braces = 0_usize;
    for (idx, ch) in after_open.char_indices() {
        if let Some(active_quote) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '{' => braces += 1,
            '}' if braces > 0 => braces -= 1,
            '>' if braces == 0 => return Some(idx),
            _ => {}
        }
    }
    None
}

fn extract_template_children(content: &str, start_line: u32, id_prefix: &str) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    for (line_idx, line) in content.lines().enumerate() {
        let absolute_line = start_line + line_idx as u32;
        let mut rest = line;
        while let Some(open_idx) = rest.find('<') {
            let after_open = &rest[open_idx + 1..];
            if let Some(close_idx) = find_tag_close(after_open) {
                let segment = &after_open[..close_idx];
                if let Some((tag, attrs)) = parse_tag_segment(segment) {
                    // Account for already-consumed tags and UTF-8 prefixes.
                    let segment_offset = line.len() - rest.len() + open_idx + 1;
                    // parse_tag_segment trims leading whitespace; retain its
                    // original UTF-8 byte offset before locating attributes.
                    let segment_prefix = segment.len() - segment.trim_start().len();
                    let trimmed_segment = &segment[segment_prefix..];
                    let tag_end = trimmed_segment
                        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
                        .unwrap_or(trimmed_segment.len());
                    let raw_attrs = &trimmed_segment[tag_end..];
                    let attrs_offset = segment_prefix + tag_end
                        + raw_attrs.len() - raw_attrs.trim_start().len();
                    let child_id = format!("{}.{}", id_prefix, children.len());
                    let attr_children: Vec<SemanticNode> = parse_attr_labels(&attrs)
                        .into_iter()
                        .enumerate()
                        .map(|(i, (label, start, end))| {
                            make_leaf(
                                &format!("{}.{}", child_id, i), "attribute", label,
                                absolute_line,
                                (segment_offset + attrs_offset + start) as u32,
                                (segment_offset + attrs_offset + end) as u32,
                            )
                        })
                        .collect();
                    children.push(
                        SemanticNodeBuilder::new(
                            &child_id, "element", tag,
                            absolute_line, (segment_offset - 1) as u32,
                            absolute_line, (segment_offset + close_idx + 1) as u32,
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
        for (label, start, end) in extract_brace_labels(line) {
            let child_id = format!("{}.{}", id_prefix, children.len());
            children.push(make_leaf(
                &child_id, "interpolation", label, absolute_line,
                start as u32, end as u32,
            ));
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

    #[test]
    fn source_spans_anchor_charset_edits_and_frontmatter_values() {
        let old = "---\nconst title = \"My Site\";\n---\n<html><meta charset=\"UTF-8\" /></html>\n";
        let changed_charset = old.replace("UTF-8", "UTF-9");
        let changed_title = old.replace("My Site", "My SiteX");
        for src in [old, changed_charset.as_str(), changed_title.as_str()] {
            let parsed: serde_json::Value =
                serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
            let attribute_label = if src.contains("UTF-9") {
                "charset=UTF-9"
            } else {
                "charset=UTF-8"
            };
            let attr = find_label(&parsed, attribute_label).expect("attribute visible");
            let line = src.lines().nth(3).unwrap();
            let raw_attr = if src.contains("UTF-9") {
                "charset=\"UTF-9\""
            } else {
                "charset=\"UTF-8\""
            };
            let attr_start = line.find(raw_attr).unwrap() as u64;
            assert_eq!(attr["position"], serde_json::json!({
                "start_line": 3, "start_col": attr_start,
                "end_line": 3, "end_col": attr_start + raw_attr.len() as u64
            }));
            let rhs = if src.contains("My SiteX") {
                "\"My SiteX\""
            } else {
                "\"My Site\""
            };
            let value = find_label(&parsed, rhs).expect("frontmatter RHS visible");
            let rhs_start = src.lines().nth(1).unwrap().find(rhs).unwrap() as u64;
            assert_eq!(value["position"], serde_json::json!({
                "start_line": 1, "start_col": rhs_start,
                "end_line": 1, "end_col": rhs_start + rhs.len() as u64
            }));
        }
    }

    #[test]
    fn attribute_spans_account_for_inline_tags_and_utf8_prefixes() {
        let src = "<p title=\"café\"></p><meta charset=\"UTF-8\" />";
        let tree: serde_json::Value =
            serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
        for (label, raw) in [
            ("title=café", "title=\"café\""),
            ("charset=UTF-8", "charset=\"UTF-8\""),
        ] {
            let child = find_label(&tree, label).expect("semantic attribute missing");
            let start = src.find(raw).unwrap() as u64;
            assert_eq!(child["position"], serde_json::json!({
                "start_line": 0, "start_col": start,
                "end_line": 0, "end_col": start + raw.len() as u64
            }));
        }
    }

    #[test]
    fn attribute_spans_preserve_whitespace_before_tag_names() {
        let src = "<  meta charset=\"UTF-8\" />";
        let tree: serde_json::Value =
            serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
        let child = find_label(&tree, "charset=UTF-8")
            .expect("attribute visible after leading tag whitespace");
        let raw = "charset=\"UTF-8\"";
        let start = src.find(raw).unwrap() as u64;
        assert_eq!(child["position"], serde_json::json!({
            "start_line": 0, "start_col": start,
            "end_line": 0, "end_col": start + raw.len() as u64
        }));
    }


    #[test]
    fn attribute_spans_preserve_quoted_whitespace_gt_and_unicode() {
        let src = "<a title=\"first > second\" data-note='café au lait'></a><a title=\"after\"/>";
        let tree: serde_json::Value =
            serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
        for (label, raw) in [
            ("title=first > second", "title=\"first > second\""),
            ("data-note=café au lait", "data-note='café au lait'"),
            ("title=after", "title=\"after\""),
        ] {
            let child = find_label(&tree, label)
                .unwrap_or_else(|| panic!("missing original attribute: {label}"));
            let start = src.find(raw).unwrap() as u64;
            assert_eq!(child["position"], serde_json::json!({
                "start_line": 0, "start_col": start,
                "end_line": 0, "end_col": start + raw.len() as u64
            }));
        }
        assert!(find_label(&tree, "second\"").is_none(),
            "quoted whitespace must not fabricate an attribute");
    }

    #[test]
    fn attribute_spans_preserve_braced_comparisons_and_spacing() {
        let src = "<button disabled={value > 1 ? true : false} title=\"a b\">go</button>";
        let tree: serde_json::Value =
            serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
        for (label, raw) in [
            ("disabled=value > 1 ? true : false", "disabled={value > 1 ? true : false}"),
            ("title=a b", "title=\"a b\""),
        ] {
            let child = find_label(&tree, label)
                .unwrap_or_else(|| panic!("missing JSX attribute: {label}"));
            let start = src.find(raw).unwrap() as u64;
            assert_eq!(child["position"], serde_json::json!({
                "start_line": 0, "start_col": start,
                "end_line": 0, "end_col": start + raw.len() as u64
            }));
        }
    }

    #[test]
    fn unfinished_quoted_attrs_do_not_panic_or_fabricate_complete_spans() {
        for src in ["<meta title=\"unfinished >", "<p title={unfinished >"] {
            let tree: serde_json::Value =
                serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
            assert!(tree.is_object());
        }
    }

    fn find_label<'a>(value: &'a serde_json::Value, label: &str)
        -> Option<&'a serde_json::Value>
    {
        if value.get("label").and_then(|v| v.as_str()) == Some(label) {
            return Some(value);
        }
        value.get("children").and_then(|c| c.as_array())
            .and_then(|children| children.iter().find_map(|child| find_label(child, label)))
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
