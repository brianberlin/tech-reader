//! Sentence segmentation, ported from the TS `segment.ts`. Two uses:
//!  - [`split_sentences`]: batch-split a finished string into TTS-sized chunks.
//!  - [`SentenceStreamer`]: pull complete sentences out of a growing buffer as
//!    Ollama streams tokens, so speech can start before a block finishes.

use std::sync::LazyLock;

use regex::Regex;
use unicode_segmentation::UnicodeSegmentation;

/// Target characters per spoken chunk.
const MAX_LEN: usize = 260;

/// Collapse all whitespace runs to single spaces and trim.
fn squeeze(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Split finished text into sentence-ish chunks, combining short sentences and
/// hard-wrapping pathologically long ones so no single utterance is unwieldy.
/// `locale` is accepted for parity with the TS API; UAX#29 boundaries are
/// language-agnostic, so it is unused.
pub fn split_sentences(text: &str, _locale: &str) -> Vec<String> {
    let clean = squeeze(text);
    if clean.is_empty() {
        return Vec::new();
    }
    let raw: Vec<String> = clean
        .split_sentence_bounds()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    for s in raw {
        if char_len(&s) > MAX_LEN {
            if !buf.trim().is_empty() {
                out.push(buf.trim().to_string());
            }
            buf.clear();
            out.extend(split_long(&s));
            continue;
        }
        if buf.is_empty() {
            buf = s;
        } else if char_len(&buf) + 1 + char_len(&s) <= MAX_LEN {
            buf.push(' ');
            buf.push_str(&s);
        } else {
            out.push(buf.trim().to_string());
            buf = s;
        }
    }
    if !buf.trim().is_empty() {
        out.push(buf.trim().to_string());
    }
    out
}

/// Split a too-long sentence at clause punctuation (`, ; : — –`), recombining up
/// to `MAX_LEN`, then hard-wrapping anything still far too long.
fn split_long(s: &str) -> Vec<String> {
    let parts = split_after_clause_punct(s);
    let mut combined: Vec<String> = Vec::new();
    let mut buf = String::new();
    for p in parts {
        if buf.is_empty() {
            buf = p;
        } else if char_len(&buf) + 1 + char_len(&p) <= MAX_LEN {
            buf.push(' ');
            buf.push_str(&p);
        } else {
            combined.push(buf.trim().to_string());
            buf = p;
        }
    }
    if !buf.trim().is_empty() {
        combined.push(buf.trim().to_string());
    }

    let cap = (MAX_LEN as f64 * 1.5) as usize;
    combined
        .into_iter()
        .flat_map(|part| {
            if char_len(&part) <= cap {
                vec![part]
            } else {
                hard_wrap(&part, MAX_LEN)
            }
        })
        .collect()
}

/// Split on a whitespace run immediately preceded by `, ; : — –`, keeping the
/// punctuation attached to the left part (the TS lookbehind `(?<=[,;:—–])\s+`).
fn split_after_clause_punct(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut last_nonspace: Option<char> = None;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            if matches!(last_nonspace, Some(',') | Some(';') | Some(':') | Some('—') | Some('–')) {
                while i < chars.len() && chars[i].is_whitespace() {
                    i += 1;
                }
                parts.push(std::mem::take(&mut cur));
                last_nonspace = None;
                continue;
            }
            cur.push(c);
        } else {
            cur.push(c);
            last_nonspace = Some(c);
        }
        i += 1;
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

/// Greedy word wrap; a single token longer than `size` is hard-sliced.
fn hard_wrap(s: &str, size: usize) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut buf = String::new();
    for w in s.split_whitespace() {
        if char_len(w) > size {
            if !buf.is_empty() {
                out.push(std::mem::take(&mut buf));
            }
            let wchars: Vec<char> = w.chars().collect();
            let mut i = 0;
            while i < wchars.len() {
                let end = (i + size).min(wchars.len());
                out.push(wchars[i..end].iter().collect());
                i = end;
            }
            continue;
        }
        if !buf.is_empty() && char_len(&buf) + 1 + char_len(w) > size {
            out.push(std::mem::take(&mut buf));
            buf = w.to_string();
        } else if buf.is_empty() {
            buf = w.to_string();
        } else {
            buf.push(' ');
            buf.push_str(w);
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// A sentence is complete when a terminator (`. ! ?`, an optional closing
/// quote/bracket) or a hard line break is followed by whitespace.
static BOUNDARY: LazyLock<Regex> =
    // (?s) so `.` spans newlines (the TS `[\s\S]`).
    LazyLock::new(|| Regex::new(r#"(?s)^(.*?(?:[.!?]+['")\]]?|\n))(\s)"#).unwrap());

/// Pulls complete sentences from a streaming buffer. Tiny fragments (e.g.
/// "e.g.") are held back and merged with following text to avoid choppy speech.
pub struct SentenceStreamer {
    buf: String,
    locale: String,
}

impl SentenceStreamer {
    pub fn new(locale: &str) -> Self {
        Self {
            buf: String::new(),
            locale: locale.to_string(),
        }
    }

    /// Feed a streamed chunk; return any newly-completed sentences.
    pub fn push(&mut self, chunk: &str) -> Vec<String> {
        self.buf.push_str(chunk);
        self.pull()
    }

    /// Emit whatever remains as a final sentence (or two). Call once at block end.
    pub fn flush(&mut self) -> Vec<String> {
        let rest = std::mem::take(&mut self.buf);
        split_sentences(&rest, &self.locale)
    }

    fn pull(&mut self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        loop {
            let (whole_end, g1) = match BOUNDARY.captures(&self.buf) {
                Some(caps) => (
                    caps.get(0).unwrap().end(),
                    caps.get(1).unwrap().as_str().to_string(),
                ),
                None => break,
            };
            let candidate = squeeze(&g1);
            self.buf = self.buf[whole_end..].to_string();
            if candidate.is_empty() {
                continue;
            }
            // Hold back very short fragments (likely abbreviations) unless they
            // end a line or there is nothing buffered after them.
            let word_count = candidate.split_whitespace().count();
            if word_count < 2 && !g1.contains('\n') && !self.buf.is_empty() {
                self.buf = format!("{} {}", candidate, self.buf);
                break;
            }
            out.extend(split_sentences(&candidate, &self.locale));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_simple() {
        assert_eq!(split_sentences("Hello world.", "en"), vec!["Hello world."]);
    }

    #[test]
    fn combines_short_sentences() {
        // Two short sentences combine into one chunk (under MAX_LEN).
        assert_eq!(
            split_sentences("One. Two.", "en"),
            vec!["One. Two."]
        );
    }

    #[test]
    fn empty_in_empty_out() {
        assert!(split_sentences("   \n  ", "en").is_empty());
    }

    #[test]
    fn streamer_emits_on_boundary() {
        let mut s = SentenceStreamer::new("en");
        assert!(s.push("Hello ").is_empty());
        assert!(s.push("world").is_empty());
        assert_eq!(s.push(". Next one too. "), vec!["Hello world.", "Next one too."]);
    }

    #[test]
    fn streamer_holds_short_fragment() {
        let mut s = SentenceStreamer::new("en");
        // The full sentence emits; the trailing "e.g." fragment is held back
        // (merged with what follows) rather than spoken as its own sentence.
        assert_eq!(s.push("This works. e.g. more"), vec!["This works."]);
        assert_eq!(s.flush(), vec!["e.g. more"]);
    }

    #[test]
    fn hard_wraps_giant_token() {
        let giant = "x".repeat(700);
        let parts = split_sentences(&format!("{giant}."), "en");
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.chars().count() <= MAX_LEN));
    }
}
