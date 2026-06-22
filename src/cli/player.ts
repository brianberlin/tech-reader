import { spawn, type ChildProcess } from 'child_process';
import { promises as fs } from 'fs';
import { EventEmitter } from 'events';
import * as os from 'os';
import * as path from 'path';
import { PiperEngine } from '../engines/piper';
import type { Session } from './session';

export interface PlayerOptions {
  piperBin: string;
  model: string;     // piper .onnx path
  lengthScale: number;
  speed: number;     // afplay -r
}

/** Resolves when the AbortSignal fires (or immediately if already aborted). */
function whenAborted(signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) resolve();
    else signal.addEventListener('abort', () => resolve(), { once: true });
  });
}

/**
 * Plays a Session's sentences in the terminal: synthesize one with Piper, then
 * play it with afplay, STRICTLY one at a time. Synthesizing while afplay runs
 * starves the macOS real-time audio thread and wedges CoreAudio, so we never
 * overlap them.
 *
 * Cancellation (pause/seek/setSpeed/stop) aborts the current "run": it kills any
 * afplay AND cancels any in-flight Piper process, and the next run only starts
 * after the previous one has fully unwound — so two synths can never overlap,
 * and a synth can never overlap a playback.
 *
 * Emits 'update' on any state change, 'error' with a message on failure.
 */
export class Player extends EventEmitter {
  idx = 0;
  playing = false;
  finished = false;
  speed: number;
  lastError = '';

  private runId = 0;
  private runDone: Promise<void> = Promise.resolve();
  private abort: AbortController | undefined;
  private afproc: ChildProcess | undefined;
  private readonly piper = new PiperEngine();
  private readonly cache = new Map<string, Buffer>();

  constructor(private readonly session: Session, private readonly opts: PlayerOptions) {
    super();
    this.speed = opts.speed;
  }

  start(): void {
    if (this.playing) return;
    if (this.finished) { this.finished = false; this.idx = 0; }
    this.playing = true;
    this.lastError = '';
    this.emit('update');
    this.restart(this.idx);
  }
  togglePause(): void { this.playing ? this.pause() : this.start(); }
  pause(): void { this.playing = false; this.cancel(); this.emit('update'); }
  stop(): void { this.playing = false; this.cancel(); this.idx = 0; this.finished = false; this.emit('update'); }
  seek(to: number): void {
    const max = Math.max(0, this.session.total() - 1);
    this.idx = Math.max(0, Math.min(max, to));
    this.finished = false;
    if (this.playing) this.restart(this.idx);
    else { this.cancel(); this.emit('update'); }
  }
  next(): void { this.seek(this.idx + 1); }
  prev(): void { this.seek(this.idx - 1); }
  setSpeed(value: number): void {
    this.speed = Math.max(0.5, Math.min(2.5, Math.round(value * 100) / 100));
    if (this.playing) this.restart(this.idx);
    else this.emit('update');
  }
  dispose(): void { this.playing = false; this.cancel(); }

  /** Abort the current run: cancel in-flight Piper synth and kill afplay. */
  private cancel(): void {
    this.abort?.abort();
    this.kill();
  }
  private kill(): void {
    if (this.afproc) {
      try { this.afproc.kill('SIGTERM'); } catch { /* ignore */ }
      this.afproc = undefined;
    }
  }

  /** Cancel the current run and queue a fresh one strictly after it unwinds. */
  private restart(from: number): void {
    this.cancel();
    const id = ++this.runId;
    const ac = new AbortController();
    this.abort = ac;
    const prev = this.runDone;
    this.runDone = (async () => {
      await prev.catch(() => {}); // never start a new run until the old one is done
      if (id !== this.runId || !this.playing) return;
      await this.loop(id, from, ac.signal);
    })();
  }

  private async loop(id: number, from: number, signal: AbortSignal): Promise<void> {
    this.idx = from;
    while (this.playing && id === this.runId && !signal.aborted) {
      // wait for the sentence we want to stream in (cancellable)
      while (this.idx >= this.session.total() && !this.session.done) {
        await Promise.race([this.session.changed(), whenAborted(signal)]);
        if (id !== this.runId || signal.aborted) return;
      }
      if (this.idx >= this.session.total()) {
        this.finished = true;
        this.playing = false;
        this.emit('update');
        return;
      }
      const sentence = this.session.sentences[this.idx];
      this.emit('update'); // highlight current sentence
      if (signal.aborted || id !== this.runId) return;

      let wav: Buffer;
      try {
        wav = await this.synth(sentence.text, signal); // Piper — no afplay active
      } catch (err) {
        if (signal.aborted || id !== this.runId) return; // cancelled, not a real error
        this.fail('synthesis failed: ' + String((err as any)?.message || err));
        return;
      }
      if (signal.aborted || id !== this.runId || !this.playing) return;

      const ok = await this.play(wav, signal); // afplay — no Piper active
      if (signal.aborted || id !== this.runId) return;
      if (!ok) {
        this.fail('audio playback failed (afplay). Is your system audio working?');
        return;
      }
      this.idx++;
    }
  }

  private fail(message: string): void {
    this.lastError = message;
    this.playing = false;
    this.emit('error', message);
    this.emit('update');
  }

  private async synth(text: string, signal: AbortSignal): Promise<Buffer> {
    const t = text.trim();
    if (!t) return Buffer.alloc(0);
    const key = this.opts.model + '|' + t;
    const cached = this.cache.get(key);
    if (cached) return cached;
    const buf = await this.piper.synth(t, {
      binPath: this.opts.piperBin,
      modelPath: this.opts.model,
      lengthScale: this.opts.lengthScale,
      signal,
    });
    if (buf.length) {
      this.cache.set(key, buf);
      if (this.cache.size > 500) {
        const oldest = this.cache.keys().next().value;
        if (oldest) this.cache.delete(oldest);
      }
    }
    return buf;
  }

  private async play(wav: Buffer, signal: AbortSignal, retry = true): Promise<boolean> {
    if (!wav.length) return true; // nothing to play; treat as success
    // Per-run temp name so a stale run's unlink can never delete a live file.
    const file = path.join(os.tmpdir(), `tech-reader-cli-${process.pid}-${this.runId}-${this.idx}.wav`);
    try {
      await fs.writeFile(file, wav);
    } catch {
      return false;
    }
    const ok = await new Promise<boolean>((resolve) => {
      const proc = spawn('afplay', ['-r', String(this.speed), file]);
      this.afproc = proc;
      proc.on('error', () => resolve(false));
      proc.on('close', (code) => {
        if (this.afproc === proc) this.afproc = undefined;
        resolve(code === 0);
      });
    });
    await fs.unlink(file).catch(() => {});
    // The flaky-device wedge is intermittent — one retry often succeeds.
    if (!ok && retry && !signal.aborted && this.playing) {
      await new Promise((r) => setTimeout(r, 300));
      if (signal.aborted) return false;
      return this.play(wav, signal, false);
    }
    return ok;
  }
}
