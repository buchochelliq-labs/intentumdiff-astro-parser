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
// A slash in a braced JSX expression may begin a JavaScript regex literal.
// In particular, /[/*]/ contains a comment-looking sequence inside a regex
// character class. Do not mistake it for /* ... */ and discard later tags.
// This is intentionally restricted to expression-start/operator contexts;
// ordinary HTML quoted values are never interpreted as JavaScript.
fn js_regex_literal_end(source: &str, slash: usize) -> Option<usize> {
    let prefix = source[..slash].trim_end();
    let previous = prefix.chars().last();
    // A slash after postfix ++/-- is division, not a regex opener. Both
    // attribute scanners share this classifier; treating it as regex can
    // consume a later closing tag and silently drop subsequent elements.
    if prefix.ends_with("++") || prefix.ends_with("--") {
        return None;
    }
    if !matches!(previous, Some('{' | '(' | '[' | '=' | ':' | ',' | '?' | '!' | '&' | '|' | '+' | '-' | '*' | '%' | '^' | '~')) {
        return None;
    }
    let mut escaped = false;
    let mut in_class = false;
    for (relative, ch) in source[slash + 1..].char_indices() {
        let at = slash + 1 + relative;
        if ch == '\n' {
            return None;
        }
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '[' => in_class = true,
            ']' => in_class = false,
            '/' if !in_class => return Some(at + 1),
            _ => {}
        }
    }
    None
}

