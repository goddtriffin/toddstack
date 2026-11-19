/**
 * The core's module worker: runs the wasm so no call of the core is ever a task on the page's
 * thread. A top-level entry for the app to call from its own worker file:
 *
 * ```ts
 * import init, { Core, bridge_protocol } from "./generated/core.js";
 * import { serveCore } from "./toddstack-frontend-engine/worker.ts";
 * serveCore(self, { init, Core, bridge_protocol }, { loadBlob: (name) => fetchBytes(name) });
 * ```
 * @module
 */

import type { EffectReq, FromWorker, ToWorker, WasmCore, WasmCoreClass } from "./protocol.ts";

/** What the generated wasm-bindgen module exports. */
export interface WasmModule {
  init(arg: { module_or_path: WebAssembly.Module }): Promise<unknown>;
  Core: WasmCoreClass;
  bridge_protocol(): number;
}

/** Worker hooks the app supplies. */
export interface ServeOptions {
  /** Fetch the bytes of the blob the core asked for (`LoadBlob`); a throw is the core's `err`. */
  loadBlob?(name: string): Promise<Uint8Array<ArrayBuffer>>;
}

/** The worker's one state machine. */
type Phase =
  | { p: "Waiting" }
  | { p: "Ready"; core: WasmCore | null }
  | { p: "Failed"; error: string };

interface Parsed {
  dirty?: string[];
  effects?: EffectReq[];
  error?: string;
}

/** The slice of a worker's global scope this module uses. */
export interface WorkerScope {
  postMessage(m: FromWorker, transfer: Transferable[]): void;
  onmessage: ((e: MessageEvent<ToWorker>) => void) | null;
}

/** Start serving: installs `onmessage` on `scope`. */
export function serveCore(scope: WorkerScope, wasm: WasmModule, options: ServeOptions = {}): void {
  let phase: Phase = { p: "Waiting" };
  /** Boots handled so far; stamped on every reply. */
  let gen = 0;
  const watched = new Set<string>();
  const enc = new TextEncoder();
  const dec = new TextDecoder();

  const post = (m: FromWorker, transfer: Transferable[] = []): void => scope.postMessage(m, transfer);

  const fail = (error: string): void => {
    phase = { p: "Failed", error };
    post({ t: "fail", error });
  };

  const announce = (): void => {
    if (phase.p === "Ready") post({ t: "ready", bridge_protocol: wasm.bridge_protocol() });
    else if (phase.p === "Failed") post({ t: "fail", error: phase.error });
  };

  const parse = (bytes: Uint8Array<ArrayBuffer>): Parsed => JSON.parse(dec.decode(bytes));

  /** The one place a reply leaves: the views it dirtied (of keys the page watches) ride along, transferred. */
  const out = (
    parsed: Parsed,
    core: WasmCore,
    extra: { seq?: number; raw?: Uint8Array<ArrayBuffer>; n?: number } = {},
    all = false,
  ): void => {
    const views: Record<string, Uint8Array<ArrayBuffer>> = {};
    const transfer: Transferable[] = [];
    for (const key of all ? watched : parsed.dirty ?? []) {
      if (!watched.has(key)) continue;
      const v = core.view(key);
      views[key] = v;
      transfer.push(v.buffer);
    }
    if (extra.raw) transfer.push(extra.raw.buffer);
    const m: FromWorker = { t: "out", gen, effects: parsed.effects ?? [], views, ...extra };
    if (parsed.error) m.error = parsed.error;
    post(m, transfer);
  };

  const dispatch = (core: WasmCore, bytes: Uint8Array<ArrayBuffer>, seq?: number): void => {
    const reply = core.dispatch(bytes);
    out(parse(reply), core, { seq, ...(seq === undefined ? {} : { raw: reply }) });
  };

  /** The blob the core asked for: fetched here. A failure is the core's to handle (`err`). */
  const blob = async (core: WasmCore, id: number, name: string, at: number): Promise<void> => {
    let bytes: Uint8Array<ArrayBuffer> | null = null;
    let why = "";
    try {
      if (!options.loadBlob) why = "this app loads no blobs";
      else bytes = await options.loadBlob(name);
    } catch (e) {
      why = String(e);
    }
    // A restart may have replaced the core while the blob came: the answer is for the old one.
    if (phase.p !== "Ready" || phase.core !== core) return;
    if (bytes) out(parse(core.provide_blob(name, bytes)), core);
    else dispatch(core, enc.encode(JSON.stringify({ at, resolve: { id, err: why } })));
  };

  const handle = (m: ToWorker): void => {
    if (m.t === "module") {
      if (phase.p !== "Waiting") return;
      if (!m.module) return fail(m.error ?? "the core did not load");
      wasm.init({ module_or_path: m.module }).then(
        () => {
          phase = { p: "Ready", core: null };
          announce();
        },
        (err) => fail(String(err)),
      );
      return;
    }
    if (m.t === "hello") return announce();
    if (phase.p !== "Ready") return;
    if (m.t === "boot") {
      phase.core?.free();
      gen++;
      const core = new wasm.Core(m.bytes);
      phase = { p: "Ready", core };
      out({}, core, { seq: m.seq }, true);
      return;
    }
    if (m.t === "free") {
      phase.core?.free();
      phase = { p: "Ready", core: null };
      return;
    }
    const core = phase.core;
    if (!core) return;
    switch (m.t) {
      case "watch":
        for (const k of m.keys) watched.add(k);
        out({}, core, {}, true);
        break;
      case "input":
        dispatch(core, m.bytes, m.seq);
        break;
      case "pointer": {
        const n = core.pointer(m.phase, m.x, m.y, m.at);
        if (m.seq !== undefined) out({}, core, { seq: m.seq, n });
        else if (n !== 0) dispatch(core, enc.encode(JSON.stringify({ at: m.at, flush: {} })));
        break;
      }
      case "blob":
        void blob(core, m.id, m.name, m.at);
        break;
      case "provide": {
        const reply = core.provide_blob(m.name, m.bytes);
        out(parse(reply), core, { seq: m.seq, raw: reply });
        break;
      }
      case "view":
        out({}, core, { seq: m.seq, raw: core.view(m.key) });
        break;
    }
  };

  scope.onmessage = (e) => {
    try {
      handle(e.data);
    } catch (err) {
      fail(String(err instanceof Error ? err.stack ?? err.message : err));
    }
  };
}
