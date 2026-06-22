// The narrator orchestrates a whole reading: it walks the document's blocks and,
// for each, produces spoken sentences — either by asking the local Ollama model
// to explain/read it (streamed, so speech can start early) or by running the
// deterministic humanizer offline. Sentences are handed to the caller one at a
// time via handlers.onSentence so the webview can speak them as they arrive.

import type { Block, Mode, NarrationSettings, Sentence, StatusState } from '../types';
import { isAvailable, streamChat, OllamaError, type OllamaConfig } from '../ollama/client';
import { humanizeProse, humanizeCode } from '../humanize/humanizer';
import { systemPrompt, userPrompt } from './prompts';
import { makeSentenceStreamer, splitSentences } from '../segment';

export interface NarrateOptions {
  mode: Mode;
  ollama: OllamaConfig;
  settings: NarrationSettings;
  /** how to handle prose blocks: AI rewrite, or deterministic humanize */
  proseHandling: 'ai' | 'verbatim';
  dictionary?: Record<string, string>;
  locale?: string;
  signal: AbortSignal;
}

export interface NarrateHandlers {
  onSentence(s: Sentence): void;
  onStatus(state: StatusState, message?: string): void;
}

export async function narrate(blocks: Block[], opts: NarrateOptions, h: NarrateHandlers): Promise<void> {
  if (!blocks.length) {
    h.onStatus('empty');
    return;
  }

  const locale = opts.locale || 'en';
  const hopts = { dictionary: opts.dictionary };
  let useAi = opts.mode === 'ai';
  let idx = 0;
  let emittedAny = false;
  let status: StatusState = 'streaming';

  // Confirm Ollama is up once, up front, so we can fall back cleanly.
  if (useAi) {
    const up = await isAvailable(opts.ollama.baseUrl);
    if (opts.signal.aborted) return;
    if (!up) {
      useAi = false;
      h.onStatus('fallback', `Ollama is not reachable at ${opts.ollama.baseUrl} — reading with the offline humanizer.`);
    } else {
      status = 'thinking';
      h.onStatus('thinking', `Explaining with ${opts.ollama.model}…`);
    }
  }

  const emit = (text: string, block: Block, blockIndex: number): boolean => {
    const t = text.trim();
    if (!t || opts.signal.aborted) return false;
    if (!emittedAny && status === 'thinking') h.onStatus('streaming');
    emittedAny = true;
    const s: Sentence = {
      idx: idx++,
      text: t,
      kind: block.kind,
      startLine: block.startLine,
      endLine: block.endLine,
      blockIndex,
    };
    h.onSentence(s);
    return true;
  };

  const fallbackOnce = (msg: string) => {
    if (useAi) {
      useAi = false;
      h.onStatus('fallback', msg);
    }
  };

  // Stream one block through the model, falling back to the humanizer on failure.
  const streamBlock = async (block: Block, kind: 'code' | 'prose' | 'table', blockIndex: number) => {
    const streamer = makeSentenceStreamer(locale);
    let emittedHere = 0;
    const emitHere = (s: string) => {
      if (emit(s, block, blockIndex)) emittedHere++;
    };
    try {
      await streamChat(
        opts.ollama,
        { system: systemPrompt(block, kind), prompt: userPrompt(block), signal: opts.signal },
        (delta) => {
          for (const s of streamer.push(delta)) emitHere(s);
        }
      );
      for (const s of streamer.flush()) emitHere(s);
    } catch (err) {
      if (opts.signal.aborted || (err instanceof OllamaError && err.code === 'aborted')) throw err;
      // model missing or server dropped → degrade to offline for this and later blocks
      const why =
        err instanceof OllamaError && err.code === 'model-missing'
          ? `Model "${opts.ollama.model}" is not installed (run: ollama pull ${opts.ollama.model}) — using the offline humanizer.`
          : `Ollama error — using the offline humanizer.`;
      fallbackOnce(why);
      // Don't re-narrate a block that already streamed partial output, or the
      // listener hears the first half twice (AI version, then humanizer version).
      if (emittedHere === 0) humanizeBlock(block, blockIndex);
    }
  };

  const humanizeBlock = (block: Block, blockIndex: number) => {
    let text: string;
    if (block.kind === 'code') text = humanizeCode(block.source, block.lang, { ...hopts, code: true });
    else if (block.kind === 'table') text = humanizeProse(tableToSpeech(block.source), hopts);
    else text = humanizeProse(block.source, hopts);
    for (const s of splitSentences(text, locale)) emit(s, block, blockIndex);
  };

  for (let bi = 0; bi < blocks.length; bi++) {
    if (opts.signal.aborted) return;
    const block = blocks[bi];

    // Headings are tiny — humanize them directly rather than spend a model call.
    if (block.kind === 'heading') {
      const head = humanizeProse(block.source, hopts);
      const text = opts.settings.announceHeadings ? `Heading. ${head}` : head;
      for (const s of splitSentences(text, locale)) emit(s, block, bi);
      continue;
    }

    // Tables: distill the takeaway rather than reading every cell.
    if (block.kind === 'table') {
      if (opts.settings.tables === 'skip') continue;
      try {
        if (opts.settings.tables === 'read' || !useAi) humanizeBlock(block, bi);
        else await streamBlock(block, 'table', bi);
      } catch (err) {
        if (opts.signal.aborted || (err instanceof OllamaError && err.code === 'aborted')) return;
        try { humanizeBlock(block, bi); } catch { /* ignore */ }
      }
      continue;
    }

    const isCode = block.kind === 'code';
    if (isCode && opts.settings.codeHandling === 'skip') continue;

    try {
      if (isCode) {
        if (opts.settings.codeHandling === 'literal' || !useAi) humanizeBlock(block, bi);
        else await streamBlock(block, 'code', bi);
      } else {
        // para / quote / listItem / comment
        if (opts.proseHandling === 'verbatim' || !useAi) humanizeBlock(block, bi);
        else await streamBlock(block, 'prose', bi);
      }
    } catch (err) {
      if (opts.signal.aborted || (err instanceof OllamaError && err.code === 'aborted')) return;
      // never let one block kill the whole reading
      try { humanizeBlock(block, bi); } catch { /* ignore */ }
    }
  }

  if (opts.signal.aborted) return;
  h.onStatus(emittedAny ? 'done' : 'empty');
}

/** Offline fallback for tables: read each row's cells, dropping the |---| rule. */
function tableToSpeech(source: string): string {
  const rows = source.split(/\r\n?|\n/);
  const out: string[] = [];
  for (const raw of rows) {
    const row = raw.trim();
    if (!row) continue;
    if (/-/.test(row) && /^[\s|:-]+$/.test(row)) continue; // delimiter row
    const cells = row.replace(/^\|/, '').replace(/\|$/, '').split('|').map((c) => c.trim()).filter(Boolean);
    if (cells.length) out.push(cells.join(', '));
  }
  return out.join('. ');
}
