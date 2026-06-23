//! Deterministic "speech humanizer" — the OFFLINE fallback used when the AI
//! narrator (Ollama) is unavailable. Turns code, identifiers, and technical
//! prose into natural spoken text so TTS never reads `return_item` as "return
//! underscore item". Ported from the TS `humanizer.ts`. Pure and total: never
//! panics on any input.
//!
//! - [`humanize_word`]  — one identifier  -> spoken fragments
//! - [`humanize_prose`] — running text     -> identifier-looking tokens humanized
//! - [`humanize_code`]  — source code      -> comments as prose, identifiers
//!   humanized, meaningful operators spoken, noise punctuation dropped

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::{Captures, Regex};

// ---------------------------------------------------------------------------
// Dictionaries / acronyms
// ---------------------------------------------------------------------------

/// Lowercased identifier fragment -> spoken expansion. Words that already read
/// fine are deliberately left alone.
static DEFAULT_DICTIONARY: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    [
        ("fn", "function"), ("func", "function"), ("idx", "index"), ("ctx", "context"),
        ("msg", "message"), ("btn", "button"), ("num", "number"), ("str", "string"),
        ("arr", "array"), ("obj", "object"), ("val", "value"), ("vals", "values"),
        ("req", "request"), ("reqs", "requests"), ("res", "response"), ("resp", "response"),
        ("err", "error"), ("errs", "errors"), ("db", "database"), ("env", "environment"),
        ("envs", "environments"), ("repo", "repository"), ("repos", "repositories"),
        ("dir", "directory"), ("dirs", "directories"), ("init", "initialize"),
        ("util", "utility"), ("utils", "utilities"), ("impl", "implementation"),
        ("impls", "implementations"), ("len", "length"), ("src", "source"),
        ("dest", "destination"), ("dst", "destination"), ("tmp", "temporary"),
        ("temp", "temporary"), ("prev", "previous"), ("curr", "current"), ("cur", "current"),
        ("pos", "position"), ("elem", "element"), ("elems", "elements"), ("el", "element"),
        ("attr", "attribute"), ("attrs", "attributes"), ("param", "parameter"),
        ("params", "parameters"), ("arg", "argument"), ("args", "arguments"),
        ("var", "variable"), ("vars", "variables"), ("decl", "declaration"),
        ("calc", "calculate"), ("gen", "generate"), ("fmt", "format"), ("parse", "parse"),
        ("mgr", "manager"), ("svc", "service"), ("svcs", "services"),
        ("auth", "authentication"), ("authz", "authorization"), ("asc", "ascending"),
        ("desc", "descending"), ("addr", "address"), ("char", "character"),
        ("chars", "characters"), ("ptr", "pointer"), ("ref", "reference"),
        ("refs", "references"), ("regex", "reg ex"), ("cmd", "command"),
        ("cmds", "commands"), ("pkg", "package"), ("pkgs", "packages"), ("lib", "library"),
        ("libs", "libraries"), ("doc", "document"), ("docs", "documents"), ("cnt", "count"),
        ("amt", "amount"), ("qty", "quantity"), ("desc_", "description"),
        ("descr", "description"), ("cb", "callback"), ("conn", "connection"),
        ("conns", "connections"), ("proc", "process"), ("sync", "sync"),
        ("recv", "receive"), ("sched", "schedule"), ("stmt", "statement"),
        ("expr", "expression"), ("cond", "condition"), ("iter", "iterate"),
        ("seq", "sequence"), ("acc", "accumulator"), ("prop", "property"),
        ("props", "properties"), ("opt", "option"), ("opts", "options"),
        ("def", "definition"), ("config", "config"), ("info", "info"), ("spec", "spec"),
        ("admin", "admin"), ("data", "data"),
    ]
    .into_iter()
    .collect()
});

