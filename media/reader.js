/* Tech Reader — reader webview.
   Two audio engines, chosen by the techReader.audioEngine setting:
     • 'webspeech' (default): the Web Speech API with your OS voices, spoken
       directly in the webview. Most natural.
     • 'say': macOS `say` rendered on the host, streamed here as WAV and played
       through an <audio> element. Fallback if Web Speech is silent.
   Sentences stream in from the host and are displayed exactly as spoken and
   highlighted as they play. UI chrome adapted from markdown-read-aloud (MIT). */
(function () {
  const vscode = acquireVsCodeApi();
  const RM = window.matchMedia('(prefers-reduced-motion: reduce)');

  const FONTS = [
    { key: 'serif', name: 'Literata', sub: 'warm serif' },
    { key: 'sans', name: 'Inter', sub: 'clean sans' },
    { key: 'a11y', name: 'Atkinson Hyperlegible', sub: 'max legibility' },
    { key: 'mono', name: 'IBM Plex Mono', sub: 'calm, technical' },
  ];
  const THEMES = ['auto', 'study', 'daylight', 'paper'];
  const COMFORTS = ['compact', 'cozy', 'wide'];
  const SPEED_PRESETS = [0.75, 1, 1.25, 1.5, 2];
  const PREFETCH = 2; // (say engine) sentences synthesized ahead of the one playing

  const state = {
    idx: 0, playing: false, ended: false, rate: 1, volume: 1, lastVol: 1, muted: false,
    mode: 'ai', engine: 'webspeech', hostPlayback: false, following: true,
    font: 'serif', theme: 'auto', comfort: 'cozy', ambient: false,
    settings: { highlight: true, codeHandling: 'explain', announceHeadings: false, mode: 'ai' },
    sleep: null, docKey: '', ollamaModel: '',
  };
  applyPrefs(vscode.getState());
  function applyPrefs(p) {
    if (!p) return;
    if (FONTS.some((f) => f.key === p.font)) state.font = p.font;
    if (THEMES.includes(p.theme)) state.theme = p.theme;
    if (COMFORTS.includes(p.comfort)) state.comfort = p.comfort;
    if (typeof p.ambient === 'boolean') state.ambient = p.ambient;
  }
  function persistPrefs() {
    const p = { font: state.font, theme: state.theme, comfort: state.comfort, ambient: state.ambient };
    vscode.setState(p);
    post({ type: 'persistPrefs', prefs: p });
  }

  let sessionId = -1;
  let SEGMENTS = [];        // SEGMENTS[idx] = { idx, el, text, kind, startLine }
  let SECTIONS = [];        // [{idx, title}] for progress ticks
  let wordsPrefix = [0];    // cumulative word counts for the ETA
  let narrationDone = false;
  let autoplayPending = false;
  let pendingResume = null; // {idx,total}
  let curBlockIndex = -1, curParaEl = null;
  let rovingEl = null;
  let waitingFor = -1;      // sentence idx we're blocked on (streaming edge / pending audio)

  /* say engine */
  const audio = new Audio();
  audio.preservesPitch = true;
  const urlCache = new Map();   // id -> blob URL
  const requested = new Set();  // ids requested but not yet received
  let audioGen = 0;             // bump to drop stale audio (voice change / new session)
  let HOST_VOICES = [], hostVoiceId = '', hostAudioReady = true;

  /* web-speech engine */
  let WS_VOICES = [], wsVoice = null, wsWantURI = '';
  const liveUtter = new Set(); // hold refs so Chromium can't GC mid-speech
  let everSpoke = false, speechWatch = 0;

  /* ---------------- icons ---------------- */
  const I = {
    prev: '<svg viewBox="0 0 24 24"><path d="M6 5v14h2V5H6zm3 7l9 7V5l-9 7z"/></svg>',
    play: '<svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg>',
    pause: '<svg viewBox="0 0 24 24"><rect x="6" y="5" width="4" height="14" rx="1.3"/><rect x="14" y="5" width="4" height="14" rx="1.3"/></svg>',
    next: '<svg viewBox="0 0 24 24"><path d="M16 5v14h2V5h-2zM15 12L6 5v14l9-7z"/></svg>',
    moon: '<svg viewBox="0 0 24 24"><path d="M12 3a9 9 0 109 9c0-.46-.04-.92-.1-1.36A5.5 5.5 0 0112.36 3.1 9.05 9.05 0 0012 3z"/></svg>',
    sun: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><circle cx="12" cy="12" r="4"/><path d="M12 2v2M12 20v2M2 12h2M20 12h2M5 5l1.4 1.4M17.6 17.6L19 19M19 5l-1.4 1.4M6.4 17.6L5 19"/></svg>',
    paper: '<svg viewBox="0 0 24 24"><path d="M6 3h9l5 5v13H6zM14 4v5h5"/></svg>',
    auto: '<svg viewBox="0 0 24 24"><path d="M12 3a9 9 0 100 18V3z"/><circle cx="12" cy="12" r="9" fill="none" stroke="currentColor" stroke-width="1.6"/></svg>',
    gear: '<svg viewBox="0 0 24 24"><path d="M19.14 12.94a7.49 7.49 0 000-1.88l2.03-1.58a.5.5 0 00.12-.64l-1.92-3.32a.5.5 0 00-.6-.22l-2.39.96a7.3 7.3 0 00-1.62-.94l-.36-2.54a.5.5 0 00-.5-.42h-3.84a.5.5 0 00-.5.42l-.36 2.54c-.58.24-1.12.55-1.62.94l-2.39-.96a.5.5 0 00-.6.22L2.71 8.84a.5.5 0 00.12.64l2.03 1.58a7.49 7.49 0 000 1.88l-2.03 1.58a.5.5 0 00-.12.64l1.92 3.32c.13.22.39.31.6.22l2.39-.96c.5.39 1.04.7 1.62.94l.36 2.54c.04.24.25.42.5.42h3.84c.25 0 .46-.18.5-.42l.36-2.54c.58-.24 1.12-.55 1.62-.94l2.39.96c.21.09.47 0 .6-.22l1.92-3.32a.5.5 0 00-.12-.64l-2.03-1.58zM12 15.5A3.5 3.5 0 1112 8.5a3.5 3.5 0 010 7z"/></svg>',
    down: '<svg viewBox="0 0 24 24"><path d="M12 16l-6-6h12z"/></svg>',
    edit: '<svg viewBox="0 0 24 24"><path d="M3 17.25V21h3.75L17.81 9.94l-3.75-3.75L3 17.25zM20.71 7.04a1 1 0 000-1.41l-2.34-2.34a1 1 0 00-1.41 0l-1.83 1.83 3.75 3.75 1.83-1.83z"/></svg>',
  };
  const SPK = '<svg viewBox="0 0 24 24"><path d="M4 9v6h3.6L13 20V4L7.6 9H4z"/><path d="M16 8.6a4 4 0 010 6.8M18.4 6a7 7 0 010 12" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/></svg>';
  const SPK_MUTE = '<svg viewBox="0 0 24 24"><path d="M4 9v6h3.6L13 20V4L7.6 9H4z"/><path d="M16.5 9.5l5 5M21.5 9.5l-5 5" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/></svg>';

  const esc = (s) => String(s).replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');
  const $ = (id) => document.getElementById(id);
  const post = (m) => vscode.postMessage(m);
  const logHost = (m) => { try { post({ type: 'log', message: String(m) }); } catch (_) {} };
  const fmtRate = (r) => (Math.round(r * 100) / 100).toFixed(2).replace(/(\.\d)0$/, '$1') + '×';
  const clampRate = (r) => Math.min(2.5, Math.max(0.5, r));

  /* ---------------- chrome ---------------- */
  function buildChrome() {
    $('app').innerHTML = `
      <header id="bar"><div class="bar-row">
        <div class="controls">
          <button class="btn" data-act="prev" title="Previous sentence (←)" aria-label="Previous sentence">${I.prev}</button>
          <button class="btn play" data-act="toggle" aria-pressed="false" aria-label="Play" title="Play (Space)">${I.play}</button>
          <button class="btn" data-act="next" title="Next sentence (→)" aria-label="Next sentence">${I.next}</button>
        </div>
        <div class="title-block">
          <div class="doc-title" id="doctitle">Tech Reader</div>
          <div class="meta"><span id="pos"></span><span class="dot"></span><span id="eta"></span><span class="dot"></span><span id="status">Ready</span></div>
        </div>
        <div class="seg-ctl" id="mode" title="AI explanation vs. literal reading" style="width:max-content">
          <button data-mode="ai" aria-pressed="true">AI</button><button data-mode="literal" aria-pressed="false">Literal</button></div>
        <button id="speed-btn" aria-haspopup="true" aria-expanded="false" title="Speed" aria-label="Speed">1.0×</button>
        <div class="vol"><button class="ico-btn" id="mute" title="Mute" aria-label="Mute">${SPK}</button><input type="range" id="volume" min="0" max="1" step="0.05" value="1" aria-label="Volume"></div>
        <button class="btn" data-act="edit" title="Open in editor" aria-label="Open in editor">${I.edit}</button>
        <button class="btn" data-act="gear" title="More settings" aria-haspopup="true" aria-label="More settings" aria-expanded="false">${I.gear}</button>
        <div class="progress" id="progress" role="slider" tabindex="0" aria-label="Progress" aria-valuemin="1" aria-valuemax="1" aria-valuenow="1"><i id="prog"></i></div>
      </div></header>
      <div id="reader-wrap">
        <article id="reader" data-font="serif" data-comfort="cozy">
          <div id="empty-note">Nothing readable here yet…</div>
          <div id="doc"></div>
        </article>
      </div>
      <div id="vignette" aria-hidden="true"></div>

      <div class="pop" id="pop" role="dialog" aria-label="More settings">
        <div class="grp"><span class="lbl">Voice</span>
          <select id="voice" aria-label="Voice"></select>
          <span class="hint" id="voice-hint">Voices come from your operating system.</span>
        </div>
        <div class="grp"><span class="lbl">Reading</span>
          <span class="lbl" style="text-transform:none;letter-spacing:0">Sleep timer</span>
          <div class="seg-row" id="sleep">
            <button data-sleep="off" class="sel-on">Off</button><button data-sleep="15">15m</button><button data-sleep="30">30m</button><button data-sleep="60">60m</button></div>
          <label class="toggle"><input type="checkbox" id="ambient-t"> Ambient focus</label>
          <span class="hint">Alt+Click a sentence to open its source line.</span>
        </div>
        <div class="grp"><span class="lbl">Reading font</span><div class="font-list" id="font-list"></div></div>
        <div class="grp"><span class="lbl">Theme</span><div class="seg-ctl pop-theme" id="theme-pop" role="group" aria-label="Theme">
          <button data-theme-set="auto" title="Follow VS Code" aria-label="Follow VS Code">${I.auto}</button><button data-theme-set="study" title="Study" aria-label="Study">${I.moon}</button><button data-theme-set="daylight" title="Daylight" aria-label="Daylight">${I.sun}</button><button data-theme-set="paper" title="Paper" aria-label="Paper">${I.paper}</button></div></div>
        <div class="grp"><span class="lbl">Comfort</span><div class="seg-row" id="comfort">
          <button data-comfort="compact">Compact</button><button data-comfort="cozy">Cozy</button><button data-comfort="wide">Wide</button></div></div>
      </div>

      <div class="pop mini" id="spop" role="dialog" aria-label="Speed">
        <div class="slider-row"><input type="range" id="rate" min="0.5" max="2.5" step="0.05" value="1" aria-label="Speed"><span class="val" id="ratev">1.0×</span></div>
        <div class="preset-row" id="speed-presets">${SPEED_PRESETS.map((p) => `<button data-rate="${p}">${fmtRate(p)}</button>`).join('')}</div>
      </div>

      <div class="pill-float" id="follow-pill"><button class="primary" id="follow-btn">${I.down} Back to reading</button></div>
      <div class="pill-float" id="resume-pill"><span class="pill-label">Pick up where you left off?</span><span class="pill-sep"></span><button class="primary" id="resume-btn">Resume</button><button id="startover-btn">Start over</button></div>
      <div id="ptip" aria-hidden="true"></div>`;
    bindChrome();
  }

  /* ================= ENGINES ================= */
  // Each engine implements: play(i), stop(), pause(), resume(), arrived(idx).
  // Shared control (playAt/advance/doPlay/…) calls through the active ENGINE.

  function startAudio(url) {
    audio.src = url;
    audio.playbackRate = clampRate(state.rate);
    audio.volume = state.muted ? 0 : state.volume;
    try { audio.currentTime = 0; } catch (_) {}
    const p = audio.play();
    if (p && p.catch) p.catch((e) => {
      logHost('audio.play() rejected: ' + (e && e.name));
      if (e && e.name === 'NotAllowedError') { setPlaying(false); setStatus('Press Play ▶ to start audio', 'warn'); }
    });
  }
  function requestSynth(i) {
    if (i < 0 || i >= SEGMENTS.length || !SEGMENTS[i]) return;
    if (urlCache.has(i) || requested.has(i)) return;
    requested.add(i);
    post({ type: 'synthAudio', id: i, gen: audioGen, text: SEGMENTS[i].text });
  }
  function clearAudioCache() {
    for (const u of urlCache.values()) URL.revokeObjectURL(u);
    urlCache.clear(); requested.clear();
  }
  const audioElEngine = {
    play(i) {
      const url = urlCache.get(i);
      if (url) { waitingFor = -1; startAudio(url); }
      else { audio.pause(); waitingFor = i; requestSynth(i); }
      for (let k = 1; k <= PREFETCH; k++) requestSynth(i + k);
    },
    stop() { audio.pause(); try { audio.removeAttribute('src'); } catch (_) {} waitingFor = -1; },
    pause() { audio.pause(); },
    resume() {
      if (audio.src && audio.currentTime > 0 && !audio.ended) { const p = audio.play(); if (p && p.catch) p.catch(() => {}); }
      else this.play(state.idx);
    },
    arrived(idx) { if (state.playing && waitingFor === idx) this.play(idx); },
  };

  function armSpeechWatchdog() {
    clearTimeout(speechWatch);
    if (everSpoke) return;
    const at = sessionId;
    speechWatch = setTimeout(() => {
      if (sessionId === at && state.playing && !everSpoke) {
        logHost('WATCHDOG: web-speech produced no onstart in 2.5s');
        setStatus('No audio from Web Speech — try the `say` engine (techReader.audioEngine).', 'warn');
      }
    }, 2500);
  }
  const wsEngine = {
    play(i) {
      const seg = SEGMENTS[i]; if (!seg) return;
      waitingFor = -1;
      const u = new SpeechSynthesisUtterance(seg.text);
      u.rate = clampRate(state.rate);
      u.volume = state.muted ? 0 : state.volume;
      if (wsVoice) { u.voice = wsVoice; u.lang = wsVoice.lang; }
      liveUtter.add(u);
      const mySession = sessionId;
      u.onstart = () => { everSpoke = true; clearTimeout(speechWatch); };
      u.onend = () => { liveUtter.delete(u); if (mySession === sessionId && state.playing && state.idx === i) advance(); };
      u.onerror = (e) => {
        liveUtter.delete(u);
        const err = e && e.error;
        logHost('ws onerror[' + i + '] ' + err);
        if (err && err !== 'interrupted' && err !== 'canceled' && mySession === sessionId) setStatus('Speech error: ' + err, 'warn');
        if (mySession === sessionId && state.playing && state.idx === i) advance();
      };
      logHost('ws speak[' + i + '] voice=' + (wsVoice ? wsVoice.name : '(default)'));
      // avoid the macOS cancel-then-speak wedge: only cancel if busy, then defer
      if (speechSynthesis.speaking || speechSynthesis.pending) {
        speechSynthesis.cancel();
        setTimeout(() => { if (state.playing && state.idx === i) speechSynthesis.speak(u); }, 70);
      } else {
        speechSynthesis.speak(u);
      }
      armSpeechWatchdog();
    },
    stop() { try { speechSynthesis.cancel(); } catch (_) {} },
    pause() { try { speechSynthesis.pause(); } catch (_) {} },
    resume() { if (speechSynthesis.paused && speechSynthesis.speaking) speechSynthesis.resume(); else this.play(state.idx); },
    arrived(idx) { if (state.playing && waitingFor === idx) this.play(idx); },
  };
  function usesHostSynth() { return state.engine === 'say' || state.engine === 'piper'; }
  function usesHostPlayback() { return usesHostSynth() && state.hostPlayback; }
  function usesAudioEl() { return usesHostSynth() && !state.hostPlayback; }
  function ENGINE() {
    if (state.engine === 'webspeech') return wsEngine;
    return state.hostPlayback ? hostEngine : audioElEngine;
  }

  // Host playback: synthesis AND sound happen on the host (afplay), like the `say`
  // CLI. The webview just shows/controls and advances when the host reports a
  // sentence finished. Avoids a persistent webview audio stream wedging CoreAudio.
  const hostEngine = {
    play(i) {
      const seg = SEGMENTS[i]; if (!seg) { waitingFor = i; return; }
      waitingFor = -1;
      logHost('hostPlay[' + i + '] rate=' + clampRate(state.rate) + ' vol=' + (state.muted ? 0 : state.volume));
      // NO prefetch: synthesizing the next sentence with Piper WHILE afplay is
      // playing starves the real-time audio thread and wedges CoreAudio
      // ("AudioQueueStart failed"). Strictly sequential (synth, then play, then
      // synth next) is reliable — at the cost of a short gap between sentences.
      post({ type: 'hostPlay', id: i, gen: audioGen, text: seg.text, rate: clampRate(state.rate), volume: state.muted ? 0 : state.volume });
    },
    stop() { post({ type: 'hostStop' }); },
    pause() { post({ type: 'hostStop' }); },
    resume() { this.play(state.idx); },
    arrived(idx) { if (state.playing && waitingFor === idx) this.play(idx); },
  };

  audio.addEventListener('ended', () => { if (usesAudioEl() && state.playing) advance(); });
  audio.addEventListener('error', () => { if (usesAudioEl() && state.playing && audio.src) { logHost('audio element error → advance'); advance(); } });
  audio.addEventListener('playing', () => { if (usesAudioEl()) { everSpoke = true; logHost('audio playing[' + state.idx + ']'); } });

  /* ---------------- streaming document build ---------------- */
  function resetDoc() {
    ENGINE().stop();
    audio.pause(); try { audio.removeAttribute('src'); } catch (_) {}
    clearAudioCache(); audioGen++;
    clearTimeout(speechWatch); liveUtter.clear();
    SEGMENTS = []; SECTIONS = []; wordsPrefix = [0];
    narrationDone = false; curBlockIndex = -1; curParaEl = null;
    rovingEl = null; waitingFor = -1;
    state.idx = 0; state.ended = false; state.playing = false; state.following = true;
    $('doc').innerHTML = '';
    document.body.classList.add('no-segments');
    hideResumePill();
    setPlayIcon(); updateProgress(); updateFollowPill();
  }

  function appendSentence(s) {
    const isHeading = s.kind === 'heading';
    if (isHeading || s.blockIndex !== curBlockIndex || !curParaEl) {
      curParaEl = document.createElement(isHeading ? 'h2' : 'p');
      if (isHeading) curParaEl.className = 'doc-h';
      if (s.kind === 'code' || s.kind === 'comment') curParaEl.classList.add('from-code');
      $('doc').appendChild(curParaEl);
      curBlockIndex = s.blockIndex;
      if (isHeading) SECTIONS.push({ idx: s.idx, title: s.text.slice(0, 60) });
    }
    const span = document.createElement('span');
    span.className = 'seg';
    span.dataset.seg = String(s.idx);
    if (s.startLine) span.dataset.line = String(s.startLine);
    span.tabIndex = -1;
    span.textContent = s.text + ' ';
    curParaEl.appendChild(span);

    SEGMENTS[s.idx] = { idx: s.idx, el: span, text: s.text, kind: s.kind, startLine: s.startLine || 1 };
    wordsPrefix[s.idx + 1] = wordsPrefix[s.idx] + (s.text.match(/\S+/g) || []).length;
    document.body.classList.toggle('no-segments', SEGMENTS.length === 0);

    if (isHeading) buildTicks();
    updateProgress();

    if (autoplayPending) {
      autoplayPending = false;
      if (pendingResume && pendingResume.idx >= 3) showResumePill();
      logHost('first sentence → autoplay (engine=' + state.engine + ')');
      playAt(0);
    } else if (state.playing && waitingFor === s.idx) {
      ENGINE().arrived(s.idx);
    } else if (state.playing && usesAudioEl()) {
      for (let k = 0; k <= PREFETCH; k++) requestSynth(state.idx + k);
    }
  }

  /* ---------------- playback control (engine-agnostic) ---------------- */
  function setPlaying(p) {
    if (state.playing !== p) {
      state.playing = p;
      post({ type: 'playState', playing: p });
      if (!p) sendPosition(true);
    }
    setPlayIcon();
    updateFollowPill();
  }
  function activate(i) {
    const seg = SEGMENTS[i]; if (!seg) return;
    SEGMENTS.forEach((s) => s && s.el.classList.remove('speaking'));
    if (state.settings.highlight) {
      seg.el.classList.add('speaking');
      if (state.following) scrollToSeg(seg);
    }
    if (rovingEl) rovingEl.tabIndex = -1;
    seg.el.tabIndex = 0; rovingEl = seg.el;
    updateProgress();
    const sr = $('sr'); if (sr) sr.textContent = seg.text;
    sendPosition(false);
  }
  function playAt(i) {
    if (i < 0) i = 0;
    if (!SEGMENTS.length && i > 0) return;
    state.following = true;
    if (i >= SEGMENTS.length || !SEGMENTS[i]) {
      if (narrationDone && i >= SEGMENTS.length) { finishAll(); return; }
      state.idx = i; waitingFor = i; setPlaying(true); return; // streaming edge
    }
    state.idx = i; state.ended = false;
    setPlaying(true); activate(i);
    ENGINE().play(i);
  }
  function advance() { playAt(state.idx + 1); }
  function doPlay() {
    if (!SEGMENTS.length) { autoplayPending = true; return; }
    if (state.ended) { playAt(0); return; }
    setPlaying(true);
    ENGINE().resume();
  }
  function doPause() { setPlaying(false); ENGINE().pause(); }
  function togglePlay() { state.playing ? doPause() : doPlay(); }
  function doStop() {
    setPlaying(false);
    ENGINE().stop();
    waitingFor = -1; state.idx = 0; state.ended = false;
    SEGMENTS.forEach((s) => s && s.el.classList.remove('speaking'));
    disarmSleep(); updateProgress();
  }
  function finishAll() {
    setPlaying(false);
    ENGINE().stop();
    state.ended = true; waitingFor = -1;
    SEGMENTS.forEach((s) => s && s.el.classList.remove('speaking'));
    disarmSleep();
    const pr = $('prog'); if (pr) pr.style.width = '100%';
  }
  function applyAudioRate() { audio.playbackRate = clampRate(state.rate); }
  function applyAudioVolume() { audio.volume = state.muted ? 0 : state.volume; }

  /* ---------------- voices ---------------- */
  function loadWsVoices() {
    WS_VOICES = (speechSynthesis.getVoices() || []).slice();
    if (!WS_VOICES.length) return;
    wsVoice =
      WS_VOICES.find((v) => v.voiceURI === wsWantURI) || wsVoice ||
      WS_VOICES.find((v) => v.default && v.localService) ||
      WS_VOICES.find((v) => v.localService && /^en/i.test(v.lang)) ||
      WS_VOICES.find((v) => /^en/i.test(v.lang)) || WS_VOICES[0] || null;
    if (state.engine === 'webspeech') renderVoiceSelect();
  }
  function renderVoiceSelect() {
    const sel = $('voice'), hint = $('voice-hint'); if (!sel) return;
    if (usesHostSynth()) {
      if (hint) hint.textContent = state.engine === 'piper' ? 'Piper voice models (.onnx).' : 'macOS `say` voices.';
      if (!HOST_VOICES.length) { sel.innerHTML = '<option>' + (state.engine === 'piper' ? 'No Piper models found' : 'System default') + '</option>'; sel.disabled = true; return; }
      sel.disabled = false;
      sel.innerHTML = HOST_VOICES.map((v) => `<option value="${esc(v.id)}">${esc(v.name)}${v.locale ? ' (' + esc(v.locale) + ')' : ''}</option>`).join('');
      if (hostVoiceId) sel.value = hostVoiceId;
    } else {
      if (hint) hint.textContent = 'Web Speech voices from your OS.';
      if (!WS_VOICES.length) { sel.innerHTML = '<option>Loading…</option>'; sel.disabled = true; return; }
      sel.disabled = false;
      sel.innerHTML = WS_VOICES.map((v) => `<option value="${esc(v.voiceURI)}">${esc(v.name)} (${esc(v.lang)})${v.localService ? '' : ' · online'}</option>`).join('');
      if (wsVoice) sel.value = wsVoice.voiceURI;
    }
  }
  function changeVoice(val) {
    if (usesHostSynth()) {
      if (!val || val === hostVoiceId) return;
      hostVoiceId = val;
      post({ type: 'setVoice', voice: val });
      audioGen++; clearAudioCache(); waitingFor = -1;
      if (state.playing) playAt(state.idx); else requestSynth(state.idx);
    } else {
      const v = WS_VOICES.find((x) => x.voiceURI === val);
      if (!v || v === wsVoice) return;
      wsVoice = v; wsWantURI = val;
      post({ type: 'persistVoice', voiceURI: val });
      if (state.playing) playAt(state.idx);
    }
  }

  /* ---------------- progress / eta / ticks ---------------- */
  function buildTicks() {
    const bar = $('progress'); if (!bar) return;
    bar.querySelectorAll('.tick').forEach((t) => t.remove());
    const n = SEGMENTS.length;
    if (n < 2) return;
    for (const s of SECTIONS) {
      if (s.idx <= 0) continue;
      const t = document.createElement('span'); t.className = 'tick';
      t.style.left = (s.idx / (n - 1)) * 100 + '%';
      bar.appendChild(t);
    }
  }
  function updateProgress() {
    const n = SEGMENTS.length;
    const pct = n > 1 ? (state.idx / (n - 1)) * 100 : 0;
    const pr = $('prog'); if (pr && !state.ended) pr.style.width = (n ? pct : 0) + '%';
    const ps = $('pos'); if (ps) ps.textContent = n ? (state.idx + 1) + ' / ' + n : '';
    const bar = $('progress');
    if (bar) {
      bar.setAttribute('aria-valuemax', String(Math.max(1, n)));
      bar.setAttribute('aria-valuenow', String(Math.min(n, state.idx + 1)));
      bar.setAttribute('aria-valuetext', `Sentence ${state.idx + 1} of ${n}`);
    }
    updateEta();
  }
  function updateEta() {
    const e = $('eta'); if (!e) return;
    const n = SEGMENTS.length;
    if (!n) { e.textContent = ''; return; }
    const remaining = (wordsPrefix[n] || 0) - (wordsPrefix[state.idx] || 0);
    const mins = Math.max(1, Math.round(remaining / (180 * state.rate)));
    e.textContent = '~' + mins + ' min';
  }
  let posTimer = 0, lastSentIdx = -1;
  function sendPosition(immediate) {
    if (!SEGMENTS.length || !state.docKey) return;
    const fire = () => {
      posTimer = 0;
      if (state.idx === lastSentIdx) return;
      lastSentIdx = state.idx;
      post({ type: 'position', docKey: state.docKey, idx: state.idx, total: SEGMENTS.length });
    };
    if (immediate) { clearTimeout(posTimer); fire(); }
    else if (!posTimer) posTimer = setTimeout(fire, 1500);
  }
  function setPlayIcon() {
    const b = document.querySelector('.btn.play'); if (!b) return;
    b.innerHTML = state.playing ? I.pause : I.play;
    b.setAttribute('aria-pressed', String(state.playing));
    b.setAttribute('aria-label', state.playing ? 'Pause' : 'Play');
    b.title = (state.playing ? 'Pause' : 'Play') + ' (Space)';
  }
  function scrollToSeg(seg) {
    seg.el.scrollIntoView({ behavior: RM.matches ? 'auto' : 'smooth', block: 'center' });
  }

  /* ---------------- follow / teleprompter ---------------- */
  function detachFollow() { if (state.following) { state.following = false; updateFollowPill(); } }
  function attachFollow() {
    state.following = true; updateFollowPill();
    const seg = SEGMENTS[state.idx]; if (seg && state.playing) scrollToSeg(seg);
  }
  function updateFollowPill() {
    const p = $('follow-pill'); if (!p) return;
    const show = !state.following && state.playing && state.settings.highlight;
    p.classList.toggle('show', show);
    if (show) hideResumePill();
  }
  let scrollRaf = 0;
  function onScroll() {
    const wrap = $('reader-wrap');
    $('bar').classList.toggle('scrolled', wrap.scrollTop > 4);
    if (state.following || !state.playing) return;
    if (scrollRaf) return;
    scrollRaf = requestAnimationFrame(() => {
      scrollRaf = 0;
      const seg = SEGMENTS[state.idx]; if (!seg) return;
      const r = seg.el.getBoundingClientRect();
      const w = wrap.getBoundingClientRect();
      const cy = (r.top + r.bottom) / 2;
      if (cy > w.top + w.height * 0.32 && cy < w.top + w.height * 0.68) attachFollow();
    });
  }

  /* ---------------- resume pill ---------------- */
  let resumeTimer = 0;
  function showResumePill() {
    const p = $('resume-pill'); if (!p) return;
    p.classList.add('show');
    clearTimeout(resumeTimer);
    resumeTimer = setTimeout(hideResumePill, 12000);
  }
  function hideResumePill() {
    const p = $('resume-pill'); if (p) p.classList.remove('show');
    clearTimeout(resumeTimer);
  }
  function resumeFromSaved() {
    hideResumePill();
    if (!pendingResume || !SEGMENTS.length) return;
    playAt(Math.min(SEGMENTS.length - 1, Math.max(0, pendingResume.idx | 0)));
  }

  /* ---------------- sleep timer ---------------- */
  function disarmSleep() {
    if (state.sleep && state.sleep.id) clearTimeout(state.sleep.id);
    state.sleep = null; updateSleepUI('off');
  }
  function armSleep(key) {
    disarmSleep();
    if (key === 'off') return;
    const mins = parseInt(key, 10);
    if (!mins) return;
    const id = setTimeout(() => { doPause(); disarmSleep(); }, mins * 60000);
    state.sleep = { id }; updateSleepUI(key);
  }
  function updateSleepUI(key) {
    document.querySelectorAll('#sleep button').forEach((b) => b.classList.toggle('sel-on', b.dataset.sleep === key));
  }

  /* ---------------- theme / font / comfort ---------------- */
  function applyTheme() {
    if (state.theme === 'auto') document.documentElement.removeAttribute('data-theme');
    else document.documentElement.dataset.theme = state.theme;
    document.querySelectorAll('[data-theme-set]').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.themeSet === state.theme)));
  }
  function setTheme(t) { state.theme = t; applyTheme(); persistPrefs(); }
  function applyFont() {
    $('reader').dataset.font = state.font;
    document.querySelectorAll('.font-opt').forEach((o) => o.classList.toggle('sel', o.dataset.font === state.font));
  }
  function setFont(key) { state.font = key; applyFont(); persistPrefs(); }
  function cycleFont() { const i = FONTS.findIndex((f) => f.key === state.font); setFont(FONTS[(i + 1) % FONTS.length].key); }
  function applyComfort() { $('reader').dataset.comfort = state.comfort; document.querySelectorAll('#comfort button').forEach((b) => b.classList.toggle('sel-on', b.dataset.comfort === state.comfort)); }
  function setComfort(c) { state.comfort = c; applyComfort(); persistPrefs(); }
  function applyAmbient() {
    document.body.classList.toggle('ambient', state.ambient);
    document.body.classList.toggle('focusing', state.ambient);
    const t = $('ambient-t'); if (t) t.checked = state.ambient;
  }
  function buildFontList() {
    const wrap = $('font-list'); if (!wrap) return;
    wrap.innerHTML = FONTS.map((f) => `<button type="button" class="font-opt" data-font="${f.key}"><span class="pv" style="font-family:var(--f-${f.key})">Ag</span><span><span class="nm">${esc(f.name)}</span><br><span class="sub">${esc(f.sub)}</span></span></button>`).join('');
    wrap.querySelectorAll('.font-opt').forEach((o) => o.onclick = () => setFont(o.dataset.font));
  }

  /* ---------------- mode ---------------- */
  function applyModeUI() {
    document.querySelectorAll('#mode button').forEach((b) => {
      const on = b.dataset.mode === state.mode;
      b.classList.toggle('sel-on', on);
      b.setAttribute('aria-pressed', String(on));
    });
  }

  /* ---------------- volume / speed ---------------- */
  function updateMuteUI() {
    const b = $('mute'); const m = state.muted || state.volume === 0;
    if (b) { b.classList.toggle('muted', m); b.innerHTML = m ? SPK_MUTE : SPK; b.setAttribute('aria-label', m ? 'Unmute' : 'Mute'); b.title = m ? 'Unmute' : 'Mute'; }
  }
  function applyVolume() { const v = $('volume'); if (v) v.value = String(state.volume); applyAudioVolume(); updateMuteUI(); }
  function applyRate() {
    const r = $('rate'); if (r) { r.value = String(state.rate); r.setAttribute('aria-valuetext', fmtRate(state.rate)); }
    const rv = $('ratev'); if (rv) rv.textContent = fmtRate(state.rate);
    const sb = $('speed-btn'); if (sb) { sb.textContent = fmtRate(state.rate); sb.classList.toggle('on', Math.abs(state.rate - 1) > 0.001); }
    document.querySelectorAll('#speed-presets button').forEach((b) => b.classList.toggle('sel-on', Math.abs(parseFloat(b.dataset.rate) - state.rate) < 0.001));
    applyAudioRate();
    updateEta();
  }
  let speedPersist = 0;
  function setRate(r, persist) {
    state.rate = clampRate(Math.round(r * 100) / 100);
    applyRate(); // <audio>: live playbackRate; ws: next utterance; host: re-render below
    clearTimeout(speedPersist);
    speedPersist = setTimeout(() => {
      if (usesHostPlayback() && state.playing) ENGINE().play(state.idx); // re-render current at new rate
      if (persist) post({ type: 'persistSpeed', value: state.rate });
    }, 350);
  }

  /* ---------------- popovers ---------------- */
  let openPopInfo = null;
  function positionPop(pop, btn) {
    const r = btn.getBoundingClientRect();
    pop.style.top = Math.round(r.bottom + 8) + 'px';
    pop.style.right = Math.max(8, Math.round(window.innerWidth - r.right)) + 'px';
    pop.style.maxHeight = Math.max(120, window.innerHeight - r.bottom - 20) + 'px';
  }
  function openPop(pop, btn) {
    closePops();
    pop.classList.add('open'); btn.setAttribute('aria-expanded', 'true');
    positionPop(pop, btn); openPopInfo = { pop, btn };
    const f = pop.querySelector('button:not([disabled]), input, select'); if (f) f.focus({ preventScroll: true });
  }
  function closePops(refocus) {
    if (!openPopInfo) return;
    openPopInfo.pop.classList.remove('open');
    openPopInfo.btn.setAttribute('aria-expanded', 'false');
    if (refocus) openPopInfo.btn.focus({ preventScroll: true });
    openPopInfo = null;
  }
  function togglePop(pop, btn) { if (openPopInfo && openPopInfo.pop === pop) closePops(); else openPop(pop, btn); }
  window.addEventListener('resize', () => { if (openPopInfo) positionPop(openPopInfo.pop, openPopInfo.btn); });

  /* ---------------- seek (progress strip) ---------------- */
  function segAtX(clientX) {
    const bar = $('progress'); const r = bar.getBoundingClientRect();
    const ratio = Math.min(1, Math.max(0, (clientX - r.left) / r.width));
    return Math.round(ratio * (SEGMENTS.length - 1));
  }
  function seekTo(i) {
    if (!SEGMENTS.length) return;
    i = Math.min(SEGMENTS.length - 1, Math.max(0, i));
    state.following = true; hideResumePill();
    if (state.playing) playAt(i);
    else { ENGINE().stop(); waitingFor = -1; state.idx = i; state.ended = false; activate(i); if (usesAudioEl()) requestSynth(i); }
  }
  function sectionTitleFor(i) { let t = ''; for (const s of SECTIONS) { if (s.idx <= i) t = s.title; else break; } return t; }
  function bindProgress() {
    const bar = $('progress'), tip = $('ptip');
    let scrubbing = false;
    const showTip = (x) => {
      if (!SEGMENTS.length) return;
      const i = segAtX(x); const sec = sectionTitleFor(i);
      tip.textContent = (sec ? sec + ' · ' : '') + `Sentence ${i + 1} of ${SEGMENTS.length}`;
      const r = bar.getBoundingClientRect();
      tip.style.left = Math.min(window.innerWidth - 20, Math.max(20, x)) + 'px';
      tip.style.top = r.top + 'px'; tip.classList.add('show');
    };
    bar.addEventListener('pointermove', (e) => { if (!scrubbing) showTip(e.clientX); });
    bar.addEventListener('pointerleave', () => { if (!scrubbing) tip.classList.remove('show'); });
    bar.addEventListener('pointerdown', (e) => {
      if (!SEGMENTS.length) return;
      scrubbing = true; bar.classList.add('scrubbing'); bar.setPointerCapture(e.pointerId);
      const move = (ev) => {
        const i = segAtX(ev.clientX);
        const pr = $('prog'); if (pr) pr.style.width = (SEGMENTS.length > 1 ? (i / (SEGMENTS.length - 1)) * 100 : 0) + '%';
        showTip(ev.clientX);
      };
      move(e);
      const finish = () => {
        bar.removeEventListener('pointermove', move);
        bar.removeEventListener('pointerup', up);
        bar.removeEventListener('pointercancel', cancel);
        scrubbing = false; bar.classList.remove('scrubbing'); tip.classList.remove('show');
      };
      const up = (ev) => { finish(); seekTo(segAtX(ev.clientX)); };
      const cancel = () => { finish(); updateProgress(); };
      bar.addEventListener('pointermove', move);
      bar.addEventListener('pointerup', up);
      bar.addEventListener('pointercancel', cancel);
    });
    bar.addEventListener('keydown', (e) => {
      if (!SEGMENTS.length) return;
      if (e.key === 'ArrowLeft' || e.key === 'ArrowDown') { e.preventDefault(); seekTo(state.idx - 1); }
      else if (e.key === 'ArrowRight' || e.key === 'ArrowUp') { e.preventDefault(); seekTo(state.idx + 1); }
      else if (e.key === 'Home') { e.preventDefault(); seekTo(0); }
      else if (e.key === 'End') { e.preventDefault(); seekTo(SEGMENTS.length - 1); }
    });
  }

  /* ---------------- bindings ---------------- */
  function bindChrome() {
    document.querySelectorAll('[data-act]').forEach((el) => el.onclick = () => {
      const a = el.dataset.act;
      if (a === 'toggle') togglePlay();
      else if (a === 'prev') seekTo(state.idx - 1);
      else if (a === 'next') seekTo(state.idx + 1);
      else if (a === 'gear') togglePop($('pop'), el);
      else if (a === 'edit') {
        const seg = SEGMENTS[state.idx];
        post({ type: 'openSource', line: seg ? seg.startLine : 1 });
      }
    });
    $('speed-btn').onclick = () => togglePop($('spop'), $('speed-btn'));
    $('speed-btn').addEventListener('wheel', (e) => { e.preventDefault(); setRate(state.rate + (e.deltaY < 0 ? 0.05 : -0.05), true); }, { passive: false });
    document.querySelectorAll('[data-theme-set]').forEach((b) => b.onclick = () => setTheme(b.dataset.themeSet));
    document.querySelectorAll('#comfort button').forEach((b) => b.onclick = () => setComfort(b.dataset.comfort));
    document.querySelectorAll('#sleep button').forEach((b) => b.onclick = () => armSleep(b.dataset.sleep));
    $('mode').addEventListener('click', (e) => {
      const b = e.target.closest('button'); if (!b) return;
      if (b.dataset.mode === state.mode) return;
      state.mode = b.dataset.mode; applyModeUI();
      post({ type: 'setMode', mode: state.mode });
    });
    $('voice').onchange = (e) => changeVoice(e.target.value);
    $('ambient-t').onchange = (e) => { state.ambient = e.target.checked; applyAmbient(); persistPrefs(); };
    $('follow-btn').onclick = () => attachFollow();
    $('resume-btn').onclick = () => resumeFromSaved();
    $('startover-btn').onclick = () => { hideResumePill(); playAt(0); };

    $('rate').oninput = (e) => setRate(parseFloat(e.target.value), true);
    document.querySelectorAll('#speed-presets button').forEach((b) => b.onclick = () => setRate(parseFloat(b.dataset.rate), true));

    const vol = $('volume');
    vol.oninput = (e) => {
      const v = parseFloat(e.target.value);
      state.volume = v;
      if (v > 0) { state.lastVol = v; state.muted = false; }
      applyAudioVolume(); updateMuteUI();
    };
    vol.onchange = () => { if (usesHostPlayback() && state.playing) ENGINE().play(state.idx); post({ type: 'persistVolume', value: state.volume }); };
    $('mute').onclick = () => {
      if (state.muted || state.volume === 0) {
        state.muted = false;
        if (state.volume === 0) state.volume = state.lastVol > 0 ? state.lastVol : 1;
        post({ type: 'persistVolume', value: state.volume });
      } else { state.lastVol = state.volume; state.muted = true; }
      applyVolume();
      if (usesHostPlayback() && state.playing) ENGINE().play(state.idx);
    };

    const wrap = $('reader-wrap');
    wrap.addEventListener('scroll', onScroll, { passive: true });
    wrap.addEventListener('wheel', (e) => {
      if (!state.playing) return;
      const canScroll = (e.deltaY < 0 && wrap.scrollTop > 0) || (e.deltaY > 0 && wrap.scrollTop < wrap.scrollHeight - wrap.clientHeight - 1);
      if (canScroll) detachFollow();
    }, { passive: true });
    wrap.addEventListener('touchmove', () => { if (state.playing) detachFollow(); }, { passive: true });
    bindProgress();

    document.addEventListener('keydown', (e) => {
      if (e.key === 'Escape') {
        if (openPopInfo) { e.preventDefault(); closePops(true); }
        else if (state.playing) { e.preventDefault(); doStop(); }
        return;
      }
      const t = e.target;
      if (t && (t.tagName === 'INPUT' || t.tagName === 'SELECT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
      if (t && t.classList && t.classList.contains('seg') && (e.key === 'Enter' || e.key === ' ')) {
        e.preventDefault();
        const id = Number(t.dataset.seg);
        if (id >= 0) playAt(id);
        return;
      }
      if (t && t.closest && t.closest('button, a, [role="button"], [role="slider"]')) return;
      if (e.ctrlKey || e.metaKey || e.altKey) return;
      if (e.key === ' ') { e.preventDefault(); togglePlay(); }
      else if (e.key === 'ArrowUp' || e.key === 'ArrowLeft') { e.preventDefault(); seekTo(state.idx - 1); }
      else if (e.key === 'ArrowDown' || e.key === 'ArrowRight') { e.preventDefault(); seekTo(state.idx + 1); }
      else if (e.key === '+' || e.key === '=') setRate(state.rate + 0.1, true);
      else if (e.key === '-') setRate(state.rate - 0.1, true);
      else if (e.key.toLowerCase() === 'm') $('mute').click();
      else if (e.key.toLowerCase() === 'f') cycleFont();
    });
    document.addEventListener('click', (e) => {
      if (!openPopInfo) return;
      if (!e.target.isConnected) return;
      if (!e.target.closest('.pop') && e.target !== openPopInfo.btn && !openPopInfo.btn.contains(e.target)) closePops();
    });
    // sentence click → read from there; Alt+Click → open the source line
    document.addEventListener('click', (e) => {
      const seg = e.target.closest && e.target.closest('.seg'); if (!seg) return;
      if (e.altKey) { post({ type: 'openSource', line: parseInt(seg.dataset.line || '1', 10) }); return; }
      const id = Number(seg.dataset.seg);
      if (id >= 0) playAt(id);
    });
  }

  /* ---------------- status ---------------- */
  function setStatus(text, kind) {
    const s = $('status'); if (!s) return;
    s.textContent = text;
    s.className = kind || '';
  }

  /* ---------------- messaging ---------------- */
  window.addEventListener('message', (e) => {
    const m = e.data; if (!m) return;
    switch (m.type) {
      case 'session': onSession(m); break;
      case 'sentence': if (m.sessionId === sessionId) appendSentence(m); break;
      case 'status': if (m.sessionId === sessionId) onStatus(m); break;
      case 'audio': onAudio(m); break;
      case 'audioError': onAudioError(m); break;
      case 'hostEnded': onHostEnded(m); break;
      case 'hostError': onHostError(m); break;
      case 'hostPlaying': if (m.gen === audioGen) { everSpoke = true; clearTimeout(speechWatch); logHost('hostPlaying[' + m.id + ']'); } break;
      case 'control': if (m.action === 'playpause') togglePlay(); else if (m.action === 'stop') doStop(); break;
    }
  });

  function onAudio(m) {
    if (m.gen !== audioGen) return;
    requested.delete(m.id);
    if (!m.bytes) { if (state.playing && state.idx === m.id) advance(); return; }
    const blob = new Blob([m.bytes], { type: m.mime || 'audio/wav' });
    const url = URL.createObjectURL(blob);
    const old = urlCache.get(m.id); if (old) URL.revokeObjectURL(old);
    urlCache.set(m.id, url);
    if (state.playing && state.idx === m.id && waitingFor === m.id) { waitingFor = -1; startAudio(url); }
  }
  function onAudioError(m) {
    if (m.gen !== audioGen) return;
    requested.delete(m.id);
    logHost('audioError[' + m.id + ']' + (m.message ? ' ' + m.message : ''));
    if (state.playing && state.idx === m.id) { setStatus('Audio error: ' + (m.message || 'synthesis failed'), 'warn'); advance(); }
  }
  function onHostEnded(m) {
    if (m.gen !== audioGen) return;
    if (state.playing && state.idx === m.id) advance();
  }
  function onHostError(m) {
    if (m.gen !== audioGen) return;
    logHost('hostError[' + m.id + ']' + (m.message ? ' ' + m.message : ''));
    if (state.playing && state.idx === m.id) {
      setPlaying(false); // stop rather than racing through every sentence on a system-audio failure
      setStatus('Audio error: ' + (m.message || 'playback failed') + ' — is your system audio working?', 'warn');
    }
  }

  function onSession(m) {
    sessionId = m.sessionId;
    state.engine = (m.engine === 'say' || m.engine === 'piper') ? m.engine : 'webspeech';
    state.hostPlayback = !!m.hostPlayback;
    resetDoc();
    state.docKey = m.docKey || '';
    state.mode = m.mode || 'ai';
    state.ollamaModel = m.ollamaModel || '';
    state.rate = typeof m.rate === 'number' ? m.rate : 1;
    state.volume = typeof m.volume === 'number' ? m.volume : 1;
    state.lastVol = state.volume || 1; state.muted = false;
    state.settings = Object.assign({ highlight: true, codeHandling: 'explain', announceHeadings: false }, m.settings || {});
    document.body.classList.toggle('no-highlight', !state.settings.highlight);
    applyPrefs(m.prefs);
    HOST_VOICES = Array.isArray(m.hostVoices) ? m.hostVoices : [];
    hostVoiceId = m.hostVoiceId || '';
    hostAudioReady = m.hostAudioReady !== false;
    wsWantURI = m.voiceURI || wsWantURI;
    everSpoke = false;
    pendingResume = m.resume || null;
    autoplayPending = true;
    lastSentIdx = -1;

    $('doctitle').textContent = m.title || 'Tech Reader';
    $('doctitle').title = m.title || '';
    buildFontList(); applyFont(); applyComfort(); applyTheme(); applyAmbient(); applyModeUI();
    if (state.engine === 'webspeech') loadWsVoices();
    renderVoiceSelect();
    applyRate(); applyVolume();
    updateProgress(); setPlayIcon();
    logHost('onSession #' + sessionId + ' engine=' + state.engine + ' mode=' + state.mode + ' hostVoices=' + HOST_VOICES.length + ' wsVoices=' + WS_VOICES.length);
    if (usesHostSynth() && !hostAudioReady) {
      setStatus(state.engine === 'piper' ? 'Piper not set up — install the piper CLI and a voice model (see settings).' : '`say` needs macOS.', 'warn');
    } else setStatus('Preparing…');
  }

  function onStatus(m) {
    switch (m.state) {
      case 'thinking': setStatus(m.message || ('Explaining with ' + state.ollamaModel + '…'), 'busy'); break;
      case 'streaming': setStatus(state.mode === 'ai' ? 'Explaining…' : 'Reading…', 'busy'); break;
      case 'fallback': setStatus(m.message || 'Ollama offline — literal mode', 'warn'); break;
      case 'done': narrationDone = true; setStatus(SEGMENTS.length ? 'Ready' : 'Nothing to read', ''); if (!SEGMENTS.length) document.body.classList.add('no-segments'); break;
      case 'empty': narrationDone = true; setStatus('Nothing readable here', 'warn'); document.body.classList.add('no-segments'); break;
      case 'error': narrationDone = true; setStatus(m.message ? ('Error: ' + m.message) : 'Something went wrong', 'warn'); break;
    }
  }

  /* ---------------- boot ---------------- */
  buildChrome();
  applyTheme(); applyFont(); applyComfort(); applyAmbient(); applyModeUI(); applyRate(); applyVolume();
  if (speechSynthesis.onvoiceschanged !== undefined) speechSynthesis.onvoiceschanged = loadWsVoices;
  loadWsVoices();
  logHost('webview booted; hasSpeechSynthesis=' + (typeof speechSynthesis !== 'undefined') + ' getVoices=' + (speechSynthesis.getVoices() || []).length);
  post({ type: 'ready' });
})();
