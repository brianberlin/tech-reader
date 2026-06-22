// Piper neural TTS (https://github.com/OHF-Voice/piper1-gpl) via its CLI. Like
// SayEngine, we render each sentence to a WAV buffer in the extension host and
// the webview plays it through <audio>. Local, offline, and much more natural
// than the OS `say` voices. A "voice" is a Piper .onnx model file.

import { spawn } from 'child_process';
import { promises as fs, existsSync } from 'fs';
import * as os from 'os';
import * as path from 'path';

export interface PiperVoice {
  name: string; // model filename without .onnx, e.g. "en_US-ryan-high"
  id: string;   // absolute path to the .onnx file (used as the voice id)
}

function expandHome(p: string): string {
  if (!p) return p;
  if (p === '~') return os.homedir();
  if (p.startsWith('~/')) return path.join(os.homedir(), p.slice(2));
  return p;
}

export class PiperEngine {
  readonly mime = 'audio/wav';
  private counter = 0;

  /** Resolve the piper executable: explicit path, else common install locations. */
  resolveBin(binPath: string): string {
    const explicit = expandHome(binPath || '');
    if (explicit && explicit !== 'piper' && existsSync(explicit)) return explicit;
    const candidates = [
      path.join(os.homedir(), '.local/bin/piper'),
      '/opt/homebrew/bin/piper',
      '/usr/local/bin/piper',
    ];
    for (const c of candidates) {
      if (existsSync(c)) return c;
    }
    return 'piper'; // fall back to PATH lookup
  }

  /** True if a piper binary can be located. */
  available(binPath: string): boolean {
    const bin = this.resolveBin(binPath);
    return bin !== 'piper' || existsSync('/opt/homebrew/bin/piper') || existsSync(path.join(os.homedir(), '.local/bin/piper'));
  }

  /** List *.onnx voice models in a directory. Never throws. */
  async listVoices(voicesDir: string): Promise<PiperVoice[]> {
    const dir = expandHome(voicesDir || '');
    if (!dir) return [];
    try {
      const files = await fs.readdir(dir);
      return files
        .filter((f) => f.endsWith('.onnx'))
        .map((f) => ({ name: f.replace(/\.onnx$/, ''), id: path.join(dir, f) }))
        .sort((a, b) => a.name.localeCompare(b.name));
    } catch {
      return [];
    }
  }

  /** Synthesize text to a WAV buffer using the given model. `lengthScale` tweaks
   *  pace (1 = normal; >1 slower). Speed is mostly handled by <audio>.playbackRate. */
  async synth(text: string, opts: { binPath: string; modelPath: string; lengthScale?: number; signal?: AbortSignal }): Promise<Buffer> {
    const clean = (text || '').trim();
    const model = expandHome(opts.modelPath || '');
    if (!clean || !model) return Buffer.alloc(0);
    const bin = this.resolveBin(opts.binPath);
    const file = path.join(os.tmpdir(), `tech-reader-piper-${process.pid}-${this.counter++}.wav`);
    const args = ['-m', model, '-f', file];
    if (opts.lengthScale && opts.lengthScale > 0 && opts.lengthScale !== 1) {
      args.push('--length-scale', String(opts.lengthScale));
    }
    try {
      await runWithStdin(bin, args, clean, opts.signal);
      return await fs.readFile(file);
    } finally {
      fs.unlink(file).catch(() => {
        /* best effort */
      });
    }
  }
}

function runWithStdin(cmd: string, args: string[], stdin: string, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    if (signal?.aborted) { reject(new Error('aborted')); return; }
    const p = spawn(cmd, args);
    const onAbort = () => { try { p.kill('SIGTERM'); } catch { /* ignore */ } };
    signal?.addEventListener('abort', onAbort, { once: true });
    const finish = (fn: () => void) => { signal?.removeEventListener('abort', onAbort); fn(); };
    let err = '';
    p.stderr.on('data', (d) => (err += d));
    p.on('error', (e) => finish(() => reject(e)));
    p.on('close', (code) => finish(() => (code === 0 ? resolve() : reject(new Error(err.trim() || `${cmd} exited ${code}`)))));
    p.stdin.on('error', () => {
      /* ignore EPIPE */
    });
    p.stdin.end(stdin);
  });
}