/// Acronyms output as spaced uppercase letters so TTS spells them ("H T T P").
static ACRONYMS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "http", "https", "url", "uri", "api", "sdk", "cli", "id", "uuid", "io", "ui", "ux",
        "db", "sql", "json", "xml", "yaml", "html", "css", "js", "ts", "cpu", "gpu", "ram",
        "os", "ip", "tcp", "udp", "dns", "ssh", "tls", "ssl", "jwt", "orm", "crud", "rest",
        "rpc", "ast", "ascii", "utf", "csv", "png", "jpg", "gif", "svg", "pdf", "http2",
        "oauth", "gui", "usb", "pid", "tty",
    ]
    .into_iter()
    .collect()
});

/// Acronyms that read fine as words, so we don't force letter-spelling.
fn spoken_acronym(lower: &str) -> Option<&'static str> {
    match lower {
        "regexp" => Some("reg exp"),
        "regex" => Some("reg ex"),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn has_letter(s: &str) -> bool {
    s.chars().any(|c| c.is_alphabetic())
}

fn spell(s: &str) -> String {
    s.to_uppercase()
        .chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Merge DEFAULT_DICTIONARY with a (lowercased) user dictionary; user wins.
fn merge_dict(user: &HashMap<String, String>) -> HashMap<String, String> {
    let mut d: HashMap<String, String> = DEFAULT_DICTIONARY
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    for (k, v) in user {
        d.insert(k.to_lowercase(), v.clone());
    }
    d
}

/// Format a split fragment for speech: acronyms spelled, stray capital kept,
/// numbers as-is, else lowercase.
fn format_fragment(frag: &str) -> String {
    if frag.is_empty() {
        return String::new();
    }
    let lower = frag.to_lowercase();
    if let Some(v) = spoken_acronym(&lower) {
        return v.to_string();
    }
    if ACRONYMS.contains(lower.as_str()) {
        if let Some((letters, digits)) = split_acronym_digits(&lower) {
            return format!("{} {}", spell(letters), digits);
        }
        return spell(&lower);
    }
    if frag.chars().count() == 1 && frag.chars().next().unwrap().is_ascii_uppercase() {
        return frag.to_string();
    }
    if !frag.is_empty() && frag.chars().all(|c| c.is_ascii_digit()) {
        return frag.to_string();
    }
    lower
}

/// `^([a-z]+)(\d+)$` split for acronym+digit fragments like "http2".
fn split_acronym_digits(lower: &str) -> Option<(&str, &str)> {
    let split = lower.find(|c: char| c.is_ascii_digit())?;
    let (letters, digits) = lower.split_at(split);
    if !letters.is_empty()
        && letters.chars().all(|c| c.is_ascii_lowercase())
        && !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
    {
        Some((letters, digits))
    } else {
        None
    }
}

/// Expand a fragment via dictionary (acronyms are never expanded).
fn expand_fragment(frag: &str, dict: &HashMap<String, String>) -> String {
    if frag.is_empty() {
        return String::new();
    }
    let lower = frag.to_lowercase();
    if ACRONYMS.contains(lower.as_str()) {
        return frag.to_string();
    }
    dict.get(&lower).cloned().unwrap_or_else(|| frag.to_string())
}

// ---------------------------------------------------------------------------
// Identifier splitting
// ---------------------------------------------------------------------------

/// Split a separator-free identifier core into camelCase / acronym / digit
/// fragments. Hand-rolled because the TS regex uses lookahead (unsupported by
/// the `regex` crate).
fn split_camel(core: &str) -> Vec<String> {
    if core.is_empty() {
        return Vec::new();
    }
    let chars: Vec<char> = core.chars().collect();
    let n = chars.len();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;

    while i < n {
        let c = chars[i];
        if c.is_ascii_uppercase() {
            // Maximal uppercase run.
            let mut j = i;
            while j < n && chars[j].is_ascii_uppercase() {
                j += 1;
            }
            // If a lowercase follows, the LAST uppercase begins a Capitalized
            // word, so the acronym chunk ends one earlier.
            let word_start = if j < n && chars[j].is_ascii_lowercase() {
                j - 1
            } else {
                j
            };
            if word_start > i {
                out.push(chars[i..word_start].iter().collect());
            }
            i = word_start;
            if i < n && chars[i].is_ascii_uppercase() {
                let mut k = i + 1;
                while k < n && chars[k].is_ascii_lowercase() {
                    k += 1;
                }
                out.push(chars[i..k].iter().collect());
                i = k;
            }
        } else if c.is_ascii_lowercase() {
            let mut k = i;
            while k < n && chars[k].is_ascii_lowercase() {
                k += 1;
            }
            out.push(chars[i..k].iter().collect());
            i = k;
        } else if c.is_ascii_digit() {
            let mut k = i;
            while k < n && chars[k].is_ascii_digit() {
                k += 1;
            }
            out.push(chars[i..k].iter().collect());
            i = k;
        } else {
            // Other runs (non-ASCII letters, punctuation). Group non-space,
            // non-ASCII-alnum chars.
            let mut k = i;
            while k < n
                && !chars[k].is_ascii_uppercase()
                && !chars[k].is_ascii_lowercase()
                && !chars[k].is_ascii_digit()
                && !chars[k].is_whitespace()
            {
                k += 1;
            }
            if k == i {
                k += 1;
            }
            out.push(chars[i..k].iter().collect());
            i = k;
        }
    }

    if out.is_empty() && has_letter(core) {
        return vec![core.to_string()];
    }
    out
}

/// Split a letter/digit-glued fragment ("utf8" -> "utf","8"). Unicode-aware.
fn split_letter_digit(frag: &str) -> Vec<String> {
    static LD1: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\p{L}+)(\d+)$").unwrap());
    static LD2: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)(\p{L}+)$").unwrap());
    if let Some(c) = LD1.captures(frag) {
        return vec![c[1].to_string(), c[2].to_string()];
    }
    if let Some(c) = LD2.captures(frag) {
        return vec![c[1].to_string(), c[2].to_string()];
    }
    vec![frag.to_string()]
}

