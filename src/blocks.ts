// Segment a document (or selection) into speakable Blocks with accurate,
// absolute source line ranges. Pure string/line processing — no markdown
// library, no runtime dependencies. Two strategies are used depending on the
// document language: a lightweight line-based Markdown scan for prose, and a
// comment-aware chunker for code.

import { Block, BlockKind } from './types';

/** Languages that get the prose (Markdown-ish) treatment. */
const PROSE_LANGS = new Set<string>([
  'markdown',
  'mdx',
  'plaintext',
  'asciidoc',
  'restructuredtext',
  'rst',
  '', // unknown / untitled
]);

/** Rough chunk targets for the code path (in lines). */
const CODE_CHUNK_MAX = 50; // hard-ish cap: split at next blank line past this
const CODE_CHUNK_SOFT = 12; // min non-blank lines before a blank can split

/**
 * Split `source` into speakable blocks. `baseLine` is the 1-based document line
 * of the first line of `source`, so every returned Block carries ABSOLUTE
 * document line numbers. Never throws; returns [] for empty/whitespace input.
 */
export function segmentBlocks(source: string, lang: string, baseLine: number): Block[] {
  if (!source || source.trim().length === 0) {
    return [];
  }

  // Normalize line endings for splitting. Splitting on /\r\n?|\n/ keeps a CRLF
  // pair as a single logical line, so local line indices match the original.
  const lines = source.split(/\r\n?|\n/);

  const normalizedLang = (lang || '').toLowerCase();
  if (PROSE_LANGS.has(normalizedLang)) {
    return segmentProse(lines, lang, baseLine);
  }
  return segmentCode(lines, lang, baseLine);
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/** Make a Block from a local line range [startIdx, endIdx] (0-based, inclusive). */
function makeBlock(
  kind: BlockKind,
  src: string,
  startIdx: number,
  endIdx: number,
  baseLine: number,
  lang: string,
  headingLevel?: number,
): Block {
  const block: Block = {
    kind,
    source: src,
    startLine: baseLine + startIdx,
    endLine: baseLine + endIdx,
    lang,
  };
  if (headingLevel !== undefined) {
    block.headingLevel = headingLevel;
  }
  return block;
}

function isBlank(line: string): boolean {
  return line.trim().length === 0;
}

// ---------------------------------------------------------------------------
// (A) PROSE: lightweight line-based Markdown scan
// ---------------------------------------------------------------------------

const RE_ATX_HEADING = /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
const RE_FENCE = /^\s{0,3}(`{3,}|~{3,})(.*)$/;
const RE_BLOCKQUOTE = /^\s{0,3}>\s?(.*)$/;
const RE_LIST_ITEM = /^(\s*)([-*+]|\d{1,9}[.)])\s+(.*)$/;
const RE_THEMATIC_BREAK = /^\s{0,3}([-*_])(\s*\1){2,}\s*$/;
const RE_SETEXT_UNDERLINE = /^\s{0,3}(=+|-+)\s*$/;
const RE_HTML_COMMENT_FULL = /^\s*<!--.*-->\s*$/;
// Reference link definition: [label]: url "optional title"
const RE_REF_LINK = /^\s{0,3}\[[^\]]+\]:\s+\S+/;

/** A GFM table delimiter row, e.g. `| --- | :--: |` or `---|---`. */
function isTableDelimiter(line: string): boolean {
  const t = line.trim();
  if (!t.includes('-')) {
    return false;
  }
  const cells = t.replace(/^\|/, '').replace(/\|$/, '').split('|');
  if (cells.length < 1) {
    return false;
  }
  return cells.every((c) => /^\s*:?-{1,}:?\s*$/.test(c));
}

/** A line that could be a table row (contains a column separator). */
function looksLikeTableRow(line: string): boolean {
  return line.includes('|');
}

/** Scan prose lines into blocks. */
function segmentProse(lines: string[], lang: string, baseLine: number): Block[] {
  const blocks: Block[] = [];
  let i = 0;

  // Skip leading YAML (---) or TOML (+++) frontmatter — but only at the true
  // document top. A selection (baseLine > 1) starting with a `---` rule must not
  // have its content swallowed as "frontmatter".
  i = baseLine === 1 ? skipFrontmatter(lines) : 0;

  while (i < lines.length) {
    const line = lines[i];

    // Blank lines are separators; nothing to emit.
    if (isBlank(line)) {
      i++;
      continue;
    }

    // Fenced code block.
    const fence = matchOpeningFence(line);
    if (fence) {
      i = consumeFence(lines, i, fence, baseLine, lang, blocks);
      continue;
    }

    // ATX heading.
    const atx = RE_ATX_HEADING.exec(line);
    if (atx) {
      const level = atx[1].length;
      blocks.push(makeBlock('heading', atx[2].trim(), i, i, baseLine, lang, level));
      i++;
      continue;
    }

    // Thematic break / horizontal rule → skip (must check before setext so a
    // standalone `---` with no text above is treated as a rule, not setext).
    if (RE_THEMATIC_BREAK.test(line)) {
      i++;
      continue;
    }

    // HTML comment line → skip.
    if (RE_HTML_COMMENT_FULL.test(line)) {
      i++;
      continue;
    }

    // Reference-link definition → skip.
    if (RE_REF_LINK.test(line)) {
      i++;
      continue;
    }

    // GFM table: a header row immediately followed by a delimiter row.
    if (looksLikeTableRow(line) && i + 1 < lines.length && isTableDelimiter(lines[i + 1])) {
      i = consumeTable(lines, i, baseLine, lang, blocks);
      continue;
    }

    // Blockquote (run of consecutive `>` lines).
    if (RE_BLOCKQUOTE.test(line)) {
      i = consumeBlockquote(lines, i, baseLine, lang, blocks);
      continue;
    }

    // List item (possibly with wrapped continuation lines).
    if (RE_LIST_ITEM.test(line)) {
      i = consumeListItem(lines, i, baseLine, lang, blocks);
      continue;
    }

    // Otherwise a paragraph — but a paragraph whose 2nd line is a setext
    // underline is actually a setext heading.
    i = consumeParagraphOrSetext(lines, i, baseLine, lang, blocks);
  }

  return blocks;
}

/** Return the index of the first content line after any top frontmatter fence. */
function skipFrontmatter(lines: string[]): number {
  if (lines.length === 0) {
    return 0;
  }
  const first = lines[0].trim();
  const marker = first === '---' ? '---' : first === '+++' ? '+++' : null;
  if (!marker) {
    return 0;
  }
  // Find the closing fence on a later line.
  for (let j = 1; j < lines.length; j++) {
    if (lines[j].trim() === marker) {
      return j + 1;
    }
  }
  // No closing fence: not real frontmatter, don't skip anything.
  return 0;
}

interface FenceInfo {
  marker: string; // the run of ` or ~ that opened the fence
}

/** If `line` opens a fenced code block, return its fence marker. */
function matchOpeningFence(line: string): FenceInfo | null {
  const m = RE_FENCE.exec(line);
  if (!m) {
    return null;
  }
  return { marker: m[1] };
}

/**
 * Consume a fenced code block starting at the opening fence on line `start`.
 * Emits a 'code' block for the inner lines (fences excluded) and returns the
 * index just past the closing fence (or end of input).
 */
function consumeFence(
  lines: string[],
  start: number,
  fence: FenceInfo,
  baseLine: number,
  lang: string,
  blocks: Block[],
): number {
  const fenceChar = fence.marker[0];
  const fenceLen = fence.marker.length;
  let j = start + 1;
  const innerStart = j;

  // Find a closing fence: same char, at least as long, nothing else but spaces.
  while (j < lines.length) {
    const m = RE_FENCE.exec(lines[j]);
    if (m && m[1][0] === fenceChar && m[1].length >= fenceLen && m[2].trim() === '') {
      break;
    }
    j++;
  }

  const innerEnd = j - 1; // last inner line index (may be < innerStart if empty)
  if (innerEnd >= innerStart) {
    const src = lines.slice(innerStart, innerEnd + 1).join('\n');
    blocks.push(makeBlock('code', src, innerStart, innerEnd, baseLine, lang));
  }

  // Skip past the closing fence if present.
  return j < lines.length ? j + 1 : j;
}

/**
 * Consume a GFM table (header + delimiter + consecutive row lines) into one
 * 'table' block. The raw markdown is kept so the narrator can hand the whole
 * table to the model to summarize.
 */
function consumeTable(
  lines: string[],
  start: number,
  baseLine: number,
  lang: string,
  blocks: Block[],
): number {
  let j = start;
  while (j < lines.length && !isBlank(lines[j]) && looksLikeTableRow(lines[j])) {
    j++;
  }
  const src = lines.slice(start, j).join('\n');
  blocks.push(makeBlock('table', src, start, j - 1, baseLine, lang));
  return j;
}

/** Consume a run of `>` blockquote lines into one 'quote' block. */
function consumeBlockquote(
  lines: string[],
  start: number,
  baseLine: number,
  lang: string,
  blocks: Block[],
): number {
  let j = start;
  const texts: string[] = [];
  while (j < lines.length) {
    const m = RE_BLOCKQUOTE.exec(lines[j]);
    if (!m) {
      break;
    }
    texts.push(m[1]);
    j++;
  }
  const src = texts.join('\n').trim();
  blocks.push(makeBlock('quote', src, start, j - 1, baseLine, lang));
  return j;
}

/**
 * Consume a single list item plus any wrapped continuation lines (non-blank,
 * non-marker lines that follow it) into one 'listItem' block.
 */
function consumeListItem(
  lines: string[],
  start: number,
  baseLine: number,
  lang: string,
  blocks: Block[],
): number {
  const first = RE_LIST_ITEM.exec(lines[start]);
  // first is guaranteed non-null by the caller's guard.
  const parts: string[] = [first ? first[3] : lines[start]];
  let j = start + 1;

  // Continuation lines: non-blank lines that are not themselves a new list
  // item, heading, fence, blockquote, or thematic break.
  while (j < lines.length) {
    const l = lines[j];
    if (isBlank(l)) {
      break;
    }
    if (
      RE_LIST_ITEM.test(l) ||
      RE_ATX_HEADING.test(l) ||
      RE_BLOCKQUOTE.test(l) ||
      matchOpeningFence(l) ||
      RE_THEMATIC_BREAK.test(l)
    ) {
      break;
    }
    parts.push(l.trim());
    j++;
  }

  const src = parts.join('\n').trim();
  blocks.push(makeBlock('listItem', src, start, j - 1, baseLine, lang));
  return j;
}

/**
 * Consume a paragraph starting at `start`. If the paragraph is exactly one text
 * line followed by a setext underline (=== or ---), emit a heading instead.
 * Returns the index after the consumed lines.
 */
function consumeParagraphOrSetext(
  lines: string[],
  start: number,
  baseLine: number,
  lang: string,
  blocks: Block[],
): number {
  // Setext heading: a single non-blank text line immediately followed by an
  // underline of only `=` (level 1) or `-` (level 2). The text line here is
  // non-blank (caller guaranteed) and is not a thematic break (already handled).
  const next = start + 1;
  if (next < lines.length && RE_SETEXT_UNDERLINE.test(lines[next]) && !isBlank(lines[start])) {
    const underline = lines[next].trim();
    const level = underline[0] === '=' ? 1 : 2;
    blocks.push(makeBlock('heading', lines[start].trim(), start, next, baseLine, lang, level));
    return next + 1;
  }

  // Otherwise gather consecutive non-blank lines that don't start a new block
  // construct into a paragraph.
  let j = start;
  const parts: string[] = [];
  while (j < lines.length) {
    const l = lines[j];
    if (isBlank(l)) {
      break;
    }
    if (j > start) {
      // A later line that begins a different construct ends the paragraph.
      if (
        RE_ATX_HEADING.test(l) ||
        matchOpeningFence(l) ||
        RE_BLOCKQUOTE.test(l) ||
        RE_LIST_ITEM.test(l) ||
        RE_THEMATIC_BREAK.test(l) ||
        RE_SETEXT_UNDERLINE.test(l)
      ) {
        break;
      }
    }
    parts.push(l.trim());
    j++;
  }

  const src = parts.join('\n').trim();
  if (src.length > 0) {
    blocks.push(makeBlock('para', src, start, j - 1, baseLine, lang));
  }
  return j > start ? j : start + 1;
}

// ---------------------------------------------------------------------------
// (B) CODE: comment-aware chunking
// ---------------------------------------------------------------------------

interface CommentSyntax {
  line: string[]; // line-comment prefixes, e.g. ['//']
  blockOpen?: string; // e.g. '/*'
  blockClose?: string; // e.g. '*/'
  docstring?: string; // e.g. '"""' or "'''" (toggled fence)
}

/** Resolve comment syntax for a language id. Falls back to // and #. */
function commentSyntaxFor(lang: string): CommentSyntax {
  const l = (lang || '').toLowerCase();

  const cLike = new Set([
    'typescript',
    'typescriptreact',
    'javascript',
    'javascriptreact',
    'tsx',
    'jsx',
    'java',
    'c',
    'cpp',
    'c++',
    'objective-c',
    'objective-cpp',
    'csharp',
    'cs',
    'go',
    'rust',
    'swift',
    'kotlin',
    'scala',
    'php',
    'dart',
  ]);
  const hashLike = new Set([
    'python',
    'ruby',
    'shellscript',
    'shell',
    'sh',
    'bash',
    'zsh',
    'yaml',
    'toml',
    'perl',
    'r',
    'makefile',
    'dockerfile',
  ]);
  const dashLike = new Set(['sql', 'lua', 'haskell']);

  if (cLike.has(l)) {
    return { line: ['//'], blockOpen: '/*', blockClose: '*/' };
  }
  if (l === 'python') {
    return { line: ['#'], docstring: '"""' };
  }
  if (hashLike.has(l)) {
    return { line: ['#'] };
  }
  if (dashLike.has(l)) {
    return { line: ['--'] };
  }
  // Reasonable default: treat both // and # as line comments.
  return { line: ['//', '#'] };
}

/** Is the (trimmed) line a line-comment for this syntax? */
function isLineComment(line: string, syntax: CommentSyntax): boolean {
  const t = line.trimStart();
  return syntax.line.some((p) => t.startsWith(p));
}

/**
 * Chunk code lines into 'code'/'comment' blocks. Leading comments stay attached
 * to the code that follows them; chunks split on blank lines once they're large
 * enough, and never mid-block-comment / mid-docstring.
 */
function segmentCode(lines: string[], lang: string, baseLine: number): Block[] {
  const syntax = commentSyntaxFor(lang);
  const blocks: Block[] = [];

  // Precompute, for each line, whether it sits inside a multi-line block comment
  // or docstring. We only allow chunk boundaries on lines that are NOT inside
  // such a span (so we never split a /* ... */ or """ ... """).
  const insideSpan = computeMultilineSpans(lines, syntax);

  let chunkStart = 0; // local index where the current chunk began
  let nonBlankInChunk = 0;

  const flush = (endIdx: number): void => {
    // endIdx is the local index of the last line in the chunk (inclusive).
    emitChunk(lines, chunkStart, endIdx, baseLine, lang, syntax, blocks);
  };

  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    const blank = isBlank(line);
    if (!blank) {
      nonBlankInChunk++;
    }

    // Only consider a boundary at a blank line that is not inside a multi-line
    // span, and only if the current chunk has real content.
    const atSafeBlank = blank && !insideSpan[i] && nonBlankInChunk > 0;
    if (!atSafeBlank) {
      continue;
    }

    const chunkLen = i - chunkStart; // includes this blank line
    const big = chunkLen >= CODE_CHUNK_MAX;
    const softSplit = nonBlankInChunk >= CODE_CHUNK_SOFT;

    // Don't split if the lines immediately after the blank are a leading comment
    // for upcoming code AND we're only at the soft threshold — keep comment
    // attached to its code. The hard cap (`big`) always splits.
    if (big || (softSplit && !blankStartsTrailingComment(lines, i, syntax))) {
      flush(i - 1); // exclude the trailing blank line from the chunk
      chunkStart = i + 1; // next chunk starts after the blank
      nonBlankInChunk = 0;
    }
  }

  // Final chunk (if any content remains).
  if (chunkStart < lines.length) {
    flush(lines.length - 1);
  }

  return blocks;
}

