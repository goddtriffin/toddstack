/**
 * The page's side of the core worker: boots the core, posts inputs, draws the views that change
 * and runs the effects the core asks for. Timers are built in; every other effect goes to the
 * app's `run` function, which answers it (or returns `undefined` for fire and forget).
 * @module
 */

import type { Answer, EffectReq, FromWorker, ToWorker } from "./protocol.ts";

type Out = Extract<FromWorker, { t: "out" }>;
type Listener = (view: unknown) => void;

/** Runs one effect and answers it; `undefined` sends no answer. */
export type RunEffect = (req: EffectReq) => Promise<Answer | undefined> | Answer | undefined;

/** Connect to a started worker: resolves once the wasm is compiled and the worker is ready. */
export function connect(worker: Worker): Promise<{ bridgeProtocol: number }> {
  return new Promise((ok, err) => {
    worker.onmessage = (e: MessageEvent<FromWorker>) => {
      if (e.data.t === "ready") ok({ bridgeProtocol: e.data.bridge_protocol });
      else if (e.data.t === "fail") err(new Error(e.data.error));
    };
    worker.onerror = (e) => err(new Error(e.message || "the worker failed to load"));
    worker.postMessage({ t: "hello" } satisfies ToWorker);
  });
}

/** One running core and its page-side loop. */
export class CoreClient {
  private readonly listeners = new Map<string, Set<Listener>>();
  /** The last view of every watched key, parsed. */
  private readonly views = new Map<string, unknown>();
  private readonly timers = new Map<number, ReturnType<typeof setTimeout>>();
  private readonly enc = new TextEncoder();
  private readonly dec = new TextDecoder();
  /** Keys the worker was asked for, and those not yet told: told in one batch, so one reply draws them all. */
  private readonly asked = new Set<string>();
  private fresh: string[] = [];
  private boots = 0;
  private gone = false;
  /** Draw errors, for a test driver to report. */
  readonly errors: string[] = [];

  /**
   * `boot` builds the facts for a (re)start: what the browser holds changes as the app runs.
   * `connect` must have resolved on `worker` first.
   */
  constructor(
    private readonly worker: Worker,
    private readonly boot: () => object,
    private readonly run: RunEffect,
  ) {
    worker.onmessage = (e: MessageEvent<FromWorker>) => this.receive(e.data);
    this.bootCore();
  }

  private post(m: ToWorker, transfer: Transferable[] = []): void {
    this.worker.postMessage(m, transfer);
  }

  private bootCore(): void {
    const bytes = this.enc.encode(JSON.stringify(this.boot()));
    this.boots++;
    this.post({ t: "boot", bytes }, [bytes.buffer]);
  }

  /** Throw the core away and boot a new one from what the browser holds now
   * (a page restored from the back-forward cache, another tab that wrote). */
  restart(): void {
    this.clearTimers();
    this.bootCore();
    this.start();
  }

  /** Stop: free the core and its timers. */
  free(): void {
    this.clearTimers();
    this.post({ t: "free" });
  }

  /** Stop running effects (the page is navigating away). */
  leaving(): void {
    this.gone = true;
  }

  private clearTimers(): void {
    for (const t of this.timers.values()) clearTimeout(t);
    this.timers.clear();
  }

  /** Draw `key` now (once the worker answers) and whenever it changes. */
  watch<T>(key: string, draw: (view: T) => void): void {
    let set = this.listeners.get(key);
    if (!set) this.listeners.set(key, set = new Set());
    set.add(draw as Listener);
    if (this.views.has(key)) return this.draw(key, draw as Listener);
    if (this.asked.has(key)) return;
    this.asked.add(key);
    if (this.fresh.push(key) === 1) {
      queueMicrotask(() => {
        this.post({ t: "watch", keys: this.fresh });
        this.fresh = [];
      });
    }
  }

  /** The last view of a watched key, as drawn; null before the first arrives. */
  view<T = unknown>(key: string): T {
    return (this.views.get(key) ?? null) as T;
  }

  /** Send an action; its effects run, its dirty views redraw. */
  act(action: unknown): void {
    this.input({ act: action });
  }

  /** One protocol input, stamped with the wall clock. */
  input(message: object): void {
    const bytes = this.enc.encode(JSON.stringify({ at: Date.now(), ...message }));
    this.post({ t: "input", bytes }, [bytes.buffer]);
  }

  /** Collect what boot queued (the first requests, a timer). */
  start(): void {
    this.input({ flush: {} });
  }

  /** One pointer sample on the typed fast path, in the units the app chose. */
  touch(phase: number, x: number, y: number): void {
    this.post({ t: "pointer", phase, x, y, at: Date.now() });
  }

  private receive(m: FromWorker): void {
    if (m.t === "fail") {
      console.error("the core worker failed:", m.error);
      this.errors.push(m.error);
      return;
    }
    // A restart boots a new core whose effect ids start over: a reply from the old one (posted
    // before the worker handled the boot) must not run its effects or draw.
    if (m.t !== "out" || m.gen !== this.boots) return;
    this.settle(m);
  }

  private draw(key: string, fn: Listener): void {
    try {
      fn(this.views.get(key));
    } catch (e) {
      this.errors.push(`${key}: ${e instanceof Error ? e.stack ?? e.message : e}`);
      console.error(e);
    }
  }

  private settle(m: Out): void {
    if (m.error) {
      console.error("core:", m.error);
      return;
    }
    const keys = Object.keys(m.views);
    // Every changed view lands before any draws, so a listener reading another key sees this reply's.
    for (const key of keys) this.views.set(key, JSON.parse(this.dec.decode(m.views[key])));
    for (const key of keys) for (const fn of this.listeners.get(key) ?? []) this.draw(key, fn);
    for (const req of m.effects) {
      if (this.gone) return;
      void this.effect(req);
    }
  }

  private async effect(req: EffectReq): Promise<void> {
    if ("Timer" in req) return this.timer(req.id, (req.Timer as { after_ms: number }).after_ms);
    if ("CancelTimer" in req) return this.cancelTimer((req.CancelTimer as { timer_id: number }).timer_id);
    if ("LoadBlob" in req) {
      this.post({ t: "blob", id: req.id, name: String((req.LoadBlob as { name: string }).name), at: Date.now() });
      return;
    }
    const gen = this.boots;
    const answer = await this.run(req);
    // A restart boots a new core whose ids start over: an old in-flight answer must not resolve one.
    if (gen !== this.boots) return;
    if (answer && !this.gone) this.input({ resolve: { id: req.id, ...answer } });
  }

  private timer(id: number, afterMs: number): void {
    this.timers.set(
      id,
      setTimeout(() => {
        this.timers.delete(id);
        this.input({ resolve: { id, ok: { fired: true } } });
      }, afterMs),
    );
  }

  private cancelTimer(id: number): void {
    clearTimeout(this.timers.get(id));
    this.timers.delete(id);
  }
}