static RE_SEP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[_\-]+").unwrap());

fn humanize_identifier_segment(seg: &str, dict: &HashMap<String, String>) -> String {
    if seg.is_empty() {
        return String::new();
    }
    let normalized = RE_SEP.replace_all(seg, " ");
    let normalized = normalized.trim();
    if normalized.is_empty() {
        return String::new();
    }

    let mut fragments: Vec<String> = Vec::new();
    for piece in normalized.split_whitespace() {
        for camel in split_camel(piece) {
            for ld in split_letter_digit(&camel) {
                fragments.push(ld);
            }
        }
    }

    let mut spoken: Vec<String> = Vec::new();
    for frag in &fragments {
        let expanded = expand_fragment(frag, dict);
        if expanded.contains(' ') {
            for sub in expanded.split_whitespace() {
                spoken.push(format_fragment(sub));
            }
        } else {
            spoken.push(format_fragment(&expanded));
        }
    }

    squeeze(&spoken.join(" "))
}

fn word_with_dict(word: &str, dict: &HashMap<String, String>) -> String {
    let w = word.trim();
    if w.is_empty() {
        return String::new();
    }
    let mut segments: Vec<String> = Vec::new();
    for part in w.split('.').filter(|p| !p.is_empty()) {
        segments.push(humanize_identifier_segment(part, dict));
    }
    squeeze(&segments.join(" "))
}

/// Humanize one identifier token into spoken fragments.
pub fn humanize_word(word: &str, user_dict: &HashMap<String, String>) -> String {
    word_with_dict(word, &merge_dict(user_dict))
}

// ---------------------------------------------------------------------------
// Prose humanization
// ---------------------------------------------------------------------------