/**
 * After a blank line at index `i`, do the following non-blank lines begin with a
 * comment? If so, splitting here would separate that comment from the code it
 * documents, so we prefer to keep going (at the soft threshold only).
 */
function blankStartsTrailingComment(lines: string[], i: number, syntax: CommentSyntax): boolean {
  let j = i + 1;
  while (j < lines.length && isBlank(lines[j])) {
    j++;
  }
  if (j >= lines.length) {
    return false;
  }
  return isLineComment(lines[j], syntax) || startsBlockComment(lines[j], syntax);
}

function startsBlockComment(line: string, syntax: CommentSyntax): boolean {
  const t = line.trimStart();
  if (syntax.blockOpen && t.startsWith(syntax.blockOpen)) {
    return true;
  }
  if (syntax.docstring && t.startsWith(syntax.docstring)) {
    return true;
  }
  return false;
}

/**
 * Mark which lines lie inside a multi-line block comment (/* ... *\/) or a
 * docstring (""" ... """). Boundaries for chunking are forbidden on these lines.
 * Single-line forms (an open and close on the same line) don't mark anything.
 */
function computeMultilineSpans(lines: string[], syntax: CommentSyntax): boolean[] {
  const inside = new Array<boolean>(lines.length).fill(false);

  // Block comments: scan char-free, line-granular. Good enough for chunking.
  if (syntax.blockOpen && syntax.blockClose) {
    let open = false;
    for (let i = 0; i < lines.length; i++) {
      const t = lines[i];
      if (!open) {
        const oIdx = t.indexOf(syntax.blockOpen);
        if (oIdx >= 0) {
          const cIdx = t.indexOf(syntax.blockClose, oIdx + syntax.blockOpen.length);
          if (cIdx < 0) {
            // Opens here, doesn't close on this line.
            open = true;
            // The opening line itself isn't "interior"; interior begins next line.
          }
        }
      } else {
        inside[i] = true;
        if (t.indexOf(syntax.blockClose) >= 0) {
          open = false;
        }
      }
    }
  }

  // Docstrings: a line containing an odd number of the docstring marker toggles.
  if (syntax.docstring) {
    const marker = syntax.docstring;
    let open = false;
    for (let i = 0; i < lines.length; i++) {
      const count = occurrences(lines[i], marker);
      if (open) {
        inside[i] = true;
      }
      if (count % 2 === 1) {
        open = !open;
      }
    }
  }

  return inside;
}

