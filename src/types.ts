// Shared types for tech-reader. The extension host segments a document into
// speakable Blocks, narrates each one (AI via local Ollama, or the deterministic
// humanizer), and streams the resulting Sentences to the reader webview, which
// speaks them with the OS Web Speech API.

export type Mode = 'ai' | 'literal';

export type BlockKind = 'heading' | 'para' | 'listItem' | 'quote' | 'code' | 'comment' | 'table';

/** A speakable unit of the source document, with its source line range. */
export interface Block {
  kind: BlockKind;
  /** raw source text of the block (code or prose) */
  source: string;
  /** 1-based, absolute line in the document where the block starts */
  startLine: number;
  /** 1-based, absolute line where the block ends */
  endLine: number;
  /** heading depth 1..6 (kind === 'heading' only) */
  headingLevel?: number;
  /** vscode languageId of the document, e.g. "typescript", "markdown" */
  lang: string;
}

/** One narration sentence streamed to the webview. */
export interface Sentence {
  idx: number;
  /** the spoken AND displayed text (already humanized / explained) */
  text: string;
  kind: BlockKind;
  /** best-effort source mapping for "jump to source" */
  startLine: number;
  endLine: number;
  blockIndex: number;
}

export interface NarrationSettings {
  /** how to treat source code blocks */
  codeHandling: 'explain' | 'literal' | 'skip';
  /** how to treat tables: distill the takeaway, read the cells, or skip */
  tables: 'summarize' | 'read' | 'skip';
  /** prepend "Heading." before heading blocks */
  announceHeadings: boolean;
}

export type StatusState =
  | 'thinking' // contacting Ollama / waiting for first token
  | 'streaming' // narration is being produced
  | 'fallback' // Ollama unavailable → using the offline humanizer
  | 'done' // narration complete
  | 'empty' // nothing readable
  | 'error'; // unrecoverable problem