static SNAKE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\p{L}_[\p{L}\p{N}_]*").unwrap());
static KEBAB_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\p{L}-[\p{L}\p{N}\-]*\p{L}").unwrap());
static CAMEL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\p{Ll}\p{Lu}|\p{Lu}\p{Lu}\p{Ll}").unwrap());
static DOTTED_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\p{L}[\p{L}\p{N}_]*\.[\p{L}_][\p{L}\p{N}_.]*").unwrap());

fn looks_like_code(tok: &str) -> bool {
    if tok.is_empty() || !has_letter(tok) {
        return false;
    }
    SNAKE_RE.is_match(tok)
        || KEBAB_RE.is_match(tok)
        || DOTTED_RE.is_match(tok)
        || CAMEL_RE.is_match(tok)
}

static RE_PATH_QUERY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[?#].*$").unwrap());

fn humanize_path_token(tok: &str, dict: &HashMap<String, String>) -> String {
    let cleaned = RE_PATH_QUERY.replace(tok, "");
    let last = cleaned
        .split(|c| c == '\\' || c == '/')
        .filter(|p| !p.is_empty())
        .next_back()
        .unwrap_or(&cleaned)
        .to_string();
    humanize_filename(&last, dict)
}

static RE_SHORT_EXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^[a-z]{1,4}$").unwrap());

fn humanize_filename(name: &str, dict: &HashMap<String, String>) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 && dot < name.len() - 1 => {
            let base = &name[..dot];
            let ext = &name[dot + 1..];
            let base_spoken = word_with_dict(base, dict);
            let ext_spoken = if RE_SHORT_EXT.is_match(ext) && !ACRONYMS.contains(ext.to_lowercase().as_str())
            {
                spell_lower(ext)
            } else {
                word_with_dict(ext, dict)
            };
            squeeze(&format!("{base_spoken} dot {ext_spoken}"))
        }
        _ => word_with_dict(name, dict),
    }
}

/// Lowercase then space-separate each character ("ts" -> "t s").
fn spell_lower(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

static RE_URL_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-zA-Z][a-zA-Z0-9+.\-]*://([^/\s?#]+)").unwrap());

fn humanize_url(url: &str) -> String {
    let host = match RE_URL_HOST.captures(url) {
        Some(c) => c[1].to_string(),
        None => return "a link".to_string(),
    };
    let host = if host.to_lowercase().starts_with("www.") {
        &host[4..]
    } else {
        &host[..]
    };
    let spoken = host
        .split('.')
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" dot ");
    if spoken.is_empty() {
        "a link".to_string()
    } else {
        spoken
    }
}

/// Inline prose symbol mappings (only clearly meaningful ones), in order.
static PROSE_SYMBOLS: LazyLock<Vec<(Regex, &'static str)>> = LazyLock::new(|| {
    vec![
        (Regex::new(r"=>").unwrap(), " arrow "),
        (Regex::new(r"===|==").unwrap(), " equals "),
        (Regex::new(r"!==|!=").unwrap(), " not equals "),
        (Regex::new(r">=").unwrap(), " greater than or equal "),
        (Regex::new(r"<=").unwrap(), " less than or equal "),
        (Regex::new(r"&&").unwrap(), " and "),
        (Regex::new(r"\|\|").unwrap(), " or "),
    ]
});

static RE_URL_GLOBAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[a-zA-Z][a-zA-Z0-9+.\-]*://[^\s)]+").unwrap());
static RE_BACKTICK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"`([^`]+)`").unwrap());
static RE_SINGLE_TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\S+$").unwrap());
static RE_NONSPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\S+").unwrap());

