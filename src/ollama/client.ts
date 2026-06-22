/**
 * Minimal HTTP client for a LOCAL Ollama server.
 *
 * Designed to run inside the VS Code Node extension host (Node 18+), relying
 * only on globals available there: `fetch`, `AbortController`, `TextDecoder`,
 * and the WHATWG `ReadableStream` exposed via `Response.body`. There are NO
 * runtime npm dependencies.
 *
 * Capabilities:
 *   - isAvailable(baseUrl, timeoutMs?) -> probe GET /api/tags, never throws.
 *   - listModels(baseUrl)             -> names of installed models.
 *   - streamChat(cfg, req, onToken)   -> stream POST /api/chat (NDJSON), invoking
 *                                        onToken(delta) for each text delta.
 *
 * Ollama streams chat responses as newline-delimited JSON (NDJSON): one JSON
 * object per line. Each object carries an incremental `message.content` delta
 * plus a `done` flag; a terminal object has `"done": true`. Error conditions
 * can surface either as a non-OK HTTP status or as a single JSON object with an
 * `error` field (sometimes returned with HTTP 200). All failures are normalized
 * to an `OllamaError` with a discriminating `code`.
 */

/** Configuration for a chat completion against an Ollama server. */
export interface OllamaConfig {
  baseUrl: string; // e.g. "http://localhost:11434" (a trailing slash is tolerated)
  model: string; // e.g. "llama3.2" or "qwen2.5-coder"
  temperature?: number; // sampling temperature, default 0.3
  keepAlive?: string; // Ollama keep_alive duration, default "5m"
  /** Abort a stream that produces no bytes for this long (ms). Default 60000. */
  idleTimeoutMs?: number;
}

/** A single chat turn: a system prompt + a user prompt, optionally cancelable. */
export interface ChatRequest {
  system: string;
  prompt: string;
  signal?: AbortSignal;
}

/** Discriminates the category of an OllamaError. */
export type OllamaErrorCode = 'unreachable' | 'model-missing' | 'http' | 'aborted' | 'parse';

/** Normalized error type for every failure path in this module. */
export class OllamaError extends Error {
  code: OllamaErrorCode;
  constructor(code: OllamaErrorCode, message: string) {
    super(message);
    this.name = 'OllamaError';
    this.code = code;
    // Restore prototype chain for instanceof to work across the TS->ES target gap.
    Object.setPrototypeOf(this, OllamaError.prototype);
  }
}

/** Strip a single trailing slash so we can safely append "/api/...". */
function normalizeBaseUrl(baseUrl: string): string {
  return baseUrl.replace(/\/+$/, '');
}

/** True if a thrown value is an AbortError (from an aborted fetch / signal). */
function isAbortError(err: unknown): boolean {
  return (
    err instanceof Error && (err.name === 'AbortError' || err.name === 'TimeoutError')
  );
}

/**
 * Inspect an Ollama `{ "error": "..." }` payload and throw the most specific
 * OllamaError. "not found" / "try pulling" indicate a missing model.
 */
function throwFromErrorField(errorText: string): never {
  const lower = errorText.toLowerCase();
  if (lower.includes('not found') || lower.includes('try pulling')) {
    throw new OllamaError('model-missing', errorText);
  }
  throw new OllamaError('http', errorText);
}

/**
 * True if the Ollama server answers /api/tags within `timeoutMs` (default 1500).
 * Never throws — any failure (network, timeout, non-OK status) yields `false`.
 */
export async function isAvailable(baseUrl: string, timeoutMs = 1500): Promise<boolean> {
  const base = normalizeBaseUrl(baseUrl);
  const controller = new AbortController();
  // Abort the probe if the server is too slow to answer.
  const timer = setTimeout(() => controller.abort(), timeoutMs);
  try {
    const res = await fetch(`${base}/api/tags`, { signal: controller.signal });
    return res.ok;
  } catch {
    return false;
  } finally {
    // Always clear the timer so we never leak a pending timeout.
    clearTimeout(timer);
  }
}

/**
 * Model names installed on the server, parsed from GET /api/tags.
 * Throws OllamaError('unreachable') if the server cannot be reached, and
 * OllamaError('http') on a non-OK status.
 */
