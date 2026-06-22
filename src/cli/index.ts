import { promises as fs } from 'fs';
import * as os from 'os';
import * as path from 'path';
import { segmentBlocks } from '../blocks';
import { narrate } from '../narrate/narrator';
import { PiperEngine } from '../engines/piper';
import type { Mode } from '../types';
import { languageIdForPath } from './lang';
import { Session } from './session';
import { Player } from './player';
import { Tui } from './tui';

interface Args {
  file?: string;
  mode: Mode;
  voice?: string;
  voicesDir: string;
  piperBin: string;
  speed: number;
  lengthScale: number;
  model: string;
  ollamaUrl: string;
  proseHandling: 'ai' | 'verbatim';
  text: boolean;
  help: boolean;
}

function expandHome(p: string): string {
  if (p === '~') return os.homedir();
  if (p.startsWith('~/')) return path.join(os.homedir(), p.slice(2));
  return p;
}

function parseArgs(argv: string[]): Args {
  const a: Args = {
    mode: 'ai',
    voicesDir: process.env.TECH_READER_VOICES_DIR || '~/.local/share/piper-voices',
    piperBin: process.env.TECH_READER_PIPER || '',
    speed: 1,
    lengthScale: 1,
    model: process.env.TECH_READER_MODEL || 'llama3.2',
    ollamaUrl: process.env.OLLAMA_HOST || 'http://localhost:11434',
    proseHandling: 'ai',
    text: false,
    help: false,
  };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    const next = () => argv[++i];
    switch (arg) {
      case '-h': case '--help': a.help = true; break;
      case '--literal': a.mode = 'literal'; break;
      case '--ai': a.mode = 'ai'; break;
      case '--mode': a.mode = next() === 'literal' ? 'literal' : 'ai'; break;
      case '--verbatim': a.proseHandling = 'verbatim'; break;
      case '--voice': case '-v': a.voice = next(); break;
      case '--voices-dir': a.voicesDir = next(); break;
      case '--piper': a.piperBin = next(); break;
      case '--speed': case '-s': a.speed = clamp(parseFloat(next()), 0.5, 2.5, 1); break;
      case '--length-scale': a.lengthScale = clamp(parseFloat(next()), 0.5, 2, 1); break;
      case '--model': case '-m': a.model = next(); break;
      case '--ollama': a.ollamaUrl = next(); break;
      case '--text': case '--no-audio': a.text = true; break;
      default:
        if (!arg.startsWith('-') && !a.file) a.file = arg;
    }
  }
  return a;
}

function clamp(n: number, lo: number, hi: number, dflt: number): number {
  return Number.isFinite(n) ? Math.min(hi, Math.max(lo, n)) : dflt;
}

const HELP = `tech-reader — read code, comments, and docs aloud, explained.

Usage:
  tech-reader <file> [options]

Options:
  --ai | --literal         Explain via local Ollama (default) or read with the offline humanizer
  --verbatim               Read prose faithfully (don't let the AI rephrase it)
  -v, --voice <name|path>  Piper voice model name or .onnx path
  --voices-dir <dir>       Where Piper .onnx voices live (default ~/.local/share/piper-voices)
  --piper <path>           Path to the piper binary (auto-detected by default)
  -s, --speed <n>          Playback speed 0.5–2.5 (default 1)
  --length-scale <n>       Piper pacing (default 1; higher = slower)
  -m, --model <name>       Ollama model (default llama3.2)
  --ollama <url>           Ollama base URL (default http://localhost:11434)
  --text, --no-audio       Print the narration to stdout instead of playing it
  -h, --help               Show this help

Keys (while reading):
  space pause · ←/→ prev/next sentence · +/- speed · ↑/↓ scroll · q quit
`;

async function resolveVoice(a: Args): Promise<{ voice?: string; error?: string }> {
  const piper = new PiperEngine();
  if (a.voice) {
    if (a.voice.endsWith('.onnx')) return { voice: expandHome(a.voice) };
    const voices = await piper.listVoices(a.voicesDir);
    const match = voices.find((v) => v.name === a.voice);
    if (match) return { voice: match.id };
    return {
      error: `voice "${a.voice}" not found in ${a.voicesDir}.` +
        (voices.length ? ` Available: ${voices.map((v) => v.name).join(', ')}.` : ' (no voices there yet)'),
    };
  }
  if (process.env.TECH_READER_VOICE) return { voice: expandHome(process.env.TECH_READER_VOICE) };
  const voices = await piper.listVoices(a.voicesDir);
  const v = voices.find((x) => /ryan/i.test(x.name))?.id || voices[0]?.id;
  return v ? { voice: v } : {};
}

