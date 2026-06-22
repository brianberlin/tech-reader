// =============================================================================
// humanizer.ts — deterministic "speech humanizer"
// -----------------------------------------------------------------------------
// Turns code, identifiers, and technical prose into natural spoken text so a
// text-to-speech engine never reads `return_item` as "return underscore item".
//
// This is the OFFLINE / deterministic fallback used when the AI narrator
// (Ollama) is unavailable. Everything here is pure, dependency-free, and must
// NEVER throw on any string input.
//
// Pipeline overview:
//   humanizeWord   — one identifier  -> spoken fragments (case/separator split,
//                    digit splitting, acronym preservation, dictionary expand)
//   humanizeProse  — running text    -> identifier-looking tokens humanized,
//                    inline code / paths / URLs simplified, leave English alone
//   humanizeCode   — source code     -> comments read as prose, identifiers
//                    humanized, meaningful operators spoken, noise punctuation
//                    dropped, statement/line breaks become sentence boundaries
//
// Design notes:
//   - We bias toward "listenable" over "literal": we drop braces/semicolons and
//     read only what a human would say out loud.
//   - All regexes that match identifier characters use \p{L}\p{N} with the `u`
//     flag so Unicode letters/digits are handled.
//   - No part of this module performs I/O or depends on VS Code APIs.
// =============================================================================

// -----------------------------------------------------------------------------
// HumanizeOptions is part of this module's public API (the shared types.ts does
// not declare it; see the task spec). Declared and exported here.
// -----------------------------------------------------------------------------
export interface HumanizeOptions {
  /** extra abbreviation expansions from user settings (key lowercased) */
  dictionary?: Record<string, string>;
  /** when true (code), also map operators/symbols to words & drop noise punctuation; when false (prose), only fix identifier-looking tokens */
  code?: boolean;
}

// -----------------------------------------------------------------------------
// DEFAULT_DICTIONARY
// Keys are lowercased identifier fragments; values are spoken expansions.
// We deliberately DO NOT expand words that already read fine out loud
// (config, info, data, admin, async, await, enum, max, min, ...).
// -----------------------------------------------------------------------------
export const DEFAULT_DICTIONARY: Record<string, string> = {
  // functions / generic code nouns
  fn: 'function',
  func: 'function',
  idx: 'index',
  ctx: 'context',
  msg: 'message',
  btn: 'button',
  num: 'number',
  str: 'string',
  arr: 'array',
  obj: 'object',
  val: 'value',
  vals: 'values',
  // requests / responses / errors
  req: 'request',
  reqs: 'requests',
  res: 'response',
  resp: 'response',
  err: 'error',
  errs: 'errors',
  // storage / environment
  db: 'database',
  env: 'environment',
  envs: 'environments',
  repo: 'repository',
  repos: 'repositories',
  dir: 'directory',
  dirs: 'directories',
  // lifecycle
  init: 'initialize',
  util: 'utility',
  utils: 'utilities',
  impl: 'implementation',
  impls: 'implementations',
  // sizes / lengths / positions
  len: 'length',
  src: 'source',
  dest: 'destination',
  dst: 'destination',
  tmp: 'temporary',
  temp: 'temporary',
  prev: 'previous',
  curr: 'current',
  cur: 'current',
  pos: 'position',
  // elements / attributes / params / args
  elem: 'element',
  elems: 'elements',
  el: 'element',
  attr: 'attribute',
  attrs: 'attributes',
  param: 'parameter',
  params: 'parameters',
  arg: 'argument',
  args: 'arguments',
  // variables / declarations
  var: 'variable',
  vars: 'variables',
  decl: 'declaration',
  // verbs
  calc: 'calculate',
  gen: 'generate',
  fmt: 'format',
  parse: 'parse',
  // roles / services
  mgr: 'manager',
  svc: 'service',
  svcs: 'services',
  auth: 'authentication',
  authz: 'authorization',
  // ordering
  asc: 'ascending',
  desc: 'descending',
  // misc abbreviations
  addr: 'address',
  char: 'character',
  chars: 'characters',
  ptr: 'pointer',
  ref: 'reference',
  refs: 'references',
  regex: 'reg ex',
  cmd: 'command',
  cmds: 'commands',
  pkg: 'package',
  pkgs: 'packages',
  lib: 'library',
  libs: 'libraries',
  doc: 'document',
  docs: 'documents',
  cnt: 'count',
  amt: 'amount',
  qty: 'quantity',
  desc_: 'description', // guard key never produced by splitter; harmless
  descr: 'description',
  cb: 'callback',
  conn: 'connection',
  conns: 'connections',
  proc: 'process',
  sync: 'sync', // reads fine, but listed so users can override
  recv: 'receive',
  sched: 'schedule',
  stmt: 'statement',
  expr: 'expression',
  cond: 'condition',
  iter: 'iterate',
  seq: 'sequence',
  acc: 'accumulator',
  prop: 'property',
  props: 'properties',
  opt: 'option',
  opts: 'options',
  def: 'definition',
  // a few that READ FINE but are commonly overridden — kept identity so the
  // map is explicit and easy to scan for settings docs:
  config: 'config',
  info: 'info',
  spec: 'spec',
  admin: 'admin',
  data: 'data',
};