fn prose_with_dict(text: &str, dict: &HashMap<String, String>) -> String {
    if text.is_empty() {
        return String::new();
    }
    // 1) URLs first.
    let s = RE_URL_GLOBAL.replace_all(text, |c: &Captures| humanize_url(&c[0]));
    // 2) Inline-code spans.
    let s = RE_BACKTICK.replace_all(&s, |c: &Captures| {
        let inner = c[1].trim();
        if inner.is_empty() {
            String::new()
        } else if RE_SINGLE_TOKEN.is_match(inner) {
            humanize_token_for_prose(inner, dict)
        } else {
            prose_with_dict(inner, dict)
        }
    });
    // 3) Meaningful inline symbols.
    let mut s = s.into_owned();
    for (re, word) in PROSE_SYMBOLS.iter() {
        s = re.replace_all(&s, *word).into_owned();
    }
    // 4) Walk remaining tokens.
    let s = RE_NONSPACE.replace_all(&s, |c: &Captures| humanize_maybe_code_token(&c[0], dict));
    squeeze(&s)
}

/// Humanize running prose: ordinary English untouched; identifiers, inline
/// code, paths, URLs fixed; meaningful symbols expanded.
pub fn humanize_prose(text: &str, user_dict: &HashMap<String, String>) -> String {
    prose_with_dict(text, &merge_dict(user_dict))
}

static RE_LEAD: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[^\p{L}\p{N}]+").unwrap());
static RE_TRAIL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}]+$").unwrap());
static RE_SENT_PUNCT_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[.,;:!?]+$").unwrap());

fn humanize_maybe_code_token(raw: &str, dict: &HashMap<String, String>) -> String {
    let lead_end = RE_LEAD.find(raw).map(|m| m.end()).unwrap_or(0);
    let trail_start = RE_TRAIL.find(raw).map(|m| m.start()).unwrap_or(raw.len());
    let (cs, ce, trail) = if trail_start < lead_end {
        (0, raw.len(), "")
    } else {
        (lead_end, trail_start, &raw[trail_start..])
    };
    let core = &raw[cs..ce];
    if core.is_empty() {
        return raw.to_string();
    }
    let kept_trail = RE_SENT_PUNCT_END
        .find(trail)
        .map(|m| m.as_str().to_string())
        .unwrap_or_default();
    let spoken = humanize_token_for_prose(core, dict);
    format!("{spoken}{kept_trail}")
}

static RE_HAS_SLASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[\\/]").unwrap());
static RE_FILENAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[\p{L}\p{N}_\-]+\.[\p{L}\p{N}]{1,5}$").unwrap());

fn humanize_token_for_prose(core: &str, dict: &HashMap<String, String>) -> String {
    if RE_HAS_SLASH.is_match(core) && has_letter(core) {
        return humanize_path_token(core, dict);
    }
    if RE_FILENAME.is_match(core) && has_letter(core) {
        return humanize_filename(core, dict);
    }
    if looks_like_code(core) {
        return word_with_dict(core, dict);
    }
    core.to_string()
}

// ---------------------------------------------------------------------------
// Code humanization
// ---------------------------------------------------------------------------

struct CommentMarkers {
    line: Vec<&'static str>,
    block_open: Option<&'static str>,
    block_close: Option<&'static str>,
}

fn markers_for_lang(lang: &str) -> CommentMarkers {
    let l = lang.to_lowercase();
    let c_block = CommentMarkers {
        line: vec!["//"],
        block_open: Some("/*"),
        block_close: Some("*/"),
    };
    match l.as_str() {
        "typescript" | "typescriptreact" | "javascript" | "javascriptreact" | "java" | "c"
        | "cpp" | "c++" | "csharp" | "cs" | "go" | "rust" | "swift" | "kotlin" | "scala"
        | "php" | "dart" => c_block,
        "python" | "ruby" | "shellscript" | "shell" | "bash" | "sh" | "yaml" | "toml"
        | "perl" | "r" | "makefile" => CommentMarkers {
            line: vec!["#"],
            block_open: None,
            block_close: None,
        },
        "sql" | "lua" | "haskell" => CommentMarkers {
            line: vec!["--"],
            block_open: Some("/*"),
            block_close: Some("*/"),
        },
        "lisp" | "clojure" | "scheme" | "elisp" => CommentMarkers {
            line: vec![";"],
            block_open: None,
            block_close: None,
        },
        _ => CommentMarkers {
            line: vec!["//", "#"],
            block_open: Some("/*"),
            block_close: Some("*/"),
        },
    }
}

