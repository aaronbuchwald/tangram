// The Worker-side host for jco-transpiled Tangram components (ADR-0002).
//
// Two import surfaces:
//   - `tangram:app/host` — the app's ENTIRE capability grant, mirroring
//     tangram-host's implementation (crates/tangram-host/src/runtime.rs):
//     `http-fetch` with a per-app outbound host allowlist, `log` into the
//     Worker's console, `now-ms`.
//   - the wasip2 std plumbing (env/clocks/random/stdio) — hand-written and
//     minimal on purpose: the components have NO filesystem or socket
//     imports at all, and @bytecodealliance/preview2-shim's node backend is
//     not workerd-compatible. Like tangram-host's empty WasiCtx, these carry
//     data (env vars, time, randomness), not reach.
//
// jco error convention: an import that implements a WIT `result<_, E>`
// delivers `err(E)` by throwing a NON-Error value (jco's getErrorPayload
// re-throws Error instances as traps); an export returning `result` throws
// a ComponentError whose `.payload` is the error value.

/** The `tangram:app/guest` surface the Worker drives (camelCased by jco).
 * `dispatch` is a Promise because the JSPI build suspends on http-fetch. */
export interface TangramGuest {
  describe(): string;
  genesis(): Uint8Array;
  dispatch(action: string, argsJson: string, doc: Uint8Array): Promise<DispatchResult>;
  stateJson(doc: Uint8Array): string;
}

export interface DispatchResult {
  /** Full automerge save IF the action mutated the document. */
  doc?: Uint8Array;
  resultJson: string;
}

/** The error value out of a jco-transpiled `result<_, string>` export. */
export function errorPayload(e: unknown): string {
  if (typeof e === "string") return e;
  if (e && typeof e === "object" && "payload" in e) {
    const payload = (e as { payload: unknown }).payload;
    if (typeof payload === "string") return payload;
  }
  return e instanceof Error ? e.message : String(e);
}

// ── tangram:app/host ─────────────────────────────────────────────────────────

const b64 = {
  encode(bytes: Uint8Array): string {
    let binary = "";
    const chunk = 0x8000;
    for (let i = 0; i < bytes.length; i += chunk) {
      binary += String.fromCharCode(...bytes.subarray(i, i + chunk));
    }
    return btoa(binary);
  },
  decode(text: string): Uint8Array {
    const binary = atob(text);
    const bytes = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) bytes[i] = binary.charCodeAt(i);
    return bytes;
  },
};

/** Build the `tangram:app/host` import for one app. Same JSON contract and
 * allowlist semantics as tangram-host's `HostState::http_fetch`. */
export function tangramHostImports(
  app: string,
  allowHosts: string[],
): Record<string, unknown> {
  return {
    // async on purpose: the JSPI transpile wraps this in
    // WebAssembly.Suspending, so the guest's synchronous call suspends
    // while the Worker's fetch() promise settles.
    httpFetch: async (requestJson: string): Promise<string> => {
      let request: {
        method?: string;
        url?: string;
        headers?: Record<string, string>;
        "body-b64"?: string;
      };
      try {
        request = JSON.parse(requestJson);
      } catch (e) {
        throw `malformed request: ${e}`;
      }
      const url = request.url;
      if (!url) throw "request is missing url";
      let parsed: URL;
      try {
        parsed = new URL(url);
      } catch (e) {
        throw `invalid url ${JSON.stringify(url)}: ${e}`;
      }
      const host = parsed.hostname;
      // The capability check: the app's registry entry grants exact hosts.
      if (!allowHosts.some((allowed) => allowed.toLowerCase() === host.toLowerCase())) {
        console.warn(`[${app}] denied outbound request to ${host} (not in allow_hosts)`);
        throw (
          `outbound request to ${JSON.stringify(host)} denied: it is not in app ` +
          `${JSON.stringify(app)}'s allow_hosts (granted: ${JSON.stringify(allowHosts)}); ` +
          `grant it in the app's registry entry (cloud/cloudflare/src/components.ts)`
        );
      }
      const headers = new Headers();
      for (const [name, value] of Object.entries(request.headers ?? {})) {
        headers.set(name, value);
      }
      const body = request["body-b64"] ? b64.decode(request["body-b64"]) : undefined;
      let response: Response;
      try {
        response = await fetch(url, {
          method: request.method ?? "GET",
          headers,
          body,
        });
      } catch (e) {
        throw `request failed: ${e instanceof Error ? e.message : e}`;
      }
      const responseHeaders: Record<string, string> = {};
      response.headers.forEach((value, name) => {
        responseHeaders[name] = value;
      });
      const bytes = new Uint8Array(await response.arrayBuffer());
      return JSON.stringify({
        status: response.status,
        headers: responseHeaders,
        "body-b64": b64.encode(bytes),
      });
    },

    log: (level: string, message: string) => {
      switch (level) {
        case "error":
          console.error(`[${app}] ${message}`);
          break;
        case "warn":
          console.warn(`[${app}] ${message}`);
          break;
        case "info":
          console.log(`[${app}] ${message}`);
          break;
        // trace/debug are dropped: automerge's internals are chatty and
        // workerd logs are per-request.
      }
    },

    nowMs: (): bigint => BigInt(Date.now()),
  };
}