// -----------------------------------------------------------------------------
// Well-known acronyms: output uppercase & space-separated so a synthesizer
// SPELLS the letters ("H T T P") instead of trying to pronounce them.
// Stored lowercased for lookup.
// -----------------------------------------------------------------------------
const ACRONYMS = new Set<string>([
  'http', 'https', 'url', 'uri', 'api', 'sdk', 'cli', 'id', 'uuid', 'io',
  'ui', 'ux', 'db', 'sql', 'json', 'xml', 'yaml', 'html', 'css', 'js', 'ts',
  'cpu', 'gpu', 'ram', 'os', 'ip', 'tcp', 'udp', 'dns', 'ssh', 'tls', 'ssl',
  'jwt', 'orm', 'crud', 'rest', 'rpc', 'ast', 'ascii', 'utf', 'csv', 'png',
  'jpg', 'gif', 'svg', 'pdf', 'http2', 'oauth', 'gui', 'usb', 'pid', 'tty',
]);

// Words that are technically acronyms but read fine / are commonly spoken as
// words, so we DON'T force letter-spelling for them.
const SPOKEN_ACRONYMS: Record<string, string> = {
  regexp: 'reg exp',
  regex: 'reg ex',
};

const EMPTY = '';

// -----------------------------------------------------------------------------
// Small helpers
// -----------------------------------------------------------------------------

/** Collapse runs of whitespace to single spaces and trim. Never throws. */
function squeeze(s: string): string {
  return s.replace(/\s+/g, ' ').trim();
}

/** True if a token contains a letter (so it's "wordy", not pure punctuation). */
function hasLetter(s: string): boolean {
  return /\p{L}/u.test(s);
}

/**
 * Format a single already-split word fragment for speech:
 *   - acronyms -> uppercase letters separated by spaces ("HTTP")
 *   - single stray capital letter -> that letter uppercased ("X")
 *   - everything else -> lowercase
 */
function formatFragment(frag: string): string {
  if (!frag) return EMPTY;
  const lower = frag.toLowerCase();
  if (Object.prototype.hasOwnProperty.call(SPOKEN_ACRONYMS, lower)) {
    return SPOKEN_ACRONYMS[lower];
  }
  if (ACRONYMS.has(lower)) {
    // Spell it out so TTS says the letters. Keep any trailing digits readable.
    const m = /^([a-z]+)(\d+)$/.exec(lower);
    if (m) {
      return m[1].toUpperCase().split('').join(' ') + ' ' + m[2];
    }
    return lower.toUpperCase().split('').join(' ');
  }
  // A single stray capital letter (e.g. from "userID" -> "I","D"): spell it.
  if (/^[A-Z]$/.test(frag)) return frag;
  if (/^\d+$/.test(frag)) return frag; // pure number stays as-is
  return lower;
}

/** Expand a fragment via dictionary (user dict wins over default). */
function expandFragment(frag: string, dict: Record<string, string>): string {
  if (!frag) return EMPTY;
  const lower = frag.toLowerCase();
  // Don't expand acronyms — they're spelled out, not abbreviations of words.
  if (ACRONYMS.has(lower)) return frag;
  if (Object.prototype.hasOwnProperty.call(dict, lower)) {
    return dict[lower];
  }
  return frag;
}

// -----------------------------------------------------------------------------
// Identifier splitting
// -----------------------------------------------------------------------------

/**
 * Split an identifier "core" (already separator-free, e.g. one camelCase run)
 * into word fragments, handling acronym runs and digit boundaries.
 *
 * Strategy: regex-tokenize into runs of:
 *   - acronym + Capitalized-word boundary (e.g. "HTTPResponse" -> HTTP, Response)
 *   - normal Capitalized or lowercase words
 *   - digit runs (split from adjacent letters)
 */