/// Operators mapped to spoken words; longest first so "===" beats "==".
const CODE_OPERATORS: &[(&str, &str)] = &[
    ("=>", "arrow"),
    ("===", "equals"),
    ("!==", "not equals"),
    ("==", "equals"),
    ("!=", "not equals"),
    (">=", "greater than or equal"),
    ("<=", "less than or equal"),
    ("&&", "and"),
    ("||", "or"),
    ("+=", "plus equals"),
    ("-=", "minus equals"),
    ("*=", "times equals"),
    ("/=", "divided equals"),
    ("->", "arrow"),
    ("::", "colon colon"),
    ("??", "or else"),
    ("...", "spread"),
    ("=", "equals"),
    ("+", "plus"),
    ("*", "times"),
    ("%", "modulo"),
    ("<", "less than"),
    (">", "greater than"),
    ("!", "not"),
];

fn is_noise_char(c: char) -> bool {
    matches!(c, '{' | '}' | '(' | ')' | '[' | ']' | ',' | ':')
}

static RE_COMMENT_STARS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\*+\s?").unwrap());

fn strip_comment_stars(s: &str) -> String {
    RE_COMMENT_STARS.replace(s, "").trim().to_string()
}

fn match_line_comment(trimmed: &str, prefixes: &[&str]) -> Option<String> {
    for p in prefixes {
        if let Some(rest) = trimmed.strip_prefix(p) {
            return Some(rest.trim().to_string());
        }
    }
    None
}

/// Split a code line into (code, comment) at the first line-comment marker not
/// inside a string literal. Best-effort.
fn split_trailing_comment(line: &str, prefixes: &[&str]) -> (String, String) {
    let chars: Vec<char> = line.chars().collect();
    let mut in_str: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        let prev = if i > 0 { chars[i - 1] } else { '\0' };
        if let Some(q) = in_str {
            if ch == q && prev != '\\' {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if ch == '"' || ch == '\'' || ch == '`' {
            in_str = Some(ch);
            i += 1;
            continue;
        }
        for p in prefixes {
            let pc: Vec<char> = p.chars().collect();
            if chars[i..].starts_with(&pc[..]) {
                let code: String = chars[..i].iter().collect();
                let comment: String = chars[i + pc.len()..].iter().collect();
                return (code, comment.trim().to_string());
            }
        }
        i += 1;
    }
    (line.to_string(), String::new())
}

fn ensure_sentence(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        return String::new();
    }
    if matches!(t.chars().last(), Some('.') | Some('!') | Some('?')) {
        t.to_string()
    } else {
        format!("{t}.")
    }
}

/// Humanize a chunk of source code into listenable spoken text.
pub fn humanize_code(source: &str, lang: &str, user_dict: &HashMap<String, String>) -> String {
    let dict = merge_dict(user_dict);
    code_with_dict(source, lang, &dict)
}

