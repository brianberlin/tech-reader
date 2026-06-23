//! Segment a document into speakable [`Block`]s with absolute source line
//! ranges. Pure string/line processing — no markdown library. Ported from the
//! TS `blocks.ts`. Two strategies by language: a lightweight line-based Markdown
//! scan for prose, and a comment-aware chunker for code.

use std::sync::LazyLock;

use regex::Regex;

use crate::types::{Block, BlockKind};

/// Languages that get the prose (Markdown-ish) treatment.
fn is_prose_lang(lang: &str) -> bool {
    matches!(
        lang,
        "markdown" | "mdx" | "plaintext" | "asciidoc" | "restructuredtext" | "rst" | ""
    )
}

/// Rough chunk targets for the code path (in lines).
const CODE_CHUNK_MAX: usize = 50; // hard-ish cap: split at next blank line past this
const CODE_CHUNK_SOFT: usize = 12; // min non-blank lines before a blank can split

/// Split `source` into speakable blocks. `base_line` is the 1-based document
/// line of the first line of `source`, so every block carries ABSOLUTE document
/// line numbers. Never panics; returns [] for empty/whitespace input.
pub fn segment_blocks(source: &str, lang: &str, base_line: usize) -> Vec<Block> {
    if source.trim().is_empty() {
        return Vec::new();
    }
    // Normalize CRLF/CR so local line indices match the original.
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();

    let lang_lc = lang.to_lowercase();
    if is_prose_lang(&lang_lc) {
        segment_prose(&lines, lang, base_line)
    } else {
        segment_code(&lines, lang, base_line)
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn make_block(
    kind: BlockKind,
    src: &str,
    start_idx: usize,
    end_idx: usize,
    base_line: usize,
    lang: &str,
    heading_level: Option<u8>,
) -> Block {
    Block {
        kind,
        source: src.to_string(),
        start_line: base_line + start_idx,
        end_line: base_line + end_idx,
        heading_level,
        lang: lang.to_string(),
    }
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

// ---------------------------------------------------------------------------
// (A) PROSE: lightweight line-based Markdown scan
// ---------------------------------------------------------------------------

static RE_ATX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$").unwrap());
static RE_FENCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s{0,3}(`{3,}|~{3,})(.*)$").unwrap());
static RE_BLOCKQUOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s{0,3}>\s?(.*)$").unwrap());
static RE_LIST_ITEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\s*)([-*+]|\d{1,9}[.)])\s+(.*)$").unwrap());
static RE_SETEXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s{0,3}(=+|-+)\s*$").unwrap());
static RE_HTML_COMMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*<!--.*-->\s*$").unwrap());
static RE_REF_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s{0,3}\[[^\]]+\]:\s+\S+").unwrap());
static RE_TABLE_CELL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*:?-+:?\s*$").unwrap());

/// A thematic break: 0-3 lead spaces then ≥3 of one of `- * _`, spaces allowed
/// between. Hand-rolled because the TS regex uses a backreference (`\1`).
fn is_thematic_break(line: &str) -> bool {
    let t = line.trim();
    let first = match t.chars().next() {
        Some(c @ ('-' | '*' | '_')) => c,
        _ => return false,
    };
    let mut count = 0;
    for c in t.chars() {
        if c == first {
            count += 1;
        } else if c == ' ' || c == '\t' {
            continue;
        } else {
            return false;
        }
    }
    count >= 3
}

/// A GFM table delimiter row, e.g. `| --- | :--: |`.
fn is_table_delimiter(line: &str) -> bool {
    let t = line.trim();
    if !t.contains('-') {
        return false;
    }
    let mut s = t;
    if let Some(rest) = s.strip_prefix('|') {
        s = rest;
    }
    if let Some(rest) = s.strip_suffix('|') {
        s = rest;
    }
    let cells: Vec<&str> = s.split('|').collect();
    if cells.is_empty() {
        return false;
    }
    cells.iter().all(|c| RE_TABLE_CELL.is_match(c))
}

fn looks_like_table_row(line: &str) -> bool {
    line.contains('|')
}

fn segment_prose(lines: &[&str], lang: &str, base_line: usize) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    // Skip leading YAML/TOML frontmatter only at the true document top.
    let mut i = if base_line == 1 { skip_frontmatter(lines) } else { 0 };

    while i < lines.len() {
        let line = lines[i];

        if is_blank(line) {
            i += 1;
            continue;
        }

        if let Some(marker) = match_opening_fence(line) {
            i = consume_fence(lines, i, &marker, base_line, lang, &mut blocks);
            continue;
        }

        if let Some(caps) = RE_ATX.captures(line) {
            let level = caps.get(1).unwrap().as_str().len() as u8;
            let text = caps.get(2).unwrap().as_str().trim();
            blocks.push(make_block(
                BlockKind::Heading,
                text,
                i,
                i,
                base_line,
                lang,
                Some(level),
            ));
            i += 1;
            continue;
        }

        if is_thematic_break(line) {
            i += 1;
            continue;
        }

        if RE_HTML_COMMENT.is_match(line) {
            i += 1;
            continue;
        }

        if RE_REF_LINK.is_match(line) {
            i += 1;
            continue;
        }

        if looks_like_table_row(line) && i + 1 < lines.len() && is_table_delimiter(lines[i + 1]) {
            i = consume_table(lines, i, base_line, lang, &mut blocks);
            continue;
        }

        if RE_BLOCKQUOTE.is_match(line) {
            i = consume_blockquote(lines, i, base_line, lang, &mut blocks);
            continue;
        }

        if RE_LIST_ITEM.is_match(line) {
            i = consume_list_item(lines, i, base_line, lang, &mut blocks);
            continue;
        }

        i = consume_paragraph_or_setext(lines, i, base_line, lang, &mut blocks);
    }

    blocks
}

/// Index of the first content line after any top frontmatter fence.
fn skip_frontmatter(lines: &[&str]) -> usize {
    let first = match lines.first() {
        Some(l) => l.trim(),
        None => return 0,
    };
    let marker = match first {
        "---" => "---",
        "+++" => "+++",
        _ => return 0,
    };
    for (j, line) in lines.iter().enumerate().skip(1) {
        if line.trim() == marker {
            return j + 1;
        }
    }
    0 // no closing fence: not real frontmatter
}

/// If `line` opens a fenced code block, return its fence marker run.
fn match_opening_fence(line: &str) -> Option<String> {
    RE_FENCE
        .captures(line)
        .map(|c| c.get(1).unwrap().as_str().to_string())
}

fn consume_fence(
    lines: &[&str],
    start: usize,
    marker: &str,
    base_line: usize,
    lang: &str,
    blocks: &mut Vec<Block>,
) -> usize {
    let fence_char = marker.chars().next().unwrap();
    let fence_len = marker.len();
    let inner_start = start + 1;
    let mut j = inner_start;

    while j < lines.len() {
        if let Some(caps) = RE_FENCE.captures(lines[j]) {
            let m = caps.get(1).unwrap().as_str();
            if m.starts_with(fence_char) && m.len() >= fence_len && caps.get(2).unwrap().as_str().trim().is_empty()
            {
                break;
            }
        }
        j += 1;
    }

    if j > inner_start {
        let inner_end = j - 1;
        let src = lines[inner_start..=inner_end].join("\n");
        blocks.push(make_block(
            BlockKind::Code,
            &src,
            inner_start,
            inner_end,
            base_line,
            lang,
            None,
        ));
    }

    if j < lines.len() {
        j + 1
    } else {
        j
    }
}

fn consume_table(
    lines: &[&str],
    start: usize,
    base_line: usize,
    lang: &str,
    blocks: &mut Vec<Block>,
) -> usize {
    let mut j = start;
    while j < lines.len() && !is_blank(lines[j]) && looks_like_table_row(lines[j]) {
        j += 1;
    }
    let src = lines[start..j].join("\n");
    blocks.push(make_block(
        BlockKind::Table,
        &src,
        start,
        j - 1,
        base_line,
        lang,
        None,
    ));
    j
}

fn consume_blockquote(
    lines: &[&str],
    start: usize,
    base_line: usize,
    lang: &str,
    blocks: &mut Vec<Block>,
) -> usize {
    let mut j = start;
    let mut texts: Vec<String> = Vec::new();
    while j < lines.len() {
        match RE_BLOCKQUOTE.captures(lines[j]) {
            Some(caps) => texts.push(caps.get(1).unwrap().as_str().to_string()),
            None => break,
        }
        j += 1;
    }
    let src = texts.join("\n").trim().to_string();
    blocks.push(make_block(
        BlockKind::Quote,
        &src,
        start,
        j - 1,
        base_line,
        lang,
        None,
    ));
    j
}

fn consume_list_item(
    lines: &[&str],
    start: usize,
    base_line: usize,
    lang: &str,
    blocks: &mut Vec<Block>,
) -> usize {
    let first_text = RE_LIST_ITEM
        .captures(lines[start])
        .map(|c| c.get(3).unwrap().as_str().to_string())
        .unwrap_or_else(|| lines[start].to_string());
    let mut parts: Vec<String> = vec![first_text];
    let mut j = start + 1;

    while j < lines.len() {
        let l = lines[j];
        if is_blank(l) {
            break;
        }
        if RE_LIST_ITEM.is_match(l)
            || RE_ATX.is_match(l)
            || RE_BLOCKQUOTE.is_match(l)
            || match_opening_fence(l).is_some()
            || is_thematic_break(l)
        {
            break;
        }
        parts.push(l.trim().to_string());
        j += 1;
    }

    let src = parts.join("\n").trim().to_string();
    blocks.push(make_block(
        BlockKind::ListItem,
        &src,
        start,
        j - 1,
        base_line,
        lang,
        None,
    ));
    j
}

fn consume_paragraph_or_setext(
    lines: &[&str],
    start: usize,
    base_line: usize,
    lang: &str,
    blocks: &mut Vec<Block>,
) -> usize {
    let next = start + 1;
    if next < lines.len() && RE_SETEXT.is_match(lines[next]) && !is_blank(lines[start]) {
        let underline = lines[next].trim();
        let level = if underline.starts_with('=') { 1 } else { 2 };
        blocks.push(make_block(
            BlockKind::Heading,
            lines[start].trim(),
            start,
            next,
            base_line,
            lang,
            Some(level),
        ));
        return next + 1;
    }

    let mut j = start;
    let mut parts: Vec<String> = Vec::new();
    while j < lines.len() {
        let l = lines[j];
        if is_blank(l) {
            break;
        }
        if j > start
            && (RE_ATX.is_match(l)
                || match_opening_fence(l).is_some()
                || RE_BLOCKQUOTE.is_match(l)
                || RE_LIST_ITEM.is_match(l)
                || is_thematic_break(l)
                || RE_SETEXT.is_match(l))
        {
            break;
        }
        parts.push(l.trim().to_string());
        j += 1;
    }

    let src = parts.join("\n").trim().to_string();
    if !src.is_empty() {
        blocks.push(make_block(
            BlockKind::Para,
            &src,
            start,
            j - 1,
            base_line,
            lang,
            None,
        ));
    }
    if j > start {
        j
    } else {
        start + 1
    }
}

// ---------------------------------------------------------------------------
// (B) CODE: comment-aware chunking
// ---------------------------------------------------------------------------

struct CommentSyntax {
    line: Vec<&'static str>,
    block_open: Option<&'static str>,
    block_close: Option<&'static str>,
    docstring: Option<&'static str>,
}

fn comment_syntax_for(lang: &str) -> CommentSyntax {
    let l = lang.to_lowercase();
    let c_like = [
        "typescript",
        "typescriptreact",
        "javascript",
        "javascriptreact",
        "tsx",
        "jsx",
        "java",
        "c",
        "cpp",
        "c++",
        "objective-c",
        "objective-cpp",
        "csharp",
        "cs",
        "go",
        "rust",
        "swift",
        "kotlin",
        "scala",
        "php",
        "dart",
    ];
    let hash_like = [
        "python",
        "ruby",
        "shellscript",
        "shell",
        "sh",
        "bash",
        "zsh",
        "yaml",
        "toml",
        "perl",
        "r",
        "makefile",
        "dockerfile",
    ];
    let dash_like = ["sql", "lua", "haskell"];

    if c_like.contains(&l.as_str()) {
        CommentSyntax {
            line: vec!["//"],
            block_open: Some("/*"),
            block_close: Some("*/"),
            docstring: None,
        }
    } else if l == "python" {
        CommentSyntax {
            line: vec!["#"],
            block_open: None,
            block_close: None,
            docstring: Some("\"\"\""),
        }
    } else if hash_like.contains(&l.as_str()) {
        CommentSyntax {
            line: vec!["#"],
            block_open: None,
            block_close: None,
            docstring: None,
        }
    } else if dash_like.contains(&l.as_str()) {
        CommentSyntax {
            line: vec!["--"],
            block_open: None,
            block_close: None,
            docstring: None,
        }
    } else {
        CommentSyntax {
            line: vec!["//", "#"],
            block_open: None,
            block_close: None,
            docstring: None,
        }
    }
}

fn is_line_comment(line: &str, syntax: &CommentSyntax) -> bool {
    let t = line.trim_start();
    syntax.line.iter().any(|p| t.starts_with(p))
}

fn segment_code(lines: &[&str], lang: &str, base_line: usize) -> Vec<Block> {
    let syntax = comment_syntax_for(lang);
    let mut blocks: Vec<Block> = Vec::new();
    let inside_span = compute_multiline_spans(lines, &syntax);

    let mut chunk_start = 0usize;
    let mut non_blank_in_chunk = 0usize;

    for i in 0..lines.len() {
        let blank = is_blank(lines[i]);
        if !blank {
            non_blank_in_chunk += 1;
        }

        let at_safe_blank = blank && !inside_span[i] && non_blank_in_chunk > 0;
        if !at_safe_blank {
            continue;
        }

        let chunk_len = i - chunk_start;
        let big = chunk_len >= CODE_CHUNK_MAX;
        let soft_split = non_blank_in_chunk >= CODE_CHUNK_SOFT;

        if big || (soft_split && !blank_starts_trailing_comment(lines, i, &syntax)) {
            emit_chunk(lines, chunk_start, i - 1, base_line, lang, &syntax, &mut blocks);
            chunk_start = i + 1;
            non_blank_in_chunk = 0;
        }
    }

    if chunk_start < lines.len() {
        emit_chunk(
            lines,
            chunk_start,
            lines.len() - 1,
            base_line,
            lang,
            &syntax,
            &mut blocks,
        );
    }

    blocks
}

fn blank_starts_trailing_comment(lines: &[&str], i: usize, syntax: &CommentSyntax) -> bool {
    let mut j = i + 1;
    while j < lines.len() && is_blank(lines[j]) {
        j += 1;
    }
    if j >= lines.len() {
        return false;
    }
    is_line_comment(lines[j], syntax) || starts_block_comment(lines[j], syntax)
}

fn starts_block_comment(line: &str, syntax: &CommentSyntax) -> bool {
    let t = line.trim_start();
    if let Some(open) = syntax.block_open {
        if t.starts_with(open) {
            return true;
        }
    }
    if let Some(doc) = syntax.docstring {
        if t.starts_with(doc) {
            return true;
        }
    }
    false
}

fn compute_multiline_spans(lines: &[&str], syntax: &CommentSyntax) -> Vec<bool> {
    let mut inside = vec![false; lines.len()];

    if let (Some(open), Some(close)) = (syntax.block_open, syntax.block_close) {
        let mut is_open = false;
        for (i, line) in lines.iter().enumerate() {
            let t = *line;
            if !is_open {
                if let Some(o_idx) = t.find(open) {
                    let after = o_idx + open.len();
                    if t[after..].find(close).is_none() {
                        is_open = true; // opens here, doesn't close on this line
                    }
                }
            } else {
                inside[i] = true;
                if t.contains(close) {
                    is_open = false;
                }
            }
        }
    }

    if let Some(marker) = syntax.docstring {
        let mut is_open = false;
        for (i, line) in lines.iter().enumerate() {
            let count = occurrences(line, marker);
            if is_open {
                inside[i] = true;
            }
            if count % 2 == 1 {
                is_open = !is_open;
            }
        }
    }

    inside
}

fn occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.matches(needle).count()
}

fn emit_chunk(
    lines: &[&str],
    start: usize,
    end: usize,
    base_line: usize,
    lang: &str,
    syntax: &CommentSyntax,
    blocks: &mut Vec<Block>,
) {
    let mut s = start;
    while s <= end && is_blank(lines[s]) {
        s += 1;
    }
    let mut e = end;
    while e >= s && is_blank(lines[e]) {
        if e == 0 {
            break;
        }
        e -= 1;
    }
    if s > e {
        return;
    }

    let slice = &lines[s..=e];
    let src = slice.join("\n");
    let kind = if is_all_comment(slice, syntax) {
        BlockKind::Comment
    } else {
        BlockKind::Code
    };
    blocks.push(make_block(kind, &src, s, e, base_line, lang, None));
}

fn is_all_comment(slice: &[&str], syntax: &CommentSyntax) -> bool {
    let mut in_block = false;
    let mut in_doc = false;

    for raw in slice {
        if is_blank(raw) {
            continue;
        }
        let t = raw.trim();

        if in_block {
            if let Some(close) = syntax.block_close {
                if let Some(idx) = t.find(close) {
                    in_block = false;
                    let after = t[idx + close.len()..].trim();
                    if !after.is_empty() {
                        return false;
                    }
                }
            }
            continue;
        }
        if in_doc {
            if let Some(doc) = syntax.docstring {
                if occurrences(t, doc) % 2 == 1 {
                    in_doc = false;
                }
            }
            continue;
        }

        if is_line_comment(t, syntax) {
            continue;
        }

        if let (Some(open), Some(close)) = (syntax.block_open, syntax.block_close) {
            if t.starts_with(open) {
                match t[open.len()..].find(close) {
                    None => {
                        in_block = true;
                        continue;
                    }
                    Some(rel) => {
                        let close_idx = open.len() + rel;
                        let after = t[close_idx + close.len()..].trim();
                        if after.is_empty() {
                            continue;
                        }
                        return false;
                    }
                }
            }
        }

        if let Some(doc) = syntax.docstring {
            if t.starts_with(doc) {
                if occurrences(t, doc) % 2 == 1 {
                    in_doc = true;
                }
                continue;
            }
        }

        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heading_and_paragraph() {
        let blocks = segment_blocks("# Title\n\nA paragraph here.", "markdown", 1);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].kind, BlockKind::Heading);
        assert_eq!(blocks[0].source, "Title");
        assert_eq!(blocks[0].heading_level, Some(1));
        assert_eq!(blocks[0].start_line, 1);
        assert_eq!(blocks[1].kind, BlockKind::Para);
        assert_eq!(blocks[1].source, "A paragraph here.");
        assert_eq!(blocks[1].start_line, 3);
    }

    #[test]
    fn fenced_code_block() {
        let blocks = segment_blocks("```rust\nlet x = 1;\n```", "markdown", 1);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Code);
        assert_eq!(blocks[0].source, "let x = 1;");
    }

    #[test]
    fn gfm_table() {
        let md = "| A | B |\n| --- | --- |\n| 1 | 2 |";
        let blocks = segment_blocks(md, "markdown", 1);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Table);
    }

    #[test]
    fn list_items() {
        let blocks = segment_blocks("- one\n- two", "markdown", 1);
        assert_eq!(blocks.len(), 2);
        assert!(blocks.iter().all(|b| b.kind == BlockKind::ListItem));
        assert_eq!(blocks[0].source, "one");
    }

    #[test]
    fn thematic_break_skipped() {
        let blocks = segment_blocks("Para one.\n\n---\n\nPara two.", "markdown", 1);
        // The `---` rule is skipped; only two paragraphs remain.
        assert_eq!(blocks.len(), 2);
        assert!(blocks.iter().all(|b| b.kind == BlockKind::Para));
    }

    #[test]
    fn code_comment_vs_code() {
        let src = "// a leading comment\nfn main() {\n    println!(\"hi\");\n}";
        let blocks = segment_blocks(src, "rust", 1);
        // Leading comment stays attached to the code it documents -> one chunk.
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Code);
    }

    #[test]
    fn pure_comment_chunk() {
        let src = "// just a comment\n// spanning two lines";
        let blocks = segment_blocks(src, "rust", 1);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Comment);
    }
}