fn attribute_token_ranges(attrs: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let mut start = None;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut braces = 0_usize;
    let mut js_block_comment = false;
    let mut js_line_comment = false;
    let mut skip_before_byte = 0_usize;

    for (idx, ch) in attrs.char_indices() {
        if idx < skip_before_byte {
            continue;
        }
        if js_block_comment {
            if ch == '*' && attrs[idx + 1..].starts_with('/') {
                js_block_comment = false;
                skip_before_byte = idx + 2;
            }
            continue;
        }
        if js_line_comment {
            if ch != '\n' {
                continue;
            }
            js_line_comment = false;
        }
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
            // HTML quoted attributes use literal backslashes. Only JS strings
            // inside braced Astro/JSX expressions have backslash escaping.
            if braces > 0 && escaped {
                escaped = false;
            } else if braces > 0 && ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }
        // JS comments inside braced expressions must not change quote or
        // brace depth. The two lexical scanners use the same boundary rule.
        if braces > 0 && ch == '/' {
            if attrs[idx + 1..].starts_with('*') {
                js_block_comment = true;
                skip_before_byte = idx + 2;
                continue;
            }
            if attrs[idx + 1..].starts_with('/') {
                js_line_comment = true;
                skip_before_byte = idx + 2;
                continue;
            }
            if let Some(end) = js_regex_literal_end(attrs, idx) {
                skip_before_byte = end;
                continue;
            }
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '`' if braces > 0 => quote = Some(ch),
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

// Return a source-byte position only for the matching expression brace.
// Treat JS strings, block/line comments and regex character classes as
// lexical bodies: a '}' in one is not the end of an Astro interpolation.
fn balanced_interpolation_end(line: &str, opening: usize) -> Option<usize> {
    let mut depth = 1_usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut block_comment = false;
    let mut skip_before_byte = 0_usize;

    for (relative, ch) in line[opening + 1..].char_indices() {
        let at = opening + 1 + relative;
        if at < skip_before_byte {
            continue;
        }
        if block_comment {
            if ch == '*' && line[at + 1..].starts_with('/') {
                block_comment = false;
                skip_before_byte = at + 2;
            }
            continue;
        }
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        if ch == '/' {
            if line[at + 1..].starts_with('*') {
                block_comment = true;
                skip_before_byte = at + 2;
                continue;
            }
            if line[at + 1..].starts_with('/') {
                // Line comment: the closing brace cannot be on this line.
                return None;
            }
            if let Some(end) = js_regex_literal_end(line, at) {
                skip_before_byte = end;
                continue;
            }
        }
        match ch {
            '"' | '\'' | '\u{60}' => quote = Some(ch),
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(at);
                }
            }
            _ => {}
        }
    }
    None
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
        if let Some(close) = balanced_interpolation_end(line, opening) {
            let raw_label = &line[opening + 1..close];
            let label = raw_label.trim();
            if !label.is_empty() && !label.starts_with('#') && !label.starts_with('/') {
                let start = opening + 1 + raw_label.len() - raw_label.trim_start().len();
                labels.push((label.to_string(), start, start + label.len()));
            }
            cursor = close + 1;
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
    let mut js_block_comment = false;
    let mut js_line_comment = false;
    let mut skip_before_byte = 0_usize;
    for (idx, ch) in after_open.char_indices() {
        if idx < skip_before_byte {
            continue;
        }
        if js_block_comment {
            if ch == '*' && after_open[idx + 1..].starts_with('/') {
                js_block_comment = false;
                skip_before_byte = idx + 2;
            }
            continue;
        }
        if js_line_comment {
            if ch != '\n' {
                continue;
            }
            js_line_comment = false;
        }
        if let Some(active_quote) = quote {
            // HTML quoted attributes use literal backslashes. Only JS strings
            // inside braced Astro/JSX expressions have backslash escaping.
            if braces > 0 && escaped {
                escaped = false;
            } else if braces > 0 && ch == '\\' {
                escaped = true;
            } else if ch == active_quote {
                quote = None;
            }
            continue;
        }
        // JS comments inside braced expressions must not change quote or
        // brace depth. The two lexical scanners use the same boundary rule.
        if braces > 0 && ch == '/' {
            if after_open[idx + 1..].starts_with('*') {
                js_block_comment = true;
                skip_before_byte = idx + 2;
                continue;
            }
            if after_open[idx + 1..].starts_with('/') {
                js_line_comment = true;
                skip_before_byte = idx + 2;
                continue;
            }
            if let Some(end) = js_regex_literal_end(after_open, idx) {
                skip_before_byte = end;
                continue;
            }
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '`' if braces > 0 => quote = Some(ch),
            '{' => braces += 1,
            '}' if braces > 0 => braces -= 1,
            '>' if braces == 0 => return Some(idx),
            _ => {}
        }
    }
    None
}

// Raw-text HTML elements do not interpret "<!--" inside script/style source.
// Match only a real closing tag boundary, preserving original UTF-8 byte offsets.
fn find_raw_text_close(rest: &str, tag: &str) -> Option<usize> {
    let mut cursor = 0;
    while let Some(relative) = rest[cursor..].find("</") {
        let at = cursor + relative;
        let remaining = &rest[at + 2..];
        if remaining.get(..tag.len()).is_some_and(|name| name.eq_ignore_ascii_case(tag))
            && remaining[tag.len()..].chars().next().is_some_and(
                |ch| ch.is_whitespace() || ch == '>' || ch == '/'
            )
        {
            return Some(at);
        }
        cursor = at + 2;
    }
    None
}

// Astro expressions can contain actual JSX elements, e.g.
// {items.map(item => <p id="inside">{item}</p>)}. A '<' in a JS
// string/comment/regex or a comparison must not fabricate template markup.
fn looks_like_nested_jsx_tag(source: &str, at: usize) -> bool {
    let tail = source[at + 1..].strip_prefix('/').unwrap_or(&source[at + 1..]);
    if !tail.chars().next().is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_') {
        return false;
    }
    let prefix = source[..at].trim_end();
    matches!(prefix.chars().last(),
        None | Some('{' | '(' | '[' | '=' | '>' | ':' | ',' | '?'))
        || prefix.ends_with("return")
}

// Find HTML/JSX markup while respecting JavaScript lexical bodies inside
// Astro interpolations. Offsets are original UTF-8 bytes, not rendered text.
fn next_html_open(rest: &str) -> Option<usize> {
    let mut braces = 0_usize;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut block_comment = false;
    let mut line_comment = false;
    let mut skip_before_byte = 0_usize;
    for (idx, ch) in rest.char_indices() {
        if idx < skip_before_byte {
            continue;
        }
        if block_comment {
            if ch == '*' && rest[idx + 1..].starts_with('/') {
                block_comment = false;
                skip_before_byte = idx + 2;
            }
            continue;
        }
        if line_comment {
            if ch == '\n' {
                line_comment = false;
            } else {
                continue;
            }
        }
        if let Some(active) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == active {
                quote = None;
            }
            continue;
        }
        if braces > 0 && ch == '/' {
            if rest[idx + 1..].starts_with('*') {
                block_comment = true;
                skip_before_byte = idx + 2;
                continue;
            }
            if rest[idx + 1..].starts_with('/') {
                line_comment = true;
                skip_before_byte = idx + 2;
                continue;
            }
            if let Some(end) = js_regex_literal_end(rest, idx) {
                skip_before_byte = end;
                continue;
            }
        }
        if braces > 0 {
            match ch {
                '{' => braces += 1,
                '}' => braces -= 1,
                '"' | '\'' | '`' => quote = Some(ch),
                '<' if looks_like_nested_jsx_tag(rest, idx) => return Some(idx),
                _ => {}
            }
            continue;
        }
        match ch {
            '{' => braces = 1,
            '<' => return Some(idx),
            _ => {}
        }
    }
    None
}

