// macOS text-to-speech via the built-in `say` command. We synthesize each
// sentence to a WAV buffer in the extension host and hand the bytes to the
// webview, which plays them through an <audio> element. This sidesteps the VS
// Code webview's broken speechSynthesis audio output (events fire but no sound)
// while staying fully local/offline and using the same system voices.

import { spawn } from 'child_process';
import { promises as fs } from 'fs';
import * as os from 'os';
import * as path from 'path';

export interface SayVoice {
  name: string;
  locale: string; // BCP-47, e.g. "en-US"
}

export class SayEngine {
  readonly mime = 'audio/wav';
  private voices: SayVoice[] | undefined;
  private counter = 0;

  /** `say` only exists on macOS. */
  get available(): boolean {
    return process.platform === 'darwin';
  }

  /** Installed voices, parsed once from `say -v '?'`. Never throws. */
  async listVoices(): Promise<SayVoice[]> {
    if (this.voices) return this.voices;
    if (!this.available) return (this.voices = []);
    try {
      const out = await run('say', ['-v', '?']);
      const voices: SayVoice[] = [];
      for (const line of out.split('\n')) {
        // "Samantha            en_US    # Hello! My name is Samantha."
        const m = line.match(/^(.+?)\s{2,}([a-z]{2,3}[_-][A-Z]{2})\b/);
        if (m) voices.push({ name: m[1].trim(), locale: m[2].replace('_', '-') });
      }
      this.voices = voices;
    } catch {
      this.voices = [];
    }
    return this.voices;
  }

  /** A sensible default voice name (Samantha → any English → first). */
  async defaultVoice(): Promise<string> {
    const v = await this.listVoices();
    return (
      v.find((x) => x.name === 'Samantha')?.name ||
      v.find((x) => /^en/i.test(x.locale))?.name ||
      v[0]?.name ||
      ''
    );
  }

  /** Synthesize text to a 16-bit PCM WAV buffer. Returns empty buffer if unavailable/empty. */
  async synth(text: string, voice: string | undefined): Promise<Buffer> {
    const clean = (text || '').trim();
    if (!clean || !this.available) return Buffer.alloc(0);
    const file = path.join(os.tmpdir(), `tech-reader-${process.pid}-${this.counter++}.wav`);
    const args = ['-o', file, '--file-format=WAVE', '--data-format=LEI16@22050'];
    if (voice) args.unshift('-v', voice);
    try {
      await runWithStdin('say', args, clean);
      return await fs.readFile(file);
    } finally {
      fs.unlink(file).catch(() => {
        /* best effort */
      });
    }
  }
}

function run(cmd: string, args: string[]): Promise<string> {
  return new Promise((resolve, reject) => {
    const p = spawn(cmd, args);
    let out = '';
    let err = '';
    p.stdout.on('data', (d) => (out += d));
    p.stderr.on('data', (d) => (err += d));
    p.on('error', reject);
    p.on('close', (code) => (code === 0 ? resolve(out) : reject(new Error(err || `${cmd} exited ${code}`))));
  });
}

function runWithStdin(cmd: string, args: string[], stdin: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const p = spawn(cmd, args);
    let err = '';
    p.stderr.on('data', (d) => (err += d));
    p.on('error', reject);
    p.on('close', (code) => (code === 0 ? resolve() : reject(new Error(err || `${cmd} exited ${code}`))));
    p.stdin.on('error', () => {
      /* ignore EPIPE if the process dies early */
    });
    p.stdin.end(stdin); // pass text via stdin → no arg-length or escaping issues
  });
}
