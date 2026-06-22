import * as readline from 'readline';
import type { Session } from './session';
import type { Player } from './player';

const ALT_ON = '\x1b[?1049h';
const ALT_OFF = '\x1b[?1049l';
const HIDE_CURSOR = '\x1b[?25l';
const SHOW_CURSOR = '\x1b[?25h';
const HOME = '\x1b[H';
const CLEAR_EOL = '\x1b[K';
const RESET = '\x1b[0m';

const C = {
  dim: (s: string) => `\x1b[2m${s}${RESET}`,
  bold: (s: string) => `\x1b[1m${s}${RESET}`,
  cyan: (s: string) => `\x1b[36m${s}${RESET}`,
  current: (s: string) => `\x1b[1;96m${s}${RESET}`, // bold bright cyan
  faint: (s: string) => `\x1b[90m${s}${RESET}`,
  warn: (s: string) => `\x1b[33m${s}${RESET}`,
};

interface WLine {
  text: string;
  idx: number; // sentence index this line belongs to (-1 = spacer)
}

/** Full-screen terminal reader: header, scrolling narration, controls footer. */
export class Tui {
  private following = true;
  private manualTop = 0;
  private renderQueued = false;
  private disposed = false;

  constructor(
    private readonly session: Session,
    private readonly player: Player,
    private readonly onQuit: () => void,
  ) {}

  start(): void {
    process.stdout.write(ALT_ON + HIDE_CURSOR);
    if (process.stdin.isTTY) process.stdin.setRawMode(true);
    readline.emitKeypressEvents(process.stdin);
    process.stdin.resume();
    process.stdin.on('keypress', this.onKey);
    process.stdout.on('resize', this.schedule);
    this.session.on('update', this.schedule);
    this.player.on('update', this.schedule);
    this.render();
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    process.stdin.off('keypress', this.onKey);
    process.stdout.off('resize', this.schedule);
    this.session.off('update', this.schedule);
    this.player.off('update', this.schedule);
    if (process.stdin.isTTY) process.stdin.setRawMode(false);
    process.stdin.pause();
    process.stdout.write(SHOW_CURSOR + ALT_OFF);
  }

  private schedule = (): void => {
    if (this.renderQueued || this.disposed) return;
    this.renderQueued = true;
    setTimeout(() => { this.renderQueued = false; this.render(); }, 16);
  };

  private onKey = (_str: string, key: readline.Key | undefined): void => {
    if (!key) return;
    const name = key.name;
    if ((key.ctrl && name === 'c') || name === 'q' || name === 'escape') { this.onQuit(); return; }
    switch (name) {
      case 'space': this.player.togglePause(); this.following = true; break;
      case 'right': case 'l': case 'n': this.following = true; this.player.next(); break;
      case 'left': case 'h': case 'p': this.following = true; this.player.prev(); break;
      case 'up': case 'k': this.following = false; this.manualTop = Math.max(0, this.manualTop - 1); this.schedule(); break;
      case 'down': case 'j': this.following = false; this.manualTop += 1; this.schedule(); break;
      case 'pageup': this.following = false; this.manualTop = Math.max(0, this.manualTop - this.bodyRows()); this.schedule(); break;
      case 'pagedown': this.following = false; this.manualTop += this.bodyRows(); this.schedule(); break;
      case 'f': this.following = true; this.schedule(); break;
      default:
        if (_str === '+' || _str === '=') this.player.setSpeed(this.player.speed + 0.1);
        else if (_str === '-' || _str === '_') this.player.setSpeed(this.player.speed - 0.1);
        else if (_str === '0') { this.following = true; this.player.seek(0); }
    }
  };

  private width(): number { return Math.max(20, process.stdout.columns || 80); }
  private height(): number { return Math.max(6, process.stdout.rows || 24); }
  private bodyRows(): number { return Math.max(1, this.height() - 3); } // header(2) + footer(1)

  private wrap(text: string, w: number): string[] {
    const out: string[] = [];
    for (const para of text.split('\n')) {
      const words = para.split(/\s+/).filter(Boolean);
      let line = '';
      for (const word of words) {
        if (!line) line = word;
        else if (line.length + 1 + word.length <= w) line += ' ' + word;
        else { out.push(line); line = word.length > w ? word.slice(0, w) : word; }
      }
      out.push(line);
    }
    return out.length ? out : [''];
  }

