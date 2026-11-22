// The minimal web example: pick a demo, see the backend that drew it.
import { type Demo, Renderer } from "./renderer.ts";

const canvas = document.querySelector<HTMLCanvasElement>("#canvas")!;
const info = document.querySelector<HTMLElement>("#info")!;
const params = new URLSearchParams(location.search);
const font = new Uint8Array(await (await fetch("./dist/Arimo-Bold.ttf")).arrayBuffer());

async function show(name: Demo, size: [number, number]): Promise<void> {
  r.resize(...size);
  r.demo(name);
  const t0 = performance.now();
  await r.render();
  info.textContent = `${name}: ${r.backend} | ${r.adapter} | compute ${r.compute} | ${
    (performance.now() - t0).toFixed(1)
  } ms`;
}

canvas.width = 520;
canvas.height = 520;
const r = await Renderer.create(canvas, { font, backend: params.get("backend") ?? undefined });
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
    show(name, [...sizes[name]]).catch((e) => {
      info.textContent = `${name}: ${e instanceof Error ? e.message : String(e)}`;
    });
  });
}
const first = (params.get("demo") as Demo | null) ?? "board";
await show(first, [...sizes[first]]);
