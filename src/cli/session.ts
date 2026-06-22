import { EventEmitter } from 'events';
import type { Sentence, StatusState } from '../types';

export interface SessionStatus {
  state: StatusState | 'idle';
  message?: string;
}

/**
 * Shared buffer between the narrator (which streams sentences in) and the
 * player/TUI (which consume them). Sentences arrive in index order. Consumers
 * await `changed()` to block until the next sentence (or completion) arrives.
 */
export class Session extends EventEmitter {
  readonly sentences: Sentence[] = [];
  done = false;
  status: SessionStatus = { state: 'idle' };
  title = '';

  private waiters: Array<() => void> = [];

  push(s: Sentence): void {
    this.sentences[s.idx] = s;
    this.wake();
    this.emit('update');
  }

  setStatus(state: SessionStatus['state'], message?: string): void {
    this.status = { state, message };
    this.emit('update');
  }

  markDone(): void {
    this.done = true;
    this.wake();
    this.emit('update');
  }

  total(): number {
    return this.sentences.length;
  }

  /** Resolves when a sentence is pushed or the narration completes. */
  changed(): Promise<void> {
    return new Promise((resolve) => this.waiters.push(resolve));
  }

  private wake(): void {
    const pending = this.waiters;
    this.waiters = [];
    for (const resolve of pending) resolve();
  }
}