fn code_with_dict(source: &str, lang: &str, dict: &HashMap<String, String>) -> String {
    if source.is_empty() {
        return String::new();
    }
    let markers = markers_for_lang(lang);
    let mut out: Vec<String> = Vec::new();
    let mut in_block_comment = false;

    for raw in source.split(|c| c == '\n').map(|l| l.trim_end_matches('\r')) {
        // Guard against pathological single huge lines.
        let line: String = if raw.chars().count() > 20000 {
            raw.chars().take(20000).collect()
        } else {
            raw.to_string()
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if in_block_comment {
            let close = markers.block_close.and_then(|c| trimmed.find(c));
            if let Some(idx) = close {
                let inner = &trimmed[..idx];
                let p = prose_with_dict(&strip_comment_stars(inner), dict);
                if !p.is_empty() {
                    out.push(ensure_sentence(&p));
                }
                in_block_comment = false;
            } else {
                let p = prose_with_dict(&strip_comment_stars(trimmed), dict);
                if !p.is_empty() {
                    out.push(ensure_sentence(&p));
                }
            }
            continue;
        }

        if let Some(comment) = match_line_comment(trimmed, &markers.line) {
            let p = prose_with_dict(&comment, dict);
            if !p.is_empty() {
                out.push(ensure_sentence(&p));
            }
            continue;
        }

        if let Some(open) = markers.block_open {
            if let Some(open_idx) = trimmed.find(open) {
                let before = &trimmed[..open_idx];
                let after_open = &trimmed[open_idx + open.len()..];
                let close_idx = markers.block_close.and_then(|c| after_open.find(c));
                if !before.trim().is_empty() {
                    let c = humanize_code_line(before, dict);
                    if !c.is_empty() {
                        out.push(ensure_sentence(&c));
                    }
                }
                if let Some(ci) = close_idx {
                    let inner = &after_open[..ci];
                    let p = prose_with_dict(&strip_comment_stars(inner), dict);
                    if !p.is_empty() {
                        out.push(ensure_sentence(&p));
                    }
                } else {
                    in_block_comment = true;
                    let p = prose_with_dict(&strip_comment_stars(after_open), dict);
                    if !p.is_empty() {
                        out.push(ensure_sentence(&p));
                    }
                }
                continue;
            }
        }

        // Plain code line: strip a trailing line-comment, narrate code then comment.
        let (code_part, comment_part) = split_trailing_comment(&line, &markers.line);
        let c = humanize_code_line(&code_part, dict);
        if !c.is_empty() {
            out.push(ensure_sentence(&c));
        }
        if !comment_part.is_empty() {
            let p = prose_with_dict(&comment_part, dict);
            if !p.is_empty() {
                out.push(ensure_sentence(&p));
            }
        }
    }

    squeeze(&out.join(" "))
}

static RE_TIDY_DOT_A: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+\.").unwrap());
static RE_TIDY_DOT_B: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.\s*\.").unwrap());

fn humanize_code_line(line: &str, dict: &HashMap<String, String>) -> String {
    if line.trim().is_empty() {
        return String::new();
    }
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;

    while i < n {
        let ch = chars[i];

        if ch.is_whitespace() {
            i += 1;
            continue;
        }

        // String literal.
        if ch == '"' || ch == '\'' || ch == '`' {
            let quote = ch;
            let mut j = i + 1;
            let mut buf = String::new();
            while j < n {
                let cj = chars[j];
                if cj == '\\' && j + 1 < n {
                    buf.push(chars[j + 1]);
                    j += 2;
                    continue;
                }
                if cj == quote {
                    j += 1;
                    break;
                }
                buf.push(cj);
                j += 1;
            }
            let inner = squeeze(&prose_with_dict(&buf, dict));
            if !inner.is_empty() {
                out.push(format!("string {inner}"));
            } else if buf.is_empty() {
                out.push("empty string".to_string());
            } else {
                let count = buf.chars().count();
                out.push(format!("string {count} space{}", if count > 1 { "s" } else { "" }));
            }
            i = j;
            continue;
        }

        // Number literal (incl. hex/float).
        if ch.is_ascii_digit() || (ch == '.' && i + 1 < n && chars[i + 1].is_ascii_digit()) {
            let mut j = i;
            while j < n && matches!(chars[j], '0'..='9' | 'a'..='f' | 'A'..='F' | 'x' | 'X' | 'o' | 'b' | '.' | '_')
            {
                j += 1;
            }
            let num: String = chars[i..j].iter().collect();
            out.push(read_number(&num));
            i = j;
            continue;
        }

        // Identifier.
        if ch.is_alphabetic() || ch == '_' || ch == '$' {
            let mut j = i;
            while j < n {
                let cj = chars[j];
                let ident_char = cj.is_alphanumeric() || cj == '_' || cj == '$' || cj == '.';
                if !ident_char {
                    break;
                }
                if cj == '.'
                    && (j + 1 >= n || !(chars[j + 1].is_alphabetic() || chars[j + 1] == '_' || chars[j + 1] == '$'))
                {
                    break;
                }
                j += 1;
            }
            let ident: String = chars[i..j].iter().collect();
            let spoken = word_with_dict(&ident, dict);
            if !spoken.is_empty() {
                out.push(spoken);
            }
            i = j;
            continue;
        }

        // Operators (longest first).
        let mut matched = false;
        for (op, w) in CODE_OPERATORS {
            let opc: Vec<char> = op.chars().collect();
            if chars[i..].starts_with(&opc[..]) {
                out.push(w.to_string());
                i += opc.len();
                matched = true;
                break;
            }
        }
        if matched {
            continue;
        }

        if ch == ';' {
            out.push(".".to_string());
            i += 1;
            continue;
        }

        if is_noise_char(ch) {
            i += 1;
            continue;
        }

        i += 1; // stray symbol -> drop
    }

    let joined = out.join(" ");
    let joined = RE_TIDY_DOT_A.replace_all(&joined, ".");
    let joined = RE_TIDY_DOT_B.replace_all(&joined, ".");
    squeeze(&joined)
}

fn read_number(num: &str) -> String {
    if num.is_empty() {
        return String::new();
    }
    // Hex like 0xFF -> "hex F F".
    let lower = num.to_lowercase();
    if let Some(rest) = lower.strip_prefix("0x") {
        if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_hexdigit()) {
            return format!("hex {}", spell(rest));
        }
    }
    let t = num.replace('_', "");
    if t.matches('.').count() > 1 {
        return t
            .split('.')
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(" dot ");
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn w(s: &str) -> String {
        humanize_word(s, &HashMap::new())
    }
    fn p(s: &str) -> String {
        humanize_prose(s, &HashMap::new())
    }
    fn c(s: &str, lang: &str) -> String {
        humanize_code(s, lang, &HashMap::new())
    }

    #[test]
    fn words() {
        assert_eq!(w("getUserByID"), "get user by I D");
        assert_eq!(w("return_item"), "return item");
        assert_eq!(w("MAX_LEN"), "max length");
        assert_eq!(w("getHTTPResponse"), "get H T T P response");
        assert_eq!(w("parseURLString"), "parse U R L string");
        assert_eq!(w("IOError"), "I O error");
        assert_eq!(w("__init__"), "initialize");
        assert_eq!(w("this.value"), "this value");
        assert_eq!(w("user.profile.name"), "user profile name");
        assert_eq!(w("utf8"), "U T F 8");
        assert_eq!(w("base64"), "base 64");
        assert_eq!(w("MAX_BUFFER_SIZE"), "max buffer size");
    }

    #[test]
    fn prose() {
        assert_eq!(p("call `getUserByID` now"), "call get user by I D now");
        // "ts" is an acronym, so the extension spells as "T S" (the doc comment's
        // lowercase "t s" was shorthand; this matches the TS code's real output).
        assert_eq!(p("see src/utils/foo.ts"), "see foo dot T S");
        // Plain English is left alone.
        assert_eq!(p("the quick brown fox"), "the quick brown fox");
    }

    #[test]
    fn code() {
        assert_eq!(c("const x = a && b;", "typescript"), "const x equals a and b.");
    }

    #[test]
    fn never_panics_on_junk() {
        // Just exercise odd inputs; must not panic.
        let _ = w("");
        let _ = w("___");
        let _ = p("https://example.com/a?b#c 你好 `x_y`");
        let _ = c("/* unterminated", "rust");
        let _ = c("s = \"unclosed", "python");
    }
}
