//! Shared types. A document is segmented into typed [`Block`]s; each block is
//! narrated (AI via Ollama, or the offline humanizer) into [`Sentence`]s that
//! the audio spine speaks.

/// The kind of a speakable block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    Heading,
    Para,
    ListItem,
    Quote,
    Code,
    Comment,
    Table,
}

/// A speakable unit of the source document, with its 1-based source line range.
#[derive(Debug, Clone)]
pub struct Block {
    pub kind: BlockKind,
    /// Raw source text of the block (code or prose).
    pub source: String,
    /// 1-based, absolute line where the block starts.
    pub start_line: usize,
    /// 1-based, absolute line where the block ends.
    pub end_line: usize,
    /// Heading depth 1..=6 (`kind == Heading` only).
    pub heading_level: Option<u8>,
    /// Language id of the document, e.g. "typescript", "markdown".
    pub lang: String,
}

/// One narration sentence — the spoken (and, later, displayed) text plus a
/// best-effort source mapping back to the block it came from.
#[derive(Debug, Clone)]
pub struct Sentence {
    pub idx: usize,
    /// The already-humanized / explained text to speak and show.
    pub text: String,
    pub kind: BlockKind,
    pub start_line: usize,
    pub end_line: usize,
    pub block_index: usize,
}