export async function listModels(baseUrl: string): Promise<string[]> {
  const base = normalizeBaseUrl(baseUrl);
  let res: Response;
  try {
    res = await fetch(`${base}/api/tags`);
  } catch (err) {
    // A network-level failure (ECONNREFUSED, DNS, etc.) means the server is down.
    throw new OllamaError(
      'unreachable',
      `Could not reach Ollama at ${base}: ${(err as Error)?.message ?? String(err)}`,
    );
  }
  if (!res.ok) {
    throw new OllamaError('http', `GET /api/tags failed with HTTP ${res.status}`);
  }
  try {
    const data = (await res.json()) as { models?: Array<{ name?: string }> };
    const models = data?.models ?? [];
    // Keep only well-formed, non-empty names.
    return models
      .map((m) => m?.name)
      .filter((n): n is string => typeof n === 'string' && n.length > 0);
  } catch (err) {
    throw new OllamaError(
      'parse',
      `Could not parse /api/tags response: ${(err as Error)?.message ?? String(err)}`,
    );
  }
}

/** Shape of a single NDJSON line emitted by /api/chat. */
interface OllamaChatLine {
  message?: { role?: string; content?: string };
  done?: boolean;
  error?: string;
}

/**
 * Stream a chat completion from POST /api/chat with `stream: true`.
 *
 * Calls `onToken(delta)` for each non-empty text delta as it arrives, and
 * resolves when the terminal `{"done": true}` line is read OR the server closes
 * the stream. Honors `req.signal` for cancellation, and aborts itself if no
 * bytes arrive for `cfg.idleTimeoutMs` (so a stalled stream can't hang forever).
 * Every failure rejects with an OllamaError:
 *   - network failure / idle timeout -> 'unreachable'
 *   - aborted via signal             -> 'aborted'
 *   - missing model                  -> 'model-missing'
 *   - other non-OK / error           -> 'http'
 *   - wholly unparseable body        -> 'parse'
 */