// ── wasip2 std plumbing ──────────────────────────────────────────────────────

class IoError extends Error {}

class Pollable {
  ready(): boolean {
    return true;
  }
  block(): void {}
}

class InputStream {
  read(): Uint8Array {
    return new Uint8Array();
  }
  blockingRead(): Uint8Array {
    return new Uint8Array();
  }
  subscribe(): Pollable {
    return new Pollable();
  }
}

const decoder = new TextDecoder();

/** Guest stdout/stderr land in the console (panic messages, mostly — the
 * guest's tracing goes through the `log` import instead). */
class OutputStream {
  constructor(private tag: string) {}
  checkWrite(): bigint {
    return 4096n;
  }
  write(bytes: Uint8Array): void {
    const text = decoder.decode(bytes).trim();
    if (text) console.error(`[guest ${this.tag}] ${text}`);
  }
  blockingWriteAndFlush(bytes: Uint8Array): void {
    this.write(bytes);
  }
  flush(): void {}
  blockingFlush(): void {}
  subscribe(): Pollable {
    return new Pollable();
  }
}

class TerminalInput {}
class TerminalOutput {}
class Descriptor {}

/** The wasip2 std imports, with `env` as the component's entire environment
 * (the capability-granted vars, e.g. nutrition's strategy selection). */
export function wasiImports(env: [string, string][]): Record<string, Record<string, unknown>> {
  return {
    "wasi:cli/environment": { getEnvironment: () => env },
    "wasi:cli/exit": {
      exit: (status: unknown) => {
        throw new Error(`guest exit(${JSON.stringify(status)})`);
      },
    },
    "wasi:cli/stdin": { getStdin: () => new InputStream() },
    "wasi:cli/stdout": { getStdout: () => new OutputStream("stdout") },
    "wasi:cli/stderr": { getStderr: () => new OutputStream("stderr") },
    "wasi:cli/terminal-input": { TerminalInput },
    "wasi:cli/terminal-output": { TerminalOutput },
    "wasi:cli/terminal-stdin": { getTerminalStdin: () => undefined },
    "wasi:cli/terminal-stdout": { getTerminalStdout: () => undefined },
    "wasi:cli/terminal-stderr": { getTerminalStderr: () => undefined },
    "wasi:clocks/wall-clock": {
      now: () => {
        const ms = Date.now();
        return {
          seconds: BigInt(Math.floor(ms / 1000)),
          nanoseconds: (ms % 1000) * 1e6,
        };
      },
      resolution: () => ({ seconds: 0n, nanoseconds: 1e6 }),
    },
    "wasi:clocks/monotonic-clock": {
      now: () => BigInt(Math.round(performance.now() * 1e6)),
      resolution: () => 1n,
      subscribeDuration: () => new Pollable(),
      subscribeInstant: () => new Pollable(),
    },
    "wasi:random/random": {
      getRandomBytes: (len: bigint) => crypto.getRandomValues(new Uint8Array(Number(len))),
      getRandomU64: () => crypto.getRandomValues(new BigUint64Array(1))[0],
    },
    "wasi:random/insecure-seed": {
      insecureSeed: () => [
        crypto.getRandomValues(new BigUint64Array(1))[0],
        crypto.getRandomValues(new BigUint64Array(1))[0],
      ],
    },
    "wasi:filesystem/preopens": { getDirectories: () => [] },
    "wasi:filesystem/types": { Descriptor, filesystemErrorCode: () => undefined },
    "wasi:io/error": { Error: IoError },
    "wasi:io/poll": { Pollable, poll: (list: unknown[]) => list.map((_, i) => i) },
    "wasi:io/streams": { InputStream, OutputStream },
  };
}
