// The minimal web example: pick a demo, see the backend that drew it.
import { Boards, type Demo, Renderer } from "./renderer.ts";

const canvas = document.querySelector<HTMLCanvasElement>("#canvas")!;
const info = document.querySelector<HTMLElement>("#info")!;
const params = new URLSearchParams(location.search);
const font = new Uint8Array(await (await fetch("./dist/Arimo-Bold.ttf")).arrayBuffer());

// Test only, this page only: `?reject=webgpu,webgl2` makes those backends look unusable, to prove the
// fallback still reaches the CPU on the same canvas. The library has no such hook.
const reject = (params.get("reject") ?? "").split(",");
if (reject.includes("webgpu")) {
  Object.defineProperty(navigator, "gpu", { value: undefined, configurable: true });
}
if (reject.includes("webgl2")) {
  const getParameter = WebGL2RenderingContext.prototype.getParameter;
  WebGL2RenderingContext.prototype.getParameter = function (pname: number) {
    // UNMASKED_RENDERER_WEBGL and RENDERER: look like a software rasterizer.
    return pname === 0x9246 || pname === 0x1f01 ? "Google SwiftShader" : getParameter.call(this, pname);
  } as typeof getParameter;
}

// `?boards=N`: N transparent board tiles on N canvases over a CSS gradient, one renderer for all.
const boardCount = Number(params.get("boards") ?? 0);

async function singlePage(): Promise<void> {
  const readback = params.get("readback") === "1";
  let current: Demo = "board";

  async function show(name: Demo, size: readonly [number, number]): Promise<void> {
    current = name;
    r.resize(...size);
    // One canvas pixel per device pixel, so it stays crisp at any zoom or screen density.
    canvas.style.width = `${size[0] / devicePixelRatio}px`;
    await r.demo(name);
    const t0 = performance.now();
    await r.render();
    const ms = performance.now() - t0;
    const how = readback ? "render + readback" : "present (submit)";
    info.textContent = `${name}: ${r.backend} | ${r.adapter} | compute ${r.compute} | ${how} ${ms.toFixed(1)} ms`;
    Object.assign(globalThis, { lastFrame: { name, backend: r.backend, ms, readback } });
  }

  canvas.width = 520;
  canvas.height = 520;
  const r = await Renderer.create(canvas, { font, backend: params.get("backend") ?? undefined, readback });
  const sizes = {
    board: [520, 520],
    chart: [640, 400],
    grid: [1000, 1000],
    images: [640, 260],
    paths: [640, 400],
    texture: [640, 270],
  } as const;
  for (const name of Object.keys(sizes) as Demo[]) {
    document.querySelector(`#${name}`)!.addEventListener("click", () => {
      show(name, sizes[name]).catch((e) => {
        info.textContent = `${name}: ${e instanceof Error ? e.message : String(e)}`;
      });
    });
  }
  // A window resize or zoom changes the device pixel ratio: redraw at the new density.
  addEventListener("resize", () => {
    show(current, sizes[current]).catch((e) => {
      info.textContent = `${current}: ${e instanceof Error ? e.message : String(e)}`;
    });
  });
  const first = (params.get("demo") as Demo | null) ?? "board";
  await show(first, sizes[first]);
}

/** Median of the samples, in ms. */
function median(ms: number[]): number {
  const sorted = [...ms].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)];
}

async function boardsPage(count: number): Promise<void> {
  document.body.classList.add("boards");
  const host = document.querySelector<HTMLElement>("#boards")!;
  const canvases: HTMLCanvasElement[] = [];
  for (let i = 0; i < count; i++) {
    const c = document.createElement("canvas");
    host.append(c);
    canvases.push(c);
  }
  // One canvas pixel per device pixel; the tiles are square.
  const pixels = () => Math.max(1, Math.round(canvases[0].getBoundingClientRect().width * devicePixelRatio));
  for (const c of canvases) c.width = c.height = pixels();
  const boards = await Boards.create({ font, backend: params.get("backend") ?? undefined });
  const ids = canvases.map((c) => boards.attach(c));
  const drawAll = (t: number) => ids.forEach((id) => boards.frame(id, t));
  const run = () => {
    const size = pixels();
    ids.forEach((id) => boards.resize(id, size, size));
    for (let i = 0; i < 3; i++) drawAll(i / 10); // warm up: pipelines and glyphs
    const samples: number[] = [];
    for (let i = 0; i < 20; i++) {
      const t0 = performance.now();
      drawAll(i / 10);
      samples.push(performance.now() - t0);
    }
    drawAll(0);
    const ms = median(samples);
    info.textContent = `boards=${count}: ${boards.backend} | ${boards.adapter} | ${size}px tiles | ${ms.toFixed(2)} ms per frame of ${count} (p50 of 20)`;
    Object.assign(globalThis, { lastFrame: { boards: count, backend: boards.backend, ms, size } });
  };
  run();
  addEventListener("resize", run);
}

if (boardCount > 0) {
  await boardsPage(boardCount).catch((e) => {
    info.textContent = `boards: ${e instanceof Error ? e.message : String(e)}`;
  });
} else {
  await singlePage();
}