async function main(): Promise<void> {
  const a = parseArgs(process.argv.slice(2));
  if (a.help || !a.file) {
    process.stdout.write(HELP);
    process.exit(a.file ? 0 : a.help ? 0 : 1);
  }

  let source: string;
  try {
    source = await fs.readFile(a.file!, 'utf8');
  } catch (err) {
    process.stderr.write(`tech-reader: cannot read ${a.file}: ${String((err as any)?.message || err)}\n`);
    process.exit(1);
  }

  const lang = languageIdForPath(a.file!);
  const blocks = segmentBlocks(source, lang, 1);
  const session = new Session();
  session.title = path.basename(a.file!);

  const ac = new AbortController();
  const narratePromise = narrate(
    blocks,
    {
      mode: a.mode,
      ollama: { baseUrl: a.ollamaUrl, model: a.model, temperature: 0.3 },
      settings: { codeHandling: 'explain', tables: 'summarize', announceHeadings: false },
      proseHandling: a.proseHandling,
      dictionary: {},
      locale: 'en',
      signal: ac.signal,
    },
    {
      onSentence: (s) => session.push(s),
      onStatus: (state, message) => session.setStatus(state, message),
    }
  ).catch((err) => session.setStatus('error', String((err as any)?.message || err)))
   .finally(() => session.markDone());

  // ---- text mode: just print the narration ----
  if (a.text) {
    let printed = 0;
    const flush = () => {
      while (printed < session.sentences.length) {
        const s = session.sentences[printed++];
        if (s) process.stdout.write(s.text + '\n');
      }
    };
    session.on('update', flush);
    await narratePromise;
    flush();
    if (!session.sentences.length) process.stderr.write('tech-reader: nothing readable.\n');
    process.exit(session.sentences.length ? 0 : 1);
  }

  // ---- audio + TUI ----
  const { voice, error: voiceError } = await resolveVoice(a);
  if (!voice) {
    if (voiceError) {
      process.stderr.write(`tech-reader: ${voiceError}\n`);
    } else {
      process.stderr.write(
        `tech-reader: no Piper voice found in ${a.voicesDir}.\n` +
        `Install one, e.g.:\n` +
        `  uv tool install piper-tts\n` +
        `  mkdir -p ${a.voicesDir} && cd ${a.voicesDir}\n` +
        `  curl -LO https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/ryan/high/en_US-ryan-high.onnx\n` +
        `  curl -LO https://huggingface.co/rhasspy/piper-voices/resolve/main/en/en_US/ryan/high/en_US-ryan-high.onnx.json\n` +
        `Or run with --text to print the narration without audio.\n`
      );
    }
    ac.abort();
    process.exit(1);
  }

  const player = new Player(session, {
    piperBin: a.piperBin,
    model: voice,
    lengthScale: a.lengthScale,
    speed: a.speed,
  });

  let tui: Tui | undefined;
  let cleaned = false;
  // Always restore the terminal — a crash must never leave the shell in raw mode
  // / alternate-screen. Register these BEFORE the TUI enters the alternate screen,
  // so even a throw inside tui.start() is cleaned up via the 'exit' handler.
  const cleanup = () => {
    if (cleaned) return;
    cleaned = true;
    try { player.dispose(); } catch { /* ignore */ }
    try { tui?.dispose(); } catch { /* ignore */ }
  };
  const quit = (code = 0) => { ac.abort(); cleanup(); process.exit(code); };
  process.on('exit', cleanup);
  process.on('SIGINT', () => quit(0));
  process.on('SIGTERM', () => quit(0));
  process.on('uncaughtException', (e) => { cleanup(); process.stderr.write('\n' + String(e?.stack || e) + '\n'); process.exit(1); });
  process.on('unhandledRejection', (e) => { cleanup(); process.stderr.write('\n' + String((e as any)?.stack || e) + '\n'); process.exit(1); });
  tui = new Tui(session, player, quit);
  tui.start();
  player.start();
}

main().catch((err) => {
  process.stderr.write(String(err?.stack || err) + '\n');
  process.exit(1);
});