fn extract_template_children(content: &str, start_line: u32, id_prefix: &str) -> Vec<SemanticNode> {
    let mut children = Vec::new();
    let mut in_html_comment = false;
    let mut raw_text_tag: Option<String> = None;
    for (line_idx, line) in content.lines().enumerate() {
        let absolute_line = start_line + line_idx as u32;
        let mut rest = line;
        // Mask only non-interpolation comment/raw-text bytes, preserving exact
        // UTF-8 byte columns of all remaining source tokens.
        let mut interpolation_visible = line.as_bytes().to_vec();
        loop {
            // The contents of <script>/<style> are not HTML markup. In
            // particular, a JS/CSS string containing "<!--" cannot open an
            // HTML comment that consumes subsequent semantic elements.
            if let Some(tag) = &raw_text_tag {
                let offset = line.len() - rest.len();
                if let Some(close) = find_raw_text_close(rest, tag) {
                    interpolation_visible[offset..offset + close].fill(b' ');
                    rest = &rest[close..];
                    raw_text_tag = None;
                } else {
                    interpolation_visible[offset..].fill(b' ');
                    break;
                }
            }
            // HTML comments are not tags or interpolations. Skip their bytes
            // before quote/brace tracking, even across multiple lines.
            if in_html_comment {
                let offset = line.len() - rest.len();
                if let Some(end) = rest.find("-->") {
                    interpolation_visible[offset..offset + end + 3].fill(b' ');
                    rest = &rest[end + 3..];
                    in_html_comment = false;
                } else {
                    interpolation_visible[offset..].fill(b' ');
                    break;
                }
            }
            let Some(open_idx) = next_html_open(rest) else { break };
            let after_open = &rest[open_idx + 1..];
            if after_open.starts_with("!--") {
                let start = line.len() - rest.len() + open_idx;
                interpolation_visible[start..start + 4].fill(b' ');
                in_html_comment = true;
                rest = &after_open[3..];
                continue;
            }
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
                    // Native HTML raw-text tags are lowercase. Astro imports
                    // such as <Script> and <Style> are components with slots;
                    // their original source case must survive classification.
                    let source_tag = segment.trim_start()
                        .split(|ch: char| ch.is_whitespace() || ch == '>' || ch == '/')
                        .next()
                        .unwrap_or("");
                    let is_raw_text = matches!(source_tag, "script" | "style")
                        && !segment.trim_end().ends_with('/');
                    let raw_name = if is_raw_text { Some(source_tag.to_string()) } else { None };
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
                    if let Some(raw_name) = raw_name {
                        raw_text_tag = Some(raw_name);
                    }
                }
                rest = &after_open[close_idx + 1..];
            } else {
                break;
            }
        }
        let visible = std::str::from_utf8(&interpolation_visible)
            .expect("masking complete UTF-8 source ranges preserves encoding");
        for (label, start, end) in extract_brace_labels(visible) {
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


    #[test]
    fn postfix_increment_and_decrement_are_division_not_regex_in_attributes() {
        for source in [
            "<div title={value++ / divisor}></div><p id=\"after\"/>",
            "<div title={value-- / divisor}></div><p id=\"after\"/>",
        ] {
            let tree: serde_json::Value =
                serde_json::from_str(&process_impl(source, "Page.astro")).unwrap();
            let later = find_label(&tree, "id=after")
                .expect("postfix operator must not swallow closing tag or next element");
            let raw = "id=\"after\"";
            let start = source.find(raw).unwrap() as u64;
            assert_eq!(later["position"], serde_json::json!({
                "start_line": 0, "start_col": start,
                "end_line": 0, "end_col": start + raw.len() as u64
            }));
            let expected = if source.contains("++") {
                "title=value++ / divisor"
            } else {
                "title=value-- / divisor"
            };
            assert!(find_label(&tree, expected).is_some(),
                "source attribute must survive the operator");
        }
    }

    #[test]
    fn nested_jsx_markup_inside_interpolation_retains_its_attributes() {
        let source = "{items.map(item => <p id=\"inside\">{item}</p>)}<p id=\"after\"/>";
        let tree: serde_json::Value =
            serde_json::from_str(&process_impl(source, "Page.astro")).unwrap();
        for (label, raw) in [
            ("id=inside", "id=\"inside\""),
            ("id=after", "id=\"after\""),
        ] {
            let attr = find_label(&tree, label)
                .unwrap_or_else(|| panic!("missing nested JSX attribute {label}"));
            let start = source.find(raw).unwrap() as u64;
            assert_eq!(attr["position"], serde_json::json!({
                "start_line": 0, "start_col": start,
                "end_line": 0, "end_col": start + raw.len() as u64
            }));
        }
    }

    #[test]
    fn js_comments_and_regex_braces_do_not_hide_following_template_elements() {
        for source in [
            "<p>{/* don't remove */ value}</p><p id=\"after\"/>",
            "<p>{/{/.test(value)}</p><p id=\"after\"/>",
            "<p>{\"<!--\"}</p><p id=\"after\"/>",
        ] {
            let tree: serde_json::Value =
                serde_json::from_str(&process_impl(source, "Page.astro")).unwrap();
            let later = find_label(&tree, "id=after")
                .expect("JS lexical body must not consume following template markup");
            let raw = "id=\"after\"";
            let start = source.find(raw).unwrap() as u64;
            assert_eq!(later["position"], serde_json::json!({
                "start_line": 0, "start_col": start,
                "end_line": 0, "end_col": start + raw.len() as u64
            }));
        }
    }

    #[test]
    fn jsx_backtick_literals_do_not_swallow_later_elements() {
        let src = "<div title={`it's fine`}></div><p id=\"after\"/>";
        let tree: serde_json::Value =
            serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
        for (label, raw) in [
            ("title=`it's fine`", "title={`it's fine`}"),
            ("id=after", "id=\"after\""),
        ] {
            let attr = find_label(&tree, label)
                .unwrap_or_else(|| panic!("missing template attribute: {label}"));
            let start = src.find(raw).unwrap() as u64;
            assert_eq!(attr["position"], serde_json::json!({
                "start_line": 0, "start_col": start,
                "end_line": 0, "end_col": start + raw.len() as u64
            }));
        }
    }

    #[test]
    fn html_comments_do_not_consume_subsequent_tag_attributes() {
        let src = "<!-- don't remove <x> --><p id=\"after\"/>";
        let tree: serde_json::Value =
            serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
        let attr = find_label(&tree, "id=after").expect("paragraph after comment");
        let raw = "id=\"after\"";
        let start = src.find(raw).unwrap() as u64;
        assert_eq!(attr["position"], serde_json::json!({
            "start_line": 0, "start_col": start,
            "end_line": 0, "end_col": start + raw.len() as u64
        }));
        assert!(find_label(&tree, "x").is_none(),
            "HTML comment tags must not become semantic elements");
    }

    #[test]
    fn multiline_comments_do_not_swallow_later_lines() {
        let src = "<!-- don't parse\n<x title='wrong'>\n--> <p id='kept' />";
        let tree: serde_json::Value =
            serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
        assert!(find_label(&tree, "title=wrong").is_none());
        let attr = find_label(&tree, "id=kept").expect("valid tag following comment");
        let last_line = src.lines().nth(2).unwrap();
        let start = last_line.find("id='kept'").unwrap() as u64;
        assert_eq!(attr["position"], serde_json::json!({
            "start_line": 2, "start_col": start,
            "end_line": 2, "end_col": start + "id='kept'".len() as u64
        }));
    }


    #[test]
    fn html_literal_backslash_does_not_escape_closing_attribute_quote() {
        let src = r#"<div title="C:\"></div><p id="after"/>"#;
        let tree: serde_json::Value =
            serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
        for (label, raw) in [
            (r#"title=C:\"#, r#"title="C:\""#),
            ("id=after", r#"id="after""#),
        ] {
            let attr = find_label(&tree, label)
                .unwrap_or_else(|| panic!("missing HTML attribute: {label}"));
            let start = src.find(raw).unwrap() as u64;
            assert_eq!(attr["position"], serde_json::json!({
                "start_line": 0, "start_col": start,
                "end_line": 0, "end_col": start + raw.len() as u64
            }));
        }
    }

    #[test]
    fn javascript_string_escaping_inside_braces_still_works() {
        let src = r#"<div title={value === "a\"b" ? 1 : 2}></div><p id="after"/>"#;
        let tree: serde_json::Value =
            serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
        assert!(find_label(&tree, "id=after").is_some());
        let raw = r#"title={value === "a\"b" ? 1 : 2}"#;
        let start = src.find(raw).unwrap() as u64;
        let attr = find_label(&tree, r#"title=value === "a\"b" ? 1 : 2"#)
            .expect("braced JS string attribute survived");
        assert_eq!(attr["position"], serde_json::json!({
            "start_line": 0, "start_col": start,
            "end_line": 0, "end_col": start + raw.len() as u64
        }));
    }

    #[test]
    fn raw_text_script_style_comment_markers_do_not_consume_later_tags() {
        for (src, next_line) in [
            ("<script>\nconst marker = \"<!--\";\n</script>\n<p id=\"after\"/>", 3),
            ("<style>\np:after { content: \"<!--\"; }\n</style>\n<p id=\"after\"/>", 3),
            ("<script>const marker=\"<!--\";</script><p id=\"after\"/>", 0),
        ] {
            let tree: serde_json::Value =
                serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
            let attr = find_label(&tree, "id=after")
                .expect("raw-text JS/CSS comment marker cannot hide following tag");
            let line = src.lines().nth(next_line).unwrap();
            let start = line.find("id=\"after\"").unwrap() as u64;
            assert_eq!(attr["position"], serde_json::json!({
                "start_line": next_line, "start_col": start,
                "end_line": next_line, "end_col": start + "id=\"after\"".len() as u64
            }));
        }
    }

    #[test]
    fn html_comments_never_create_fake_interpolations() {
        let src = "<!-- {hidden} --> <p>{visible}</p>\n<!-- {also_hidden}\n more {also_hidden} --> <p>{also_visible}</p>";
        let tree: serde_json::Value =
            serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
        assert!(find_label(&tree, "hidden").is_none());
        assert!(find_label(&tree, "also_hidden").is_none());
        for (label, line_index) in [("visible", 0), ("also_visible", 2)] {
            let attr = find_label(&tree, label).expect("visible template interpolation");
            let line = src.lines().nth(line_index).unwrap();
            let start = line.find(label).unwrap() as u64;
            assert_eq!(attr["position"], serde_json::json!({
                "start_line": line_index,
                "start_col": start,
                "end_line": line_index,
                "end_col": start + label.len() as u64
            }));
        }
    }

    #[test]
    fn capitalized_astro_components_keep_slot_tags_and_interpolations() {
        for tag in ["Script", "Style"] {
            let src = format!(
                "<{tag}><p id=\"inside\">{{value}}</p></{tag}><p id=\"after\"/>"
            );
            let tree: serde_json::Value =
                serde_json::from_str(&process_impl(&src, "Page.astro")).unwrap();
            for (label, raw) in [
                ("id=inside", "id=\"inside\""),
                ("value", "value"),
                ("id=after", "id=\"after\""),
            ] {
                let node = find_label(&tree, label)
                    .unwrap_or_else(|| panic!("component {tag} suppressed {label}"));
                let at = src.find(raw).unwrap() as u64;
                assert_eq!(node["position"], serde_json::json!({
                    "start_line": 0, "start_col": at,
                    "end_line": 0, "end_col": at + raw.len() as u64
                }), "component {tag}, node {label}");
            }
        }
        // Lowercase native tags still suppress raw-text interpolation
        // instead of inventing template nodes out of JavaScript/CSS.
        for tag in ["script", "style"] {
            let src = format!(
                "<{tag}>const marker = \"{{not_an_interpolation}}\";</{tag}><p id=\"after\"/>"
            );
            let tree: serde_json::Value =
                serde_json::from_str(&process_impl(&src, "Page.astro")).unwrap();
            assert!(find_label(&tree, "not_an_interpolation").is_none());
            assert!(find_label(&tree, "id=after").is_some());
        }
    }

    #[test]
    fn jsx_attribute_comments_do_not_swallow_later_elements_or_spans() {
        for src in [
            "<div title={/* don't remove */ value}></div><p id=\"after\"/>",
            "<div title={/* don't } > parse */ value}></div><p id=\"after\"/>",
            "<div title={/* café ' } > */ value}></div><p id=\"after\"/>",
        ] {
            let tree: serde_json::Value =
                serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
            let attr = find_label(&tree, "id=after")
                .expect("a JS comment must not swallow following HTML tags");
            let raw = "id=\"after\"";
            let at = src.find(raw).unwrap() as u64;
            assert_eq!(attr["position"], serde_json::json!({
                "start_line": 0, "start_col": at,
                "end_line": 0, "end_col": at + raw.len() as u64
            }));
            let first_attr = find_label(&tree, "title=/* don't remove */ value")
                .or_else(|| find_label(&tree, "title=/* don't } > parse */ value"))
                .or_else(|| find_label(&tree, "title=/* café ' } > */ value"))
                .expect("JS attribute containing comment should survive");
            let start = src.find("title={").unwrap() as u64;
            let end = src.find("}></div>").unwrap() as u64 + 1;
            assert_eq!(first_attr["position"], serde_json::json!({
                "start_line": 0, "start_col": start,
                "end_line": 0, "end_col": end
            }));
        }
    }

    #[test]
    fn jsx_regex_character_classes_do_not_become_comments() {
        let src = r#"<div title={/[/*]/.test(value)}></div><p id="after"/>"#;
        let tree: serde_json::Value =
            serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
        for (label, raw) in [
            ("title=/[/*]/.test(value)", "title={/[/*]/.test(value)}"),
            ("id=after", r#"id="after""#),
        ] {
            let node = find_label(&tree, label)
                .unwrap_or_else(|| panic!("JS regex character class swallowed {label}"));
            let at = src.find(raw).unwrap() as u64;
            assert_eq!(node["position"], serde_json::json!({
                "start_line": 0, "start_col": at,
                "end_line": 0, "end_col": at + raw.len() as u64
            }));
        }
    }

    #[test]
    fn html_comment_markers_inside_interpolation_strings_are_not_comments() {
        for src in [
            r#"<p>{"<!--"}</p><p id="after"/>"#,
            r#"<p>{"café<!--"}</p><p id="after"/>"#,
            r#"<p>{'<!--'}</p><p id="after"/>"#,
        ] {
            let tree: serde_json::Value =
                serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
            let text = if src.contains("café") {
                r#""café<!--""#
            } else if src.contains("'<!--'") {
                "'<!--'"
            } else {
                r#""<!--""#
            };
            let interpolation = find_label(&tree, text)
                .unwrap_or_else(|| panic!("lost source interpolation {text}"));
            let at = src.find(text).unwrap() as u64;
            assert_eq!(interpolation["position"], serde_json::json!({
                "start_line": 0, "start_col": at,
                "end_line": 0, "end_col": at + text.len() as u64
            }), "{src}");
            let later = find_label(&tree, "id=after")
                .expect("quoted comment marker must not hide next element");
            let at = src.find(r#"id="after""#).unwrap() as u64;
            assert_eq!(later["position"], serde_json::json!({
                "start_line": 0, "start_col": at,
                "end_line": 0, "end_col": at + r#"id="after""#.len() as u64
            }), "{src}");
        }
    }

    #[test]
    fn interpolation_source_spans_balance_js_comments_regex_and_nested_braces() {
        for (src, expected) in [
            (
                "<p>{/* don't } remove */ value}</p><p id=\"after\"/>",
                "/* don't } remove */ value",
            ),
            (
                "<p>{/[{}]/.test(text) ? {count: 1} : null}</p><p id=\"after\"/>",
                "/[{}]/.test(text) ? {count: 1} : null",
            ),
            (
                "<p>{\"café }\" + ({v: 1}).v}</p><p id=\"after\"/>",
                "\"café }\" + ({v: 1}).v",
            ),
        ] {
            let tree: serde_json::Value =
                serde_json::from_str(&process_impl(src, "Page.astro")).unwrap();
            let node = find_label(&tree, expected)
                .unwrap_or_else(|| panic!("missing complete Rust interpolation: {expected}"));
            let start = src.find(expected).unwrap() as u64;
            assert_eq!(node["position"], serde_json::json!({
                "start_line": 0, "start_col": start,
                "end_line": 0, "end_col": start + expected.len() as u64
            }), "source span for {src}");
            assert!(find_label(&tree, "id=after").is_some(),
                "lexical bodies must not swallow following markup: {src}");
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