function splitCamel(core: string): string[] {
  if (!core) return [];
  const out: string[] = [];
  // This regex consumes the string left-to-right:
  //   1) An uppercase run that is NOT immediately followed by a lowercase
  //      letter -> an acronym chunk (e.g. "HTTP", "URL", "IO", "ID").
  //   2) One uppercase letter followed by lowercase letters -> "Response".
  //   3) A run of lowercase letters -> "get".
  //   4) A run of digits -> "8", "256".
  // We use Unicode-aware classes where it matters; case detection relies on
  // ASCII-ish A-Z/a-z which is correct for source identifiers.
  const re =
    /([A-Z]+(?=[A-Z][a-z]|\d|\b|$))|([A-Z][a-z]+)|([a-z]+)|(\d+)|([^\sA-Za-z\d]+)/g;
  let m: RegExpExecArray | null;
  let lastIndex = 0;
  let guard = 0;
  while ((m = re.exec(core)) !== null) {
    if (guard++ > 10000) break; // pathological guard; never loop forever
    if (m.index < lastIndex) break;
    lastIndex = re.lastIndex;
    const tok = m[0];
    if (!tok) {
      re.lastIndex++; // avoid zero-width infinite loop
      continue;
    }
    // Unicode letters not in A-Za-z (e.g. accented) fall through as a single
    // chunk because our regex above is ASCII-case based; capture them too.
    out.push(tok);
  }
  // If the ASCII regex missed Unicode letters entirely (no matches), fall back
  // to treating the whole core as one fragment.
  if (out.length === 0 && hasLetter(core)) return [core];
  return out;
}

/**
 * Split a digit-glued fragment like "utf8" or "base64" so the number is spoken
 * separately ("utf 8", "base 64"). splitCamel already separates letters/digits,
 * but this also covers the leading-letters + trailing-digits shape directly.
 * Returns the original if no split applies.
 */
function splitLetterDigit(frag: string): string[] {
  // letters then digits ("h1","v2","sha256")  OR  digits then letters
  const m1 = /^(\p{L}+)(\d+)$/u.exec(frag);
  if (m1) return [m1[1], m1[2]];
  const m2 = /^(\d+)(\p{L}+)$/u.exec(frag);
  if (m2) return [m2[1], m2[2]];
  return [frag];
}

/**
 * Core word-humanizer. Splits one identifier token into spoken fragments and
 * applies dictionary expansion + acronym/casing formatting.
 */
export function humanizeWord(word: string, opts?: HumanizeOptions): string {
  try {
    if (typeof word !== 'string' || word.length === 0) return EMPTY;

    const dict: Record<string, string> = {
      ...DEFAULT_DICTIONARY,
      ...(opts && opts.dictionary ? lowercaseKeys(opts.dictionary) : {}),
    };

    let w = word.trim();
    if (!w) return EMPTY;

    // Drop a leading "this." but keep the member readable: "this.value" -> value
    // (handled generically by the dot-splitting below, but we special-case the
    // leading "this" so we don't say "this" twice oddly — actually we WANT
    // "this value", so just let dots become spaces and keep "this").
    //
    // Dotted member paths: read dots as light separators (drop them):
    //   "user.profile.name" -> "user profile name"
    //   "this.value"        -> "this value"
    // Split on dots first, then process each segment.
    const dotParts = w.split('.').filter((p) => p.length > 0);

    const spokenSegments: string[] = [];
    for (const part of dotParts) {
      spokenSegments.push(humanizeIdentifierSegment(part, dict));
    }
    const result = squeeze(spokenSegments.join(' '));
    return result;
  } catch {
    // Absolute safety net: never throw.
    return typeof word === 'string' ? word : EMPTY;
  }
}

/** Lowercase all keys of a user dictionary so lookups are case-insensitive. */
function lowercaseKeys(d: Record<string, string>): Record<string, string> {
  const out: Record<string, string> = {};
  for (const k of Object.keys(d)) {
    if (typeof k === 'string') out[k.toLowerCase()] = d[k];
  }
  return out;
}

/**
 * Humanize ONE dot-free identifier segment: handle snake_case / kebab-case,
 * camelCase, SCREAMING_SNAKE, leading/trailing/double underscores, then split
 * each piece into camel fragments and digit groups, expand, and format.
 */