function occurrences(haystack: string, needle: string): number {
  if (needle.length === 0) {
    return 0;
  }
  let count = 0;
  let idx = haystack.indexOf(needle);
  while (idx >= 0) {
    count++;
    idx = haystack.indexOf(needle, idx + needle.length);
  }
  return count;
}

/**
 * Emit one chunk (local lines [start, end] inclusive), trimming leading/trailing
 * blank lines but preserving interior formatting. Empty chunks are skipped. The
 * block kind is 'comment' if every non-blank line is a comment, else 'code'.
 */
function emitChunk(
  lines: string[],
  start: number,
  end: number,
  baseLine: number,
  lang: string,
  syntax: CommentSyntax,
  blocks: Block[],
): void {
  // Trim leading blank lines.
  let s = start;
  while (s <= end && isBlank(lines[s])) {
    s++;
  }
  // Trim trailing blank lines.
  let e = end;
  while (e >= s && isBlank(lines[e])) {
    e--;
  }
  if (s > e) {
    return; // entirely blank
  }

  const slice = lines.slice(s, e + 1);
  const src = slice.join('\n');
  const kind: BlockKind = isAllComment(slice, syntax) ? 'comment' : 'code';
  blocks.push(makeBlock(kind, src, s, e, baseLine, lang));
}

