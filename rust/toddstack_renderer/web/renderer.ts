/**
 * Thin wrapper over the wasm renderer: build a scene with calls, `render()` it onto a canvas.
 * The GPU backends draw straight to the canvas; there is no readback unless you ask for it.
 * Works from plain TypeScript and Deno projects; the wasm glue is `./pkg` (see `build.sh`).
 * @module
 */

import init, { WebRenderer } from "./pkg/toddstack_renderer.js";

/** R, G, B and an optional alpha, 0-255. */
export type Color = readonly [number, number, number, number?];

/** Options for {@link Renderer.create}. */
export interface Options {
  /** TTF or OTF bytes for text. */
  font: Uint8Array;
  /** Force `webgpu`, `webgl2` or `cpu`; default is the fallback chain. */
  backend?: string;
  /** Where the `.wasm` is, when it is not next to the glue. */
  wasm?: URL | string;
  /**
   * Draw offscreen, read the pixels back and `putImageData` them: the old path, for timing the
   * readback. The default draws straight to the canvas with no readback.
   */
  readback?: boolean;
}

/** The built-in demos. */
export type Demo = "board" | "chart" | "grid" | "images" | "paths" | "texture";

/** One renderer bound to a canvas (or an `OffscreenCanvas` in a worker). */
export class Renderer {
  private constructor(
    private readonly core: WebRenderer,
    private readonly canvas: HTMLCanvasElement | OffscreenCanvas,
    private readonly ctx: CanvasRenderingContext2D | OffscreenCanvasRenderingContext2D | null,
  ) {}

  /**
   * Picks the best backend (WebGPU, WebGL2, CPU) and draws to `canvas` at its current
   * `width` x `height`. The renderer takes the canvas's context, so nothing else may draw on it.
   */
  static async create(
    canvas: HTMLCanvasElement | OffscreenCanvas,
    options: Options,
  ): Promise<Renderer> {
    await init(options.wasm);
    if (options.readback) {
      const ctx = canvas.getContext("2d");
      if (!ctx) throw new Error("the canvas has no 2D context");
      const core = await WebRenderer.createReadback(canvas.width, canvas.height, options.font, options.backend);
      return new Renderer(core, canvas, ctx);
    }
    const core = canvas instanceof HTMLCanvasElement
      ? await WebRenderer.create(canvas, options.font, options.backend)
      : await WebRenderer.createOffscreen(canvas, options.font, options.backend);
    return new Renderer(core, canvas, null);
  }

  /** The backend in use: `webgpu`, `webgl2` or `cpu`. */
  get backend(): string {
    return this.core.backend();
  }

  /** Adapter name and driver. */
  get adapter(): string {
    return this.core.adapter();
  }

  /** Whether compute kernels can run. */
  get compute(): boolean {
    return this.core.compute();
  }

  /** True while `render()` is pending; the wasm object is borrowed then. */
  private rendering = false;

  private idle(): void {
    if (this.rendering) throw new Error("a render is still in flight; await it first");
  }

  /** Resizes the canvas (in device pixels) and the render target. */
  resize(width: number, height: number): void {
    this.idle();
    this.canvas.width = width;
    this.canvas.height = height;
    this.core.resize(width, height);
  }

  /** Starts a new, empty scene. */
  begin(clear: Color = [255, 255, 255]): void {
    this.core.begin(clear[0], clear[1], clear[2], clear[3] ?? 255);
  }

  /** Rounded rectangle; radius 0 is a plain rectangle. */
  rrect(x: number, y: number, w: number, h: number, r: number, c: Color): void {
    this.core.rrect(x, y, w, h, r, c[0], c[1], c[2], c[3] ?? 255);
  }

  /** Line with round caps. */
  line(x0: number, y0: number, x1: number, y1: number, width: number, c: Color): void {
    this.core.line(x0, y0, x1, y1, width, c[0], c[1], c[2], c[3] ?? 255);
  }

  /** Text centred on `x`, on baseline `y`. */
  text(text: string, x: number, y: number, size: number, c: Color): void {
    this.core.text(text, x, y, size, c[0], c[1], c[2], c[3] ?? 255);
  }

  /** Replaces the scene with a built-in demo. */
  demo(name: Demo): void {
    this.idle();
    this.core.demo(name);
  }

  /** Draws the scene onto the canvas. Without `readback` this is one synchronous GPU submit. */
  async render(): Promise<void> {
    this.idle();
    if (!this.ctx) {
      this.core.present();
      return;
    }
    this.rendering = true;
    let px: Uint8Array;
    try {
      px = await this.core.render();
    } finally {
      this.rendering = false;
    }
    const { width, height } = this.canvas;
    this.ctx.putImageData(new ImageData(new Uint8ClampedArray(px), width, height), 0, 0);
  }
}