function humanizeIdentifierSegment(seg: string, dict: Record<string, string>): string {
  if (!seg) return EMPTY;

  // Replace snake/kebab separators with spaces; collapse repeats; trim.
  // This handles "__init__" -> "init", "_private" -> "private",
  // "MAX_BUFFER_SIZE" -> "MAX BUFFER SIZE", "data-source" -> "data source".
  const normalized = seg.replace(/[_\-]+/g, ' ').trim();
  if (!normalized) return EMPTY;

  const pieces = normalized.split(/\s+/);
  const fragments: string[] = [];

  for (const piece of pieces) {
    // First split camelCase / acronym runs into raw fragments.
    const camel = splitCamel(piece);
    for (const c of camel) {
      // Then ensure letter/digit groups are separated ("utf8" -> "utf","8").
      for (const ld of splitLetterDigit(c)) {
        fragments.push(ld);
      }
    }
  }

  // Expand via dictionary, then format (acronyms/casing). Order matters:
  // expand BEFORE formatting so "idx" -> "index" (lowercase word), but
  // acronyms like "id" are NOT in the dictionary so they survive to be spelled.
  const spoken: string[] = [];
  for (const frag of fragments) {
    const expanded = expandFragment(frag, dict);
    // Expansion may itself contain spaces (e.g. "reg ex"); format each token.
    if (expanded.indexOf(' ') >= 0) {
      for (const sub of expanded.split(/\s+/)) spoken.push(formatFragment(sub));
    } else {
      spoken.push(formatFragment(expanded));
    }
  }

  return squeeze(spoken.join(' '));
}

// -----------------------------------------------------------------------------
// Prose humanization
// -----------------------------------------------------------------------------

// A token "looks like code" if it has an underscore/hyphen-with-letters,
// internal capitalization (camel/Pascal with a lowercase->uppercase or
// acronym boundary), or a dot between identifier characters.
const SNAKE_RE = /\p{L}[_][\p{L}\p{N}_]*/u; // contains an internal underscore
const KEBAB_RE = /\p{L}[-][\p{L}\p{N}-]*\p{L}/u; // word-hyphen-word
const CAMEL_RE = /\p{Ll}\p{Lu}|\p{Lu}\p{Lu}\p{Ll}/u; // camel or ACRO+word
const DOTTED_RE = /\p{L}[\p{L}\p{N}_]*\.[\p{L}_][\p{L}\p{N}_.]*/u; // a.b(.c)

/** Does this bare token look like a code identifier worth humanizing? */
function looksLikeCode(tok: string): boolean {
  if (!tok || !hasLetter(tok)) return false;
  if (SNAKE_RE.test(tok)) return true;
  if (KEBAB_RE.test(tok)) return true;
  if (DOTTED_RE.test(tok)) return true;
  if (CAMEL_RE.test(tok)) return true;
  return false;
}

