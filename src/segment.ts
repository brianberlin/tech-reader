// Sentence segmentation. Two uses:
//  - splitSentences(): batch-split a finished string into TTS-sized chunks.
//  - makeSentenceStreamer(): pull complete sentences out of a growing buffer as
//    Ollama streams tokens, so the webview can start speaking the first sentence
//    before the model has finished the paragraph.

const MAX_LEN = 260; // target characters per spoken chunk

function segmenterFor(locale: string): Intl.Segmenter {
  const base = (locale || 'en').split('-')[0] || 'en';
  try {
    return new Intl.Segmenter(base, { granularity: 'sentence' });
  } catch {
    return new Intl.Segmenter('en', { granularity: 'sentence' });
  }
}

/** Split finished text into sentence-ish chunks, combining short sentences and
 *  hard-wrapping pathologically long ones so no single utterance is unwieldy. */
export function splitSentences(text: string, locale = 'en'): string[] {
  const clean = text.replace(/\s+/g, ' ').trim();
  if (!clean) return [];
  const seg = segmenterFor(locale);
  const raw = [...seg.segment(clean)].map((s) => s.segment.trim()).filter(Boolean);
  const out: string[] = [];
  let buf = '';
  const flush = () => {
    if (buf.trim()) out.push(buf.trim());
    buf = '';
  };
  for (const s of raw) {
    if (s.length > MAX_LEN) {
      flush();
      out.push(...splitLong(s));
      continue;
    }
    if (!buf) buf = s;
    else if (buf.length + 1 + s.length <= MAX_LEN) buf += ' ' + s;
    else { flush(); buf = s; }
  }
  flush();
  return out;
}

function splitLong(s: string): string[] {
  const parts = s.split(/(?<=[,;:—–])\s+/);
  const out: string[] = [];
  let buf = '';
  for (const p of parts) {
    if (!buf) buf = p;
    else if (buf.length + 1 + p.length <= MAX_LEN) buf += ' ' + p;
    else { out.push(buf.trim()); buf = p; }
  }
  if (buf.trim()) out.push(buf.trim());
  return out.flatMap((part) => (part.length <= MAX_LEN * 1.5 ? [part] : hardWrap(part, MAX_LEN)));
}

function hardWrap(s: string, size: number): string[] {
  const words = s.split(/\s+/);
  const out: string[] = [];
  let buf = '';
  for (const w of words) {
    if (w.length > size) {
      // a single token longer than the limit (e.g. a huge base64 blob or URL):
      // flush the buffer and hard-slice it so no utterance is unwieldy.
      if (buf) { out.push(buf); buf = ''; }
      for (let i = 0; i < w.length; i += size) out.push(w.slice(i, i + size));
      continue;
    }
    if (buf && buf.length + 1 + w.length > size) { out.push(buf); buf = w; }
    else buf = buf ? buf + ' ' + w : w;
  }
  if (buf) out.push(buf);
  return out;
}

export interface SentenceStreamer {
  /** Feed a streamed token/chunk; returns any newly-completed sentences. */
  push(chunk: string): string[];
  /** Emit whatever remains as a final sentence (or two). Call once at block end. */
  flush(): string[];
}

/**
 * Pull complete sentences from a streaming buffer. A sentence is considered
 * complete when a terminator (. ! ? or a hard line break) is followed by
 * whitespace — i.e. the model has clearly moved on. Tiny fragments (e.g. "e.g.")
 * are held back and merged with the following text to avoid choppy speech.
 */
export function makeSentenceStreamer(locale = 'en'): SentenceStreamer {
  let buf = '';
  // a run of non-terminators, then 1+ terminators / a newline, then whitespace
  const boundary = /^([\s\S]*?(?:[.!?]+['")\]]?|\n))(\s)/;

  const pull = (): string[] => {
    const out: string[] = [];
    // collapse hard breaks so a blank line acts as a strong boundary
    for (;;) {
      const m = boundary.exec(buf);
      if (!m) break;
      const candidate = m[1].replace(/\s+/g, ' ').trim();
      buf = buf.slice(m[0].length);
      if (!candidate) continue;
      // hold back very short fragments (likely abbreviations) unless they end a line
      const words = candidate.match(/\S+/g) || [];
      if (words.length < 2 && !/\n/.test(m[1]) && buf) {
        buf = candidate + ' ' + buf;
        break;
      }
      out.push(...splitSentences(candidate, locale));
    }
    return out;
  };

  return {
    push(chunk: string): string[] {
      buf += chunk;
      return pull();
    },
    flush(): string[] {
      const rest = buf;
      buf = '';
      return splitSentences(rest, locale);
    },
  };
}
