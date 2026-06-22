// System/user prompts for the local Ollama model. The golden rule for every
// prompt: the OUTPUT is going straight into a text-to-speech engine, so it must
// be plain spoken prose — no markdown, no code fences, no bullets, and no
// punctuation/symbols spelled out as words.
//
// The two worked examples below are taken from how the author wants code and
// tables to sound, and act as few-shot guidance for small local models.

import type { Block } from '../types';

export type PromptKind = 'code' | 'prose' | 'table';

const SPEAK_RULES = [
  'Write only plain spoken prose in complete sentences. No markdown, no headings, no bullet points, no code fences, no emoji.',
  'Never spell out punctuation or symbols (do not say "underscore", "open brace", "hash", or "pipe").',
  'Pronounce identifiers as ordinary words: read snake_case and camelCase as separate words and expand obvious abbreviations (for example, "fn" is "function", "idx" is "index").',
  'Keep it concise and easy to follow by ear.',
].join(' ');

const CODE_EXAMPLE =
  'Example. Given this code:\n' +
  'function hello(name: string): string {\n  if (name) {\n    return "Hello, #{name}"\n  }\n  return "Hello, World!"\n}\n' +
  'a good narration is: "Hello function accepts a single parameter, name, which is expected to be a string. ' +
  'If name is not empty, it returns a personalized greeting. Otherwise, it returns Hello, World." ' +
  'Notice it explains the parameter and its type, describes each branch, and does NOT read the string-interpolation token literally.';

const TABLE_EXAMPLE =
  'Example. A table comparing two systems across many attributes (order source, order type, inventory model, and so on) ' +
  'should be distilled to its essence, for instance: ' +
  '"PMI is a traditional SAP customer-order business. PACT is a Shopify resale business with item master data available but unused for returns." ' +
  'Do not read the columns or rows.';

export function systemPrompt(block: Block, kind: PromptKind): string {
  if (kind === 'code') {
    return (
      `You are a narrator who explains source code out loud to a developer who cannot see the screen. ` +
      `Explain what this ${block.lang || 'code'} does: its purpose, the parameters it takes and their types, what it returns, and any notable conditions or side effects. ` +
      `Do NOT read the code line by line or quote it verbatim, and do NOT read string-interpolation or format placeholders (like #{...}, ${'${...}'}, %s) literally — describe the resulting value instead. ` +
      `Speak in a few short sentences. ${SPEAK_RULES}\n\n${CODE_EXAMPLE}`
    );
  }
  if (kind === 'table') {
    return (
      `You are a narrator who explains documentation out loud. The following is a table. ` +
      `Do NOT read the rows or cells one by one. Summarize what the table conveys in one or two short sentences — the key comparison or takeaway a listener needs. ` +
      `${SPEAK_RULES}\n\n${TABLE_EXAMPLE}`
    );
  }
  return (
    `You are a narrator who reads technical documentation out loud naturally. ` +
    `Rewrite the following text so it sounds good when spoken: drop any markup, expand identifiers and abbreviations, and smooth out anything awkward to hear. ` +
    `Preserve the meaning and the important details — do not drop information. ${SPEAK_RULES}`
  );
}

export function userPrompt(block: Block): string {
  const label =
    block.kind === 'code' ? `Code (${block.lang}):` :
    block.kind === 'comment' ? `Code comment:` :
    block.kind === 'table' ? `Table:` :
    `Text:`;
  return `${label}\n\n${block.source}`;
}