/**
 * True when every non-blank line in the chunk is a comment — line comments,
 * lines inside a block comment / docstring, or the comment delimiters
 * themselves. Conservative: anything that doesn't read as comment text makes the
 * chunk 'code'.
 */
function isAllComment(slice: string[], syntax: CommentSyntax): boolean {
  let inBlock = false;
  let inDoc = false;

  for (const raw of slice) {
    if (isBlank(raw)) {
      continue;
    }
    const t = raw.trim();

    // Inside a multi-line block comment.
    if (inBlock) {
      if (syntax.blockClose && t.includes(syntax.blockClose)) {
        inBlock = false;
        // If there's meaningful code after the close on the same line, treat as code.
        const after = t.slice(t.indexOf(syntax.blockClose) + syntax.blockClose.length).trim();
        if (after.length > 0) {
          return false;
        }
      }
      continue;
    }
    // Inside a docstring.
    if (inDoc) {
      if (syntax.docstring && occurrences(t, syntax.docstring) % 2 === 1) {
        inDoc = false;
      }
      continue;
    }

    // Line comment.
    if (isLineComment(t, syntax)) {
      continue;
    }

    // Opening a block comment on this line.
    if (syntax.blockOpen && syntax.blockClose && t.startsWith(syntax.blockOpen)) {
      const closeIdx = t.indexOf(syntax.blockClose, syntax.blockOpen.length);
      if (closeIdx < 0) {
        inBlock = true;
        continue;
      }
      const after = t.slice(closeIdx + syntax.blockClose.length).trim();
      if (after.length === 0) {
        continue; // whole line is a single-line block comment
      }
      return false;
    }

    // Opening (or one-line) docstring.
    if (syntax.docstring && t.startsWith(syntax.docstring)) {
      if (occurrences(t, syntax.docstring) % 2 === 1) {
        inDoc = true;
      }
      continue;
    }

    // Anything else is real code.
    return false;
  }

  return true;
}