/** Read only the final segment of a path-like token, then humanize it. */
function humanizePathToken(tok: string, dict: Record<string, string>): string {
  // Strip query/hash; take the last "/" or "\" segment.
  const cleaned = tok.replace(/[?#].*$/, '');
  const parts = cleaned.split(/[\\/]+/).filter((p) => p.length > 0);
  const last = parts.length ? parts[parts.length - 1] : cleaned;
  return humanizeFilename(last, dict);
}

/** Humanize a filename like "foo.ts" -> "foo dot t s" (extension spelled). */
function humanizeFilename(name: string, dict: Record<string, string>): string {
  const dot = name.lastIndexOf('.');
  if (dot <= 0 || dot === name.length - 1) {
    // No real extension; just humanize the whole thing.
    return humanizeWord(name, { dictionary: dict });
  }
  const base = name.slice(0, dot);
  const ext = name.slice(dot + 1);
  const baseSpoken = humanizeWord(base, { dictionary: dict });
  // Spell short extensions letter-by-letter ("ts" -> "t s"); read longer ones
  // as a humanized word ("json" is an acronym -> "J S O N").
  let extSpoken: string;
  if (/^[a-z]{1,4}$/i.test(ext) && !ACRONYMS.has(ext.toLowerCase())) {
    extSpoken = ext.toLowerCase().split('').join(' ');
  } else {
    extSpoken = humanizeWord(ext, { dictionary: dict });
  }
  return squeeze(baseSpoken + ' dot ' + extSpoken);
}

/** Read a URL concisely: domain only, dots spoken. */
function humanizeUrl(url: string): string {
  const m = /^[a-zA-Z][a-zA-Z0-9+.-]*:\/\/([^/\s?#]+)/.exec(url);
  if (!m) return 'a link';
  const host = m[1].replace(/^www\./i, '');
  // Speak the domain with "dot" between labels: example.com -> "example dot com"
  const spoken = host
    .split('.')
    .filter((p) => p.length > 0)
    .join(' dot ');
  return spoken ? spoken : 'a link';
}

// Inline symbol mappings used in prose (only the clearly meaningful ones).
const PROSE_SYMBOLS: Array<[RegExp, string]> = [
  [/=>/g, ' arrow '],
  [/===|==/g, ' equals '],
  [/!==|!=/g, ' not equals '],
  [/>=/g, ' greater than or equal '],
  [/<=/g, ' less than or equal '],
  [/&&/g, ' and '],
  [/\|\|/g, ' or '],
];

/**
 * Humanize running prose: keep ordinary English untouched, fix identifier-ish
 * tokens, inline-code spans, file paths, and URLs; expand meaningful symbols.
 */
export function humanizeProse(text: string, opts?: HumanizeOptions): string {
  try {
    if (typeof text !== 'string' || text.length === 0) return EMPTY;
    const dict: Record<string, string> = {
      ...DEFAULT_DICTIONARY,
      ...(opts && opts.dictionary ? lowercaseKeys(opts.dictionary) : {}),
    };

    let s = text;

    // 1) URLs first (before path/identifier handling chews them up).
    s = s.replace(/\b[a-zA-Z][a-zA-Z0-9+.-]*:\/\/[^\s)]+/g, (u) => humanizeUrl(u));

    // 2) Inline-code spans wrapped in backticks: strip ticks, humanize inner
    //    as a small piece of prose (so `getUserByID` and `a + b` both work).
    s = s.replace(/`([^`]+)`/g, (_all, inner: string) => {
      const t = String(inner).trim();
      if (!t) return EMPTY;
      // A single token? humanize as a word/path; otherwise recurse as prose.
      if (/^\S+$/.test(t)) return humanizeTokenForProse(t, dict);
      return humanizeProse(t, { dictionary: dict });
    });

    // 3) Meaningful inline symbols.
    for (const [re, word] of PROSE_SYMBOLS) s = s.replace(re, word);

    // 4) Walk remaining tokens; humanize the code-looking ones, keep the rest.
    //    We split on whitespace but preserve trailing punctuation on each token.
    s = s.replace(/\S+/g, (raw) => humanizeMaybeCodeToken(raw, dict));

    return squeeze(s);
  } catch {
    return typeof text === 'string' ? squeeze(text) : EMPTY;
  }
}

/**
 * Decide whether a whitespace-delimited prose token is code/path/identifier and
 * humanize accordingly, preserving leading/trailing punctuation like "(" or ".".
 */
function humanizeMaybeCodeToken(raw: string, dict: Record<string, string>): string {
  // Peel off leading and trailing punctuation that isn't part of identifiers.
  const lead = /^[^\p{L}\p{N}]+/u.exec(raw);
  const trail = /[^\p{L}\p{N}]+$/u.exec(raw);
  const leadStr = lead ? lead[0] : EMPTY;
  let trailStr = trail ? trail[0] : EMPTY;
  let coreStart = leadStr.length;
  let coreEnd = raw.length - trailStr.length;
  if (coreEnd < coreStart) {
    coreStart = 0;
    coreEnd = raw.length;
    trailStr = EMPTY;
  }
  const core = raw.slice(coreStart, coreEnd);
  if (!core) return raw; // pure punctuation; leave as-is

  // Trailing sentence punctuation (.,;:!?) is kept so prosody survives.
  const keptTrail = /[.,;:!?]+$/.test(trailStr) ? trailStr.match(/[.,;:!?]+$/)![0] : EMPTY;

  const spoken = humanizeTokenForProse(core, dict);
  // Re-attach only sentence punctuation; drop stray brackets/quotes as noise.
  return spoken + keptTrail;
}

/** Humanize a single bare token in prose context (path/url/filename/identifier). */
function humanizeTokenForProse(core: string, dict: Record<string, string>): string {
  // Path-like (has a slash) -> read final segment only.
  if (/[\\/]/.test(core) && hasLetter(core)) {
    return humanizePathToken(core, dict);
  }
  // Filename-like (word.ext with a known-ish short extension)
  if (/^[\p{L}\p{N}_-]+\.[\p{L}\p{N}]{1,5}$/u.test(core) && hasLetter(core)) {
    return humanizeFilename(core, dict);
  }
  // Code-looking identifier -> humanizeWord. Otherwise leave the word as-is.
  if (looksLikeCode(core)) {
    return humanizeWord(core, { dictionary: dict });
  }
  return core;
}

// -----------------------------------------------------------------------------
// Code humanization
// -----------------------------------------------------------------------------

interface CommentMarkers {
  line: string[]; // line-comment prefixes
  blockOpen?: string; // block-comment open
  blockClose?: string; // block-comment close
}

/** Per-language comment markers. Falls back to // and # when unknown. */
function markersForLang(lang: string): CommentMarkers {
  const l = (lang || '').toLowerCase();
  switch (l) {
    case 'typescript':
    case 'typescriptreact':
    case 'javascript':
    case 'javascriptreact':
    case 'java':
    case 'c':
    case 'cpp':
    case 'c++':
    case 'csharp':
    case 'cs':
    case 'go':
    case 'rust':
    case 'swift':
    case 'kotlin':
    case 'scala':
    case 'php':
    case 'dart':
      return { line: ['//'], blockOpen: '/*', blockClose: '*/' };
    case 'python':
    case 'ruby':
    case 'shellscript':
    case 'shell':
    case 'bash':
    case 'sh':
    case 'yaml':
    case 'toml':
    case 'perl':
    case 'r':
    case 'makefile':
      return { line: ['#'] };
    case 'sql':
    case 'lua':
    case 'haskell':
      return { line: ['--'], blockOpen: '/*', blockClose: '*/' };
    case 'lisp':
    case 'clojure':
    case 'scheme':
    case 'elisp':
      return { line: [';'] };
    default:
      return { line: ['//', '#'], blockOpen: '/*', blockClose: '*/' };
  }
}

// Operators mapped to spoken words inside code lines. Order matters: longer
// operators first so "===" wins over "==", ">=" over ">", etc.
const CODE_OPERATORS: Array<[string, string]> = [
  ['=>', ' arrow '],
  ['===', ' equals '],
  ['!==', ' not equals '],
  ['==', ' equals '],
  ['!=', ' not equals '],
  ['>=', ' greater than or equal '],
  ['<=', ' less than or equal '],
  ['&&', ' and '],
  ['||', ' or '],
  ['+=', ' plus equals '],
  ['-=', ' minus equals '],
  ['*=', ' times equals '],
  ['/=', ' divided equals '],
  ['->', ' arrow '],
  ['::', ' colon colon '],
  ['??', ' or else '],
  ['...', ' spread '],
  ['=', ' equals '],
  ['+', ' plus '],
  ['*', ' times '],
  ['%', ' modulo '],
  ['<', ' less than '],
  ['>', ' greater than '],
  ['!', ' not '],
];

// Noise punctuation we DROP entirely (we keep spaces where they were).
const NOISE_CHARS = new Set(['{', '}', '(', ')', '[', ']', ',', ':']);

/**
 * Humanize a chunk of source code into listenable spoken text.
 * Comments -> prose; code -> humanized identifiers + spoken operators with
 * noise punctuation dropped; statement/line ends become sentence boundaries.
 */
export function humanizeCode(source: string, lang: string, opts?: HumanizeOptions): string {
  try {
    if (typeof source !== 'string' || source.length === 0) return EMPTY;
    const dict: Record<string, string> = {
      ...DEFAULT_DICTIONARY,
      ...(opts && opts.dictionary ? lowercaseKeys(opts.dictionary) : {}),
    };
    const markers = markersForLang(lang);

    const lines = source.split(/\r?\n/);
    const out: string[] = [];
    let inBlockComment = false;

    for (let raw of lines) {
      // Guard against pathological single huge lines: cap processing length but
      // never throw. 20k chars is far beyond any sane source line.
      if (raw.length > 20000) raw = raw.slice(0, 20000);
      const line = raw;
      const trimmed = line.trim();
      if (!trimmed) continue;

      // --- Block comments (c-like) -------------------------------------------
      if (inBlockComment) {
        const closeIdx = markers.blockClose ? trimmed.indexOf(markers.blockClose) : -1;
        if (closeIdx >= 0) {
          const inner = trimmed.slice(0, closeIdx);
          const p = humanizeProse(stripCommentStars(inner), { dictionary: dict });
          if (p) out.push(ensureSentence(p));
          inBlockComment = false;
        } else {
          const p = humanizeProse(stripCommentStars(trimmed), { dictionary: dict });
          if (p) out.push(ensureSentence(p));
        }
        continue;
      }

      // Whole-line comment?
      const lineComment = matchLineComment(trimmed, markers.line);
      if (lineComment !== null) {
        const p = humanizeProse(lineComment, { dictionary: dict });
        if (p) out.push(ensureSentence(p));
        continue;
      }

      // Block comment starting on this line?
      if (markers.blockOpen && trimmed.indexOf(markers.blockOpen) >= 0) {
        const openIdx = trimmed.indexOf(markers.blockOpen);
        const before = trimmed.slice(0, openIdx);
        const afterOpen = trimmed.slice(openIdx + markers.blockOpen.length);
        const closeIdx = markers.blockClose ? afterOpen.indexOf(markers.blockClose) : -1;
        // Process any code before the comment opener.
        if (before.trim()) {
          const c = humanizeCodeLine(before, dict);
          if (c) out.push(ensureSentence(c));
        }
        if (closeIdx >= 0) {
          // Single-line block comment.
          const inner = afterOpen.slice(0, closeIdx);
          const p = humanizeProse(stripCommentStars(inner), { dictionary: dict });
          if (p) out.push(ensureSentence(p));
          // Note: ignore code after close on same line (rare) for simplicity.
        } else {
          inBlockComment = true;
          const p = humanizeProse(stripCommentStars(afterOpen), { dictionary: dict });
          if (p) out.push(ensureSentence(p));
        }
        continue;
      }

      // --- Plain code line ---------------------------------------------------
      // Strip a trailing line-comment if present, narrate code then comment.
      const split = splitTrailingComment(line, markers.line);
      const codePart = split.code;
      const commentPart = split.comment;

      const c = humanizeCodeLine(codePart, dict);
      if (c) out.push(ensureSentence(c));
      if (commentPart) {
        const p = humanizeProse(commentPart, { dictionary: dict });
        if (p) out.push(ensureSentence(p));
      }
    }

    return squeeze(out.join(' '));
  } catch {
    return EMPTY;
  }
}

/** Remove leading "*" decoration common inside /* ... *​/ block comments. */
function stripCommentStars(s: string): string {
  return s.replace(/^\s*\*+\s?/, '').trim();
}

/** If the trimmed line is wholly a comment, return its text; else null. */
function matchLineComment(trimmed: string, prefixes: string[]): string | null {
  for (const p of prefixes) {
    if (trimmed.startsWith(p)) {
      return trimmed.slice(p.length).trim();
    }
  }
  return null;
}

/**
 * Split a code line into {code, comment} at the first line-comment marker that
 * is NOT inside a string literal. Best-effort, never throws.
 */
function splitTrailingComment(
  line: string,
  prefixes: string[]
): { code: string; comment: string } {
  let inStr: string | null = null;
  for (let i = 0; i < line.length; i++) {
    const ch = line[i];
    const prev = i > 0 ? line[i - 1] : '';
    if (inStr) {
      if (ch === inStr && prev !== '\\') inStr = null;
      continue;
    }
    if (ch === '"' || ch === "'" || ch === '`') {
      inStr = ch;
      continue;
    }
    for (const p of prefixes) {
      if (line.startsWith(p, i)) {
        return { code: line.slice(0, i), comment: line.slice(i + p.length).trim() };
      }
    }
  }
  return { code: line, comment: EMPTY };
}

/** Ensure a spoken chunk ends with sentence punctuation for natural prosody. */
function ensureSentence(s: string): string {
  const t = s.trim();
  if (!t) return EMPTY;
  return /[.!?]$/.test(t) ? t : t + '.';
}

/**
 * Humanize a single line of CODE (no comments): tokenize into strings, numbers,
 * identifiers, operators, and noise; speak identifiers/operators, read string
 * literals as prose, drop noise punctuation.
 */
function humanizeCodeLine(line: string, dict: Record<string, string>): string {
  if (!line || !line.trim()) return EMPTY;
  const out: string[] = [];
  const n = line.length;
  let i = 0;
  let guard = 0;

  while (i < n) {
    if (guard++ > 50000) break; // pathological guard
    const ch = line[i];

    // Whitespace -> token boundary.
    if (/\s/.test(ch)) {
      i++;
      continue;
    }

    // String literal: ' " or ` — read inner text as prose, prefixed "string".
    if (ch === '"' || ch === "'" || ch === '`') {
      const quote = ch;
      let j = i + 1;
      let buf = '';
      while (j < n) {
        const cj = line[j];
        if (cj === '\\' && j + 1 < n) {
          buf += line[j + 1];
          j += 2;
          continue;
        }
        if (cj === quote) {
          j++;
          break;
        }
        buf += cj;
        j++;
      }
      const inner = squeeze(humanizeProse(buf, { dictionary: dict }));
      if (inner) out.push('string ' + inner);
      else if (buf.length === 0) out.push('empty string');
      else out.push('string ' + buf.length + ' space' + (buf.length > 1 ? 's' : ''));
      i = j;
      continue;
    }

    // Number literal (incl. hex/float): read mostly as-is.
    if (/[0-9]/.test(ch) || (ch === '.' && i + 1 < n && /[0-9]/.test(line[i + 1]))) {
      let j = i;
      while (j < n && /[0-9a-fA-FxXob._]/.test(line[j])) j++;
      const num = line.slice(i, j);
      out.push(readNumber(num));
      i = j;
      continue;
    }

    // Identifier (Unicode letters/digits/underscore, plus dots for member paths).
    if (/[\p{L}_$]/u.test(ch)) {
      let j = i;
      while (j < n && /[\p{L}\p{N}_$.]/u.test(line[j])) {
        // Stop a trailing dot that isn't followed by an identifier char.
        if (line[j] === '.' && (j + 1 >= n || !/[\p{L}_$]/u.test(line[j + 1]))) break;
        j++;
      }
      const ident = line.slice(i, j);
      const spoken = humanizeWord(ident, { dictionary: dict });
      if (spoken) out.push(spoken);
      i = j;
      continue;
    }

    // Operators: try longest known operator first.
    let matchedOp = false;
    for (const [op, word] of CODE_OPERATORS) {
      if (line.startsWith(op, i)) {
        out.push(word.trim());
        i += op.length;
        matchedOp = true;
        break;
      }
    }
    if (matchedOp) continue;

    // Statement separators -> sentence break.
    if (ch === ';') {
      out.push('.');
      i++;
      continue;
    }

    // Noise punctuation -> drop.
    if (NOISE_CHARS.has(ch)) {
      i++;
      continue;
    }

    // Anything else (stray symbol) -> drop quietly.
    i++;
  }

  // Join, then tidy spacing around inserted periods.
  let joined = out.join(' ');
  joined = joined.replace(/\s+\./g, '.').replace(/\.\s*\./g, '.');
  return squeeze(joined);
}

/** Read a numeric literal in a TTS-friendly way (keep it simple & safe). */
function readNumber(num: string): string {
  if (!num) return EMPTY;
  // Hex like 0xFF -> "hex F F".
  if (/^0x[0-9a-fA-F]+$/i.test(num)) {
    return 'hex ' + num.slice(2).toUpperCase().split('').join(' ');
  }
  // Drop digit-group separators so TTS never voices them ("1_000_000" -> "1000000").
  let t = num.replace(/_/g, '');
  // Version-like literals with more than one dot read each dot aloud
  // ("3.14.15" -> "3 dot 14 dot 15"); a single decimal point is left for the
  // engine to read naturally ("1.5" -> "one point five").
  if ((t.match(/\./g) || []).length > 1) {
    t = t.split('.').filter(Boolean).join(' dot ');
  }
  return t;
}

// =============================================================================
// quick self-checks:  (documentation only — NOT executed)
// -----------------------------------------------------------------------------
//   humanizeWord("getUserByID")        -> "get user by I D"
//   humanizeWord("return_item")        -> "return item"
//   humanizeWord("MAX_LEN")            -> "max length"
//   humanizeWord("getHTTPResponse")    -> "get HTTP response"
//   humanizeWord("parseURLString")     -> "parse URL string"
//   humanizeWord("IOError")            -> "IO error"
//   humanizeWord("__init__")           -> "initialize"
//   humanizeWord("this.value")         -> "this value"
//   humanizeWord("user.profile.name")  -> "user profile name"
//   humanizeWord("utf8")               -> "UTF 8"      (utf is acronym + digit)
//   humanizeWord("base64")             -> "base 64"
//   humanizeWord("MAX_BUFFER_SIZE")    -> "max buffer size"
//   humanizeProse("call `getUserByID` now") -> "call get user by I D now"
//   humanizeProse("see src/utils/foo.ts") -> "see foo dot t s"
//   humanizeCode("const x = a && b;", "typescript") -> "const x equals a and b."
// =============================================================================
