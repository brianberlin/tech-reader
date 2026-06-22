import * as vscode from 'vscode';
import { spawn, type ChildProcess } from 'child_process';
import { promises as fsp } from 'fs';
import * as os from 'os';
import * as path from 'path';
import type { Mode, NarrationSettings, Sentence, StatusState } from '../types';
import { segmentBlocks } from '../blocks';
import { narrate } from '../narrate/narrator';
import { OllamaError, type OllamaConfig } from '../ollama/client';
import { SayEngine } from '../engines/say';
import { PiperEngine } from '../engines/piper';
import { log } from '../log';

export interface OpenRequest {
  source: string;
  /** 1-based document line of source[0] */
  baseLine: number;
  lang: string;
  docUri: vscode.Uri;
  /** stable key for resume persistence ('' = don't persist, e.g. selections) */
  docKey: string;
  title: string;
}

const PREFS_KEY = 'techReader.readerPrefs';
const POSITIONS_KEY = 'techReader.positions';
const MAX_POSITIONS = 24;

interface StoredPosition { idx: number; total: number; ts: number; }

function nonce(): string {
  const chars = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789';
  let s = '';
  for (let i = 0; i < 32; i++) s += chars[Math.floor(Math.random() * chars.length)];
  return s;
}

/**
 * Owns the reader webview. The webview speaks with the OS Web Speech API; this
 * host segments the active document into blocks, runs the narrator (local Ollama
 * with an offline humanizer fallback), and streams the resulting sentences to the
 * webview as they are produced.
 */
export class PlayerPanel {
  static current: PlayerPanel | undefined;
  private static readonly viewType = 'techReader.reader';
  private static status: vscode.StatusBarItem | undefined;

  private readonly panel: vscode.WebviewPanel;
  private readonly context: vscode.ExtensionContext;
  private readonly extensionUri: vscode.Uri;
  private disposables: vscode.Disposable[] = [];
  private disposed = false;

  private sessionId = 0;
  private abort: AbortController | undefined;
  private lastRequest: OpenRequest | undefined;
  private docTitle = '';
  private webviewReady = false;

  private readonly say = new SayEngine();
  private readonly piper = new PiperEngine();
  private synthVoiceId = ''; // say voice name OR piper model path, per audioEngine
  private audioCache = new Map<string, ArrayBuffer>(); // key: engine|voice|text → wav bytes

  // host playback (afplay) — short-lived per-sentence process, like the `say` CLI
  private afplay: ChildProcess | undefined;
  private hostPlayId = -1;

  static show(context: vscode.ExtensionContext): PlayerPanel {
    const column = vscode.ViewColumn.Beside;
    if (PlayerPanel.current) {
      PlayerPanel.current.panel.reveal(column, true);
      return PlayerPanel.current;
    }
    const panel = vscode.window.createWebviewPanel(
      PlayerPanel.viewType,
      'Tech Reader',
      { viewColumn: column, preserveFocus: true },
      {
        enableScripts: true,
        retainContextWhenHidden: true,
        localResourceRoots: [vscode.Uri.joinPath(context.extensionUri, 'media')],
      }
    );
    PlayerPanel.current = new PlayerPanel(panel, context);
    return PlayerPanel.current;
  }

  private constructor(panel: vscode.WebviewPanel, context: vscode.ExtensionContext) {
    this.panel = panel;
    this.context = context;
    this.extensionUri = context.extensionUri;
    this.panel.webview.html = this.getHtml(this.panel.webview);
    this.panel.onDidDispose(() => this.dispose(), null, this.disposables);
    this.panel.webview.onDidReceiveMessage((m) => this.onMessage(m), null, this.disposables);
  }

  // ---- config helpers -------------------------------------------------------

  private cfg() {
    return vscode.workspace.getConfiguration('techReader');
  }

  private ollamaConfig(): OllamaConfig {
    const c = this.cfg();
    return {
      baseUrl: c.get<string>('ollama.baseUrl', 'http://localhost:11434'),
      model: c.get<string>('ollama.model', 'llama3.2'),
      temperature: c.get<number>('ollama.temperature', 0.3),
      keepAlive: c.get<string>('ollama.keepAlive', '5m'),
      idleTimeoutMs: c.get<number>('ollama.idleTimeoutMs', 60000),
    };
  }

