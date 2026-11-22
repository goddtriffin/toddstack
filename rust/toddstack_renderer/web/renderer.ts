/**
 * Thin wrapper over the wasm renderer: build a scene with calls, `render()` it onto a canvas.
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
}

/** One renderer bound to a 2D canvas. */
export class Renderer {
  private constructor(
    private readonly core: WebRenderer,
    private readonly ctx: CanvasRenderingContext2D,
  ) {}

  /** Picks the best backend (WebGPU, WebGL2, CPU) and sizes the canvas. */
  static async create(canvas: HTMLCanvasElement, options: Options): Promise<Renderer> {
    await init(options.wasm);
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("the canvas has no 2D context");
    const core = await WebRenderer.create(canvas.width, canvas.height, options.font, options.backend);
    return new Renderer(core, ctx);
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

  /** Resizes the canvas and the render target. */
  resize(width: number, height: number): void {
    this.ctx.canvas.width = width;
    this.ctx.canvas.height = height;
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

  /** Replaces the scene with a built-in demo: `board`, `chart` or `grid`. */
  demo(name: "board" | "chart" | "grid"): void {
    this.core.demo(name);
  }

  /** Draws the scene and puts it on the canvas. */
  async render(): Promise<void> {
    const px = await this.core.render();
    const { width, height } = this.ctx.canvas;
    this.ctx.putImageData(new ImageData(new Uint8ClampedArray(px), width, height), 0, 0);
  }
}
