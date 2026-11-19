/**
 * The messages between the page and the core's worker. The core (wasm) runs in a dedicated
 * module worker; the page draws views and runs effects. Everything crosses as `Uint8Array<ArrayBuffer>`s whose
 * buffers are transferred, never copied: inputs and views are the core's own JSON bytes.
 * @module
 */

/** One effect request from the core, flat: `{"id":3,"Http":{...}}` or an app effect `{"id":4,"Vibrate":{...}}`. */
export interface EffectReq {
  id: number;
  [effect: string]: unknown;
}

/** An answer to an effect: sent back to the core as `{"resolve":{"id":..,"ok":..}}` or `{"err":..}`. */
export type Answer = { ok: unknown } | { err: string };

/** The wasm module's surface (`export_wasm_core!`), as wasm-bindgen generates it. */
export interface WasmCore {
  dispatch(input: Uint8Array<ArrayBuffer>): Uint8Array<ArrayBuffer>;
  pointer(phase: number, x: number, y: number, atMs: number): number;
  view(key: string): Uint8Array<ArrayBuffer>;
  provide_blob(name: string, bytes: Uint8Array<ArrayBuffer>): Uint8Array<ArrayBuffer>;
  free(): void;
}

/** Constructor of the generated `Core` class. */
export type WasmCoreClass = new (boot: Uint8Array<ArrayBuffer>) => WasmCore;

/** Page to worker. Every message is handled in order; one that wants an answer carries a `seq`. */
export type ToWorker =
  /** The compiled module, or why there is none. Sent once, by the page's head script. */
  | { t: "module"; module?: WebAssembly.Module; error?: string }
  /** Ask again for `ready` or `fail` (the answer may have been sent before the page script ran). */
  | { t: "hello" }
  /** Boot a new core from these facts, dropping the old one; replies the views of every watched key. */
  | { t: "boot"; bytes: Uint8Array<ArrayBuffer>; seq?: number }
  /** Draw these keys now and whenever they change: replies their views. */
  | { t: "watch"; keys: string[] }
  /** One protocol input (`dispatch`). */
  | { t: "input"; bytes: Uint8Array<ArrayBuffer>; seq?: number }
  /** One pointer sample (typed, no JSON); with no `seq`, a flush follows when it left something queued. */
  | { t: "pointer"; phase: number; x: number; y: number; at: number; seq?: number }
  /** The core asked for a blob (`LoadBlob`): fetch it in the worker and hand it over. */
  | { t: "blob"; id: number; name: string; at: number }
  /** Hand a blob over directly, and read one view (for drivers and tests). */
  | { t: "provide"; name: string; bytes: Uint8Array<ArrayBuffer>; seq: number }
  | { t: "view"; key: string; seq: number }
  | { t: "free" };

/** Worker to page. */
export type FromWorker =
  | { t: "ready"; bridge_protocol: number }
  | { t: "fail"; error: string }
  | {
    t: "out";
    /** How many boots the worker had handled when it posted this: the page drops a reply from a replaced core. */
    gen: number;
    seq?: number;
    /** The reply's effects, for the page to run. */
    effects: EffectReq[];
    error?: string;
    /** The view of every watched key that changed (or of every watched key, after a boot or a watch). */
    views: Record<string, Uint8Array<ArrayBuffer>>;
    /** The call's own bytes, and a pointer's bitmask (for drivers and tests). */
    raw?: Uint8Array<ArrayBuffer>;
    n?: number;
  };