  private narrationSettings(): NarrationSettings {
    const c = this.cfg();
    return {
      codeHandling: c.get<'explain' | 'literal' | 'skip'>('codeHandling', 'explain'),
      tables: c.get<'summarize' | 'read' | 'skip'>('tables', 'summarize'),
      announceHeadings: c.get<boolean>('announceHeadings', false),
    };
  }

  // ---- public API -----------------------------------------------------------

  open(req: OpenRequest) {
    this.lastRequest = req;
    this.docTitle = req.title;
    this.panel.title = req.title;
    this.panel.reveal(vscode.ViewColumn.Beside, true);
    // The webview buffers messages only after it signals 'ready'. Posting a
    // session before then is lost AND would double-fire when 'ready' re-opens, so
    // defer the first session to the 'ready' handler. An already-ready panel
    // (subsequent reads) starts immediately.
    if (this.webviewReady) void this.startSession(req);
    else log('open: deferring session until webview is ready');
  }

  private async startSession(req: OpenRequest) {
    // session bookkeeping synchronously, before any await, so a rapid re-open
    // supersedes this one deterministically
    this.hostStopPlayback();
    this.abort?.abort();
    const myId = ++this.sessionId;
    const ac = new AbortController();
    this.abort = ac;

    const c = this.cfg();
    const mode = c.get<Mode>('mode', 'ai');
    const blocks = segmentBlocks(req.source, req.lang, req.baseLine);
    const oc = this.ollamaConfig();

    const engine = c.get<string>('audioEngine', 'webspeech');
    let hostVoices: Array<{ name: string; id: string; locale?: string }> = [];
    let hostVoiceId = '';
    let hostAudioReady = true;
    if (engine === 'piper') {
      hostVoices = (await this.piper.listVoices(c.get<string>('piper.voicesDir', '~/.local/share/piper-voices'))).map((v) => ({ name: v.name, id: v.id }));
      if (myId !== this.sessionId) return;
      hostVoiceId = c.get<string>('piper.model', '') || hostVoices[0]?.id || '';
      hostAudioReady = this.piper.available(c.get<string>('piper.binPath', '')) && !!hostVoiceId;
    } else if (engine === 'say') {
      const sv = await this.say.listVoices();
      if (myId !== this.sessionId) return;
      hostVoices = sv.map((v) => ({ name: v.name, id: v.name, locale: v.locale }));
      hostVoiceId = c.get<string>('sayVoice', '') || (await this.say.defaultVoice());
      hostAudioReady = this.say.available;
    }
    if (myId !== this.sessionId) return;
    this.synthVoiceId = hostVoiceId;

    log(`session: "${req.title}" lang=${req.lang || '(none)'} mode=${mode} blocks=${blocks.length} ollama=${oc.baseUrl} model=${oc.model} engine=${engine} voice=${hostVoiceId || '(n/a)'} hostAudioReady=${hostAudioReady}`);

    const resume = req.docKey ? this.positions()[req.docKey] : undefined;
    this.post({
      type: 'session',
      sessionId: myId,
      title: req.title,
      docKey: req.docKey,
      mode,
      lang: req.lang,
      source: req.source,
      rate: c.get<number>('speed', 1),
      volume: c.get<number>('volume', 1),
      engine,
      hostVoices,
      hostVoiceId,
      hostAudioReady,
      hostPlayback: c.get<boolean>('hostPlayback', false),
      voiceURI: c.get<string>('voiceURI', ''),
      settings: {
        highlight: c.get<boolean>('highlightWhileReading', true),
        codeHandling: c.get<string>('codeHandling', 'explain'),
        announceHeadings: c.get<boolean>('announceHeadings', false),
        mode,
      },
      prefs: this.context.globalState.get(PREFS_KEY) ?? null,
      resume: resume ? { idx: resume.idx, total: resume.total } : null,
      ollamaModel: this.ollamaConfig().model,
    });
    this.updateStatus(false);

    void this.runNarration(blocks, mode, myId, ac.signal);
  }