  private buildLines(w: number): WLine[] {
    const lines: WLine[] = [];
    let lastBlock = -1;
    for (const s of this.session.sentences) {
      if (!s) continue;
      if (lastBlock !== -1 && s.blockIndex !== lastBlock) lines.push({ text: '', idx: -1 });
      lastBlock = s.blockIndex;
      for (const wl of this.wrap(s.text, w - 2)) lines.push({ text: wl, idx: s.idx });
    }
    return lines;
  }

  private render(): void {
    if (this.disposed) return;
    const w = this.width();
    const rows = this.height();
    const bodyRows = this.bodyRows();
    const lines = this.buildLines(w);

    // scroll: follow current sentence (keep it ~1/3 down), or honor manual offset
    let top = this.manualTop;
    if (this.following) {
      const first = lines.findIndex((l) => l.idx === this.player.idx);
      if (first >= 0) top = Math.max(0, first - Math.floor(bodyRows / 3));
    }
    top = Math.max(0, Math.min(top, Math.max(0, lines.length - bodyRows)));
    this.manualTop = top; // persist the bounded offset so over-scroll can't wedge the view

    const out: string[] = [];
    out.push(this.headerLine(w));
    out.push(C.faint('─'.repeat(w)));
    for (let r = 0; r < bodyRows; r++) {
      const l = lines[top + r];
      out.push(this.bodyLine(l, w));
    }
    out.push(this.footerLine(w));

    // paint — clip every line to the width so header/footer can't soft-wrap and
    // shift the frame on narrow terminals
    let frame = HOME;
    for (let i = 0; i < rows; i++) {
      frame += clip(out[i] ?? '', w) + CLEAR_EOL;
      if (i < rows - 1) frame += '\r\n';
    }
    process.stdout.write(frame);
  }

  private headerLine(w: number): string {
    const total = this.session.total();
    const pos = total ? `${Math.min(this.player.idx + 1, total)}/${total}` : '—';
    const icon = this.player.finished ? '■' : this.player.playing ? '▶' : '⏸';
    const speed = this.player.speed.toFixed(2).replace(/\.?0+$/, '') + '×';
    const st = this.session.status;
    let status = '';
    if (this.player.lastError) status = C.warn(this.player.lastError);
    else if (st.state === 'thinking') status = C.dim(st.message || 'Thinking…');
    else if (st.state === 'fallback') status = C.warn(st.message || 'Offline (humanizer)');
    else if (st.state === 'streaming') status = C.dim('Explaining…');
    const left = `${C.bold(this.session.title || 'Tech Reader')}  ${C.cyan(icon)} ${C.dim(pos)} ${C.dim(speed)}`;
    const pad = Math.max(1, w - visibleLen(left) - visibleLen(status));
    return left + ' '.repeat(pad) + status;
  }

  private bodyLine(l: WLine | undefined, w: number): string {
    if (!l || l.idx === -1) return '';
    const text = l.text;
    if (l.idx === this.player.idx) return '  ' + C.current(text);
    if (l.idx < this.player.idx) return '  ' + C.dim(text);
    return '  ' + text;
  }

  private footerLine(w: number): string {
    return C.faint('space pause · ←/→ prev/next · +/- speed · ↑/↓ scroll · q quit');
  }
}

// length of a string ignoring ANSI escape sequences
function visibleLen(s: string): number {
  return s.replace(/\x1b\[[0-9;]*m/g, '').length;
}

// truncate to `w` visible columns, copying ANSI escapes through; append RESET if cut
function clip(s: string, w: number): string {
  let out = '';
  let vis = 0;
  let i = 0;
  while (i < s.length) {
    if (s[i] === '\x1b') {
      const m = /^\x1b\[[0-9;]*m/.exec(s.slice(i));
      if (m) { out += m[0]; i += m[0].length; continue; }
    }
    if (vis >= w) return out + RESET;
    out += s[i];
    vis++;
    i++;
  }
  return out;
}