export async function streamChat(
  cfg: OllamaConfig,
  req: ChatRequest,
  onToken: (delta: string) => void,
): Promise<void> {
  const base = normalizeBaseUrl(cfg.baseUrl);
  const body = {
    model: cfg.model,
    stream: true,
    options: { temperature: cfg.temperature ?? 0.3 },
    keep_alive: cfg.keepAlive ?? '5m',
    messages: [
      { role: 'system', content: req.system },
      { role: 'user', content: req.prompt },
    ],
  };

  // --- Idle-timeout plumbing -------------------------------------------------
  // Ollama is local, but a frozen process, a GPU hang, or an OS suspend can
  // leave the socket open with no further bytes. Without a guard, reader.read()
  // would await forever and hang the whole narration. We abort a local
  // controller if no progress is made within `idleTimeoutMs`, and link the
  // caller's signal to it so user-cancel still works.
  const idleMs = cfg.idleTimeoutMs ?? 60000;
  const localCtrl = new AbortController();
  let timedOut = false;
  let idleTimer: ReturnType<typeof setTimeout> | undefined;
  const resetIdle = () => {
    if (idleTimer) clearTimeout(idleTimer);
    idleTimer = setTimeout(() => {
      timedOut = true;
      localCtrl.abort();
    }, idleMs);
  };
  const onParentAbort = () => localCtrl.abort();
  if (req.signal) {
    if (req.signal.aborted) localCtrl.abort();
    else req.signal.addEventListener('abort', onParentAbort, { once: true });
  }
  // Classify how `localCtrl` ended up aborted: user-cancel vs. our idle timeout.
  const abortError = (): OllamaError =>
    req.signal?.aborted
      ? new OllamaError('aborted', 'Chat request was aborted.')
      : timedOut
        ? new OllamaError('unreachable', `Ollama produced no output for ${idleMs}ms.`)
        : new OllamaError('aborted', 'Chat request was aborted.');
  const cleanup = () => {
    if (idleTimer) clearTimeout(idleTimer);
    req.signal?.removeEventListener('abort', onParentAbort);
  };

  // --- Issue the request -----------------------------------------------------
  let res: Response;
  resetIdle();
  try {
    res = await fetch(`${base}/api/chat`, {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
      signal: localCtrl.signal,
    });
  } catch (err) {
    cleanup();
    // Aborting before/while connecting surfaces as an AbortError here.
    if (isAbortError(err) || localCtrl.signal.aborted) {
      throw abortError();
    }
    throw new OllamaError(
      'unreachable',
      `Could not reach Ollama at ${base}: ${(err as Error)?.message ?? String(err)}`,
    );
  }

  // --- Classify non-OK responses --------------------------------------------
  // Ollama returns errors as a JSON object with an `error` field; the status
  // is often 404 for a missing model but can vary. Read the body and classify.
  if (!res.ok) {
    cleanup();
    const text = await res.text().catch(() => '');
    const errorText = extractErrorText(text) ?? `HTTP ${res.status}`;
    throwFromErrorField(errorText);
  }

  // Some Ollama error payloads arrive with HTTP 200 but no streamable body.
  if (!res.body) {
    cleanup();
    const text = await res.text().catch(() => '');
    const errorText = extractErrorText(text);
    if (errorText) {
      throwFromErrorField(errorText);
    }
    throw new OllamaError('http', 'Ollama returned an empty response body.');
  }

  // --- Stream the NDJSON body ------------------------------------------------
  const reader = res.body.getReader();
  const decoder = new TextDecoder();
  let buffer = ''; // Holds the partial line carried across chunk boundaries.
  let sawAnyValidLine = false; // Tracks whether we ever parsed a usable line.
  let sawParseFailure = false; // Tracks whether at least one line failed to parse.

  try {
    // Read chunks until the stream ends or a terminal/error line is hit.
    // eslint-disable-next-line no-constant-condition
    while (true) {
      // Cooperative cancellation: the underlying fetch will also reject the
      // read, but checking here lets us fail fast and cleanly.
      if (req.signal?.aborted) {
        throw new OllamaError('aborted', 'Chat stream was aborted.');
      }

      const { done, value } = await reader.read();
      resetIdle(); // progress made — restart the inactivity countdown
      if (done) {
        break; // Server closed the stream.
      }

      // Decode this chunk (streaming mode keeps multi-byte chars intact) and
      // append to whatever partial line we had left over.
      buffer += decoder.decode(value, { stream: true });

      // Split on newlines; the last element is a (possibly empty) partial line
      // that we keep buffered until more bytes arrive.
      const lines = buffer.split('\n');
      buffer = lines.pop() ?? '';

      for (const rawLine of lines) {
        const line = rawLine.trim();
        if (!line) {
          continue; // Skip blank lines between JSON objects.
        }

        let obj: OllamaChatLine;
        try {
          obj = JSON.parse(line) as OllamaChatLine;
        } catch {
          // A single malformed line is tolerated; remember it in case the whole
          // stream turns out to be unparseable.
          sawParseFailure = true;
          continue;
        }
        sawAnyValidLine = true;

        // An inline error object can appear even within a 200 stream.
        if (typeof obj.error === 'string' && obj.error.length > 0) {
          throwFromErrorField(obj.error);
        }

        // Emit the text delta, if any. Empty deltas are skipped.
        const delta = obj.message?.content;
        if (delta) {
          onToken(delta);
        }

        // The terminal line marks completion.
        if (obj.done === true) {
          return;
        }
      }
    }

    // --- Stream ended without an explicit done:true ---------------------------
    // Flush any trailing buffered line (e.g. a final object with no newline).
    const tail = buffer.trim();
    if (tail) {
      try {
        const obj = JSON.parse(tail) as OllamaChatLine;
        sawAnyValidLine = true;
        if (typeof obj.error === 'string' && obj.error.length > 0) {
          throwFromErrorField(obj.error);
        }
        const delta = obj.message?.content;
        if (delta) {
          onToken(delta);
        }
        // If this last line marks done, we finished cleanly.
        if (obj.done === true) {
          return;
        }
      } catch {
        sawParseFailure = true;
      }
    }

    // If we never parsed a single valid line but did see parse failures, the
    // body was effectively garbage.
    if (!sawAnyValidLine && sawParseFailure) {
      throw new OllamaError('parse', 'Ollama stream contained no parseable JSON lines.');
    }

    // Otherwise the stream ended (server closed) after emitting deltas without a
    // formal done marker — treat as a normal completion.
    return;
  } catch (err) {
    // Preserve already-classified errors.
    if (err instanceof OllamaError) {
      throw err;
    }
    // Abort surfacing as a reader/read rejection — distinguish user-cancel
    // (aborted) from our own idle timeout (unreachable, so the narrator falls
    // back to the offline humanizer rather than stopping).
    if (isAbortError(err) || localCtrl.signal.aborted) {
      throw abortError();
    }
    // Anything else mid-stream is a transport-level failure.
    throw new OllamaError(
      'unreachable',
      `Lost connection to Ollama: ${(err as Error)?.message ?? String(err)}`,
    );
  } finally {
    cleanup();
    // Always release the reader so the underlying connection is freed.
    try {
      reader.releaseLock();
    } catch {
      // Reader may already be released if the stream errored; ignore.
    }
  }
}

/**
 * Pull a human-readable error string out of an Ollama error response body.
 * Returns the `error` field if the body is JSON, the raw text if it is a
 * non-empty plain string, or undefined when there is nothing useful.
 */
function extractErrorText(text: string): string | undefined {
  const trimmed = text.trim();
  if (!trimmed) {
    return undefined;
  }
  try {
    const obj = JSON.parse(trimmed) as { error?: string };
    if (typeof obj.error === 'string' && obj.error.length > 0) {
      return obj.error;
    }
  } catch {
    // Not JSON — fall through and use the raw text.
  }
  return trimmed;
}