  /** Synthesize one sentence's audio on demand (webview asks per sentence). */
  /** Synthesize text to WAV bytes with the active host engine, cached. Throws on engine failure. */
  private async synthWav(clean: string): Promise<Buffer> {
    const c = this.cfg();
    const engine = c.get<string>('audioEngine', 'webspeech');
    const voice = this.synthVoiceId;
    const key = engine + '|' + voice + '|' + clean;
    const cached = this.audioCache.get(key);
    if (cached) return Buffer.from(new Uint8Array(cached));
    const buf =
      engine === 'piper'
        ? await this.piper.synth(clean, { binPath: c.get<string>('piper.binPath', ''), modelPath: voice, lengthScale: c.get<number>('piper.lengthScale', 1) })
        : await this.say.synth(clean, voice);
    if (buf.length) {
      const ab = buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength) as ArrayBuffer;
      this.audioCache.set(key, ab);
      if (this.audioCache.size > 400) {
        const oldest = this.audioCache.keys().next().value;
        if (oldest) this.audioCache.delete(oldest);
      }
    }
    return buf;
  }

  /** Webview-playback path: synthesize and send bytes to the <audio> element. */
  private async provideAudio(id: number, gen: number, text: string) {
    const clean = text.trim();
    if (!clean) { this.post({ type: 'audioError', id, gen, message: 'empty' }); return; }
    let buf: Buffer;
    try { buf = await this.synthWav(clean); }
    catch (err) { log('synth error: ' + String((err as any)?.message || err)); this.post({ type: 'audioError', id, gen, message: String((err as any)?.message || err) }); return; }
    if (!buf.length) { this.post({ type: 'audioError', id, gen, message: 'no audio produced (check the engine/model)' }); return; }
    const ab = buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength) as ArrayBuffer;
    this.post({ type: 'audio', id, gen, mime: this.say.mime, bytes: ab });
  }

  // ---- host playback (afplay): short-lived per-sentence process, like `say` ---

  private killAfplay() {
    if (this.afplay) { try { this.afplay.kill('SIGTERM'); } catch { /* ignore */ } this.afplay = undefined; }
  }
  private hostStopPlayback() { this.hostPlayId = -1; this.killAfplay(); }

  private async hostPlay(id: number, gen: number, text: string, rate: number, volume: number) {
    this.killAfplay();
    this.hostPlayId = id;
    const clean = text.trim();
    if (!clean) { this.post({ type: 'hostError', id, gen, message: 'empty' }); return; }
    let buf: Buffer;
    try { buf = await this.synthWav(clean); }
    catch (err) { this.post({ type: 'hostError', id, gen, message: String((err as any)?.message || err) }); return; }
    if (this.hostPlayId !== id) return; // superseded while synthesizing
    if (!buf.length) { this.post({ type: 'hostError', id, gen, message: 'no audio produced (check the engine/model)' }); return; }
    const file = path.join(os.tmpdir(), `tech-reader-play-${process.pid}-${id}-${gen}.wav`);
    try { await fsp.writeFile(file, buf); }
    catch (err) { this.post({ type: 'hostError', id, gen, message: String((err as any)?.message || err) }); return; }
    if (this.hostPlayId !== id) { void fsp.unlink(file).catch(() => {}); return; }
    const v = String(Math.max(0, Math.min(2, volume)));
    const r = String(Math.max(0.5, Math.min(2.5, rate)));
    // Run afplay inside the user's GUI launchd domain. VS Code's extension-host
    // child processes otherwise land in a context with no CoreAudio access
    // (afplay → "AudioQueueStart failed") even though system audio works fine.
    // `launchctl asuser <own-uid>` needs no sudo, blocks until done, and forwards
    // exit code + stderr.
    const uid = typeof process.getuid === 'function' ? process.getuid() : -1;
    const proc = uid >= 0
      ? spawn('/bin/launchctl', ['asuser', String(uid), '/usr/bin/afplay', '-v', v, '-r', r, file])
      : spawn('/usr/bin/afplay', ['-v', v, '-r', r, file]);
    this.afplay = proc;
    let stderr = '';
    proc.stderr?.on('data', (d) => { stderr += String(d); });
    this.post({ type: 'hostPlaying', id, gen });
    proc.on('error', (e) => {
      if (this.afplay === proc) this.afplay = undefined;
      void fsp.unlink(file).catch(() => {});
      log('afplay spawn error: ' + String((e as any)?.message || e));
      this.post({ type: 'hostError', id, gen, message: String((e as any)?.message || e) });
    });
    proc.on('close', (code) => {
      if (this.afplay === proc) this.afplay = undefined;
      void fsp.unlink(file).catch(() => {});
      if (this.hostPlayId !== id) return; // paused / seeked / superseded — no advance
      if (code === 0) { this.post({ type: 'hostEnded', id, gen }); return; }
      const msg = stderr.trim() || ('afplay exited ' + code);
      log('afplay failed: ' + msg);
      this.post({ type: 'hostError', id, gen, message: msg });
    });
  }

  private async hostPrefetch(text: string) {
    const clean = text.trim();
    if (clean) { try { await this.synthWav(clean); } catch { /* ignore */ } }
  }

  private async runNarration(blocks: ReturnType<typeof segmentBlocks>, mode: Mode, myId: number, signal: AbortSignal) {
    const c = this.cfg();
    const handlers = {
      onSentence: (s: Sentence) => {
        if (myId !== this.sessionId) return;
        this.post({ type: 'sentence', sessionId: myId, ...s });
      },
      onStatus: (state: StatusState, message?: string) => {
        if (myId !== this.sessionId) return;
        log(`narration ${state}${message ? ' — ' + message : ''}`, 'ollama');
        this.post({ type: 'status', sessionId: myId, state, message });
      },
    };
    try {
      await narrate(
        blocks,
        {
          mode,
          ollama: this.ollamaConfig(),
          settings: this.narrationSettings(),
          proseHandling: c.get<'ai' | 'verbatim'>('proseHandling', 'ai'),
          dictionary: c.get<Record<string, string>>('dictionary', {}),
          locale: c.get<string>('locale', 'en'),
          signal,
        },
        handlers
      );
    } catch (err) {
      if (signal.aborted || (err instanceof OllamaError && err.code === 'aborted')) return;
      log('narration error: ' + String((err as any)?.message || err), 'ollama');
      if (myId === this.sessionId) {
        this.post({ type: 'status', sessionId: myId, state: 'error', message: String((err as any)?.message || err) });
      }
    }
  }

  control(action: 'playpause' | 'stop') {
    this.post({ type: 'control', action });
  }

  toggleMode() {
    const c = this.cfg();
    const next: Mode = c.get<Mode>('mode', 'ai') === 'ai' ? 'literal' : 'ai';
    void c.update('mode', next, vscode.ConfigurationTarget.Global).then(() => {
      if (this.lastRequest) this.open(this.lastRequest);
    });
  }

  // ---- messages from the webview -------------------------------------------

  private async onMessage(m: any) {
    switch (m?.type) {
      case 'ready':
        log('webview ready');
        this.webviewReady = true;
        if (this.lastRequest) void this.startSession(this.lastRequest);
        break;
      case 'synthAudio':
        await this.provideAudio(Number(m.id), Number(m.gen) || 0, String(m.text || ''));
        break;
      case 'hostPlay':
        await this.hostPlay(Number(m.id), Number(m.gen) || 0, String(m.text || ''), Number(m.rate) || 1, m.volume == null ? 1 : Number(m.volume));
        break;
      case 'hostPrefetch':
        void this.hostPrefetch(String(m.text || ''));
        break;
      case 'hostStop':
        this.hostStopPlayback();
        break;
      case 'setVoice': // host-engine voice (say name or piper model path)
        if (typeof m.voice === 'string' && m.voice) {
          this.synthVoiceId = m.voice;
          const eng = this.cfg().get<string>('audioEngine', 'webspeech');
          await this.cfg().update(eng === 'piper' ? 'piper.model' : 'sayVoice', m.voice, vscode.ConfigurationTarget.Global);
        }
        break;
      case 'persistVoice': // web-speech voice (voiceURI), webview-managed
        if (typeof m.voiceURI === 'string') await this.cfg().update('voiceURI', m.voiceURI, vscode.ConfigurationTarget.Global);
        break;
      case 'log':
        log(String(m.message || ''), 'webview');
        break;
      case 'playState':
        this.updateStatus(!!m.playing);
        break;
      case 'position':
        this.savePosition(m);
        break;
      case 'setMode':
        if (m.mode === 'ai' || m.mode === 'literal') {
          await this.cfg().update('mode', m.mode, vscode.ConfigurationTarget.Global);
          if (this.lastRequest) this.open(this.lastRequest);
        }
        break;
      case 'openSource':
        await this.revealSource(Number(m.line) || 1);
        break;
      case 'persistPrefs':
        if (m.prefs && typeof m.prefs === 'object') void this.context.globalState.update(PREFS_KEY, m.prefs);
        break;
      case 'persistSpeed':
        await this.persistNumber('speed', m.value, 0.5, 2.5);
        break;
      case 'persistVolume':
        await this.persistNumber('volume', m.value, 0, 1);
        break;
    }
  }

  private async persistNumber(key: string, value: unknown, min: number, max: number) {
    const n = Number(value);
    if (!Number.isFinite(n)) return;
    await this.cfg().update(key, Math.min(max, Math.max(min, n)), vscode.ConfigurationTarget.Global);
  }

  // ---- resume positions -----------------------------------------------------

  private positions(): Record<string, StoredPosition> {
    return this.context.workspaceState.get<Record<string, StoredPosition>>(POSITIONS_KEY, {});
  }

  private savePosition(m: any) {
    const key = typeof m.docKey === 'string' ? m.docKey : '';
    if (!key) return;
    const all = { ...this.positions() };
    all[key] = { idx: Number(m.idx) || 0, total: Number(m.total) || 0, ts: Date.now() };
    const keys = Object.keys(all);
    if (keys.length > MAX_POSITIONS) {
      keys.sort((a, b) => all[a].ts - all[b].ts);
      for (const k of keys.slice(0, keys.length - MAX_POSITIONS)) delete all[k];
    }
    void this.context.workspaceState.update(POSITIONS_KEY, all);
  }

  // ---- status bar -----------------------------------------------------------

  private updateStatus(playing: boolean) {
    if (!PlayerPanel.status) {
      PlayerPanel.status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 100);
      PlayerPanel.status.command = 'techReader.togglePlayPause';
      this.context.subscriptions.push(PlayerPanel.status);
    }
    const t = this.docTitle.length > 24 ? this.docTitle.slice(0, 23) + '…' : this.docTitle;
    PlayerPanel.status.text = `${playing ? '$(debug-pause)' : '$(play)'} ${t}`;
    PlayerPanel.status.tooltip = 'Tech Reader — click to play/pause';
    PlayerPanel.status.show();
    this.panel.title = playing ? `▶ ${this.docTitle}` : this.docTitle;
  }

  // ---- jump to source -------------------------------------------------------

  private async revealSource(line: number) {
    if (!this.lastRequest) return;
    try {
      const doc = await vscode.workspace.openTextDocument(this.lastRequest.docUri);
      const ln = Math.max(0, Math.min(doc.lineCount - 1, line - 1));
      const editor = await vscode.window.showTextDocument(doc, { viewColumn: vscode.ViewColumn.One, preserveFocus: false });
      const range = doc.lineAt(ln).range;
      editor.selection = new vscode.Selection(range.start, range.start);
      editor.revealRange(range, vscode.TextEditorRevealType.InCenter);
    } catch {
      /* document may be gone */
    }
  }

  // ---- html -----------------------------------------------------------------

  private post(message: any) {
    if (this.disposed) return;
    void this.panel.webview.postMessage(message);
  }

  private getHtml(webview: vscode.Webview): string {
    const n = nonce();
    const js = webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri, 'media', 'reader.js'));
    const css = webview.asWebviewUri(vscode.Uri.joinPath(this.extensionUri, 'media', 'reader.css'));
    const csp = [
      `default-src 'none'`,
      `media-src blob: data:`,
      `img-src ${webview.cspSource} https: data:`,
      `style-src ${webview.cspSource} 'unsafe-inline'`,
      `script-src 'nonce-${n}'`,
      `font-src ${webview.cspSource}`,
    ].join('; ');

    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8" />
<meta http-equiv="Content-Security-Policy" content="${csp}" />
<meta name="viewport" content="width=device-width, initial-scale=1.0" />
<link href="${css}" rel="stylesheet" />
<title>Tech Reader</title>
</head>
<body class="hide-badges">
  <div id="app"></div>
  <div id="sr" class="sr-only" role="status" aria-live="polite"></div>
  <script nonce="${n}" src="${js}"></script>
</body>
</html>`;
  }

  dispose() {
    if (this.disposed) return;
    this.disposed = true;
    this.hostStopPlayback();
    this.abort?.abort();
    PlayerPanel.current = undefined;
    PlayerPanel.status?.hide();
    this.panel.dispose();
    while (this.disposables.length) this.disposables.pop()?.dispose();
  }
}
