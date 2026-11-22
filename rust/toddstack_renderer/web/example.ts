// The minimal web example: pick a demo, see the backend that drew it.
import { Renderer } from "./renderer.ts";

const canvas = document.querySelector<HTMLCanvasElement>("#canvas")!;
const info = document.querySelector<HTMLElement>("#info")!;
const params = new URLSearchParams(location.search);
const font = new Uint8Array(await (await fetch("./dist/Arimo-Bold.ttf")).arrayBuffer());

async function show(name: "board" | "chart" | "grid", size: [number, number]): Promise<void> {
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
const sizes = { board: [520, 520], chart: [640, 400], grid: [1000, 1000] } as const;
for (const name of ["board", "chart", "grid"] as const) {
  document.querySelector(`#${name}`)!.addEventListener("click", () => show(name, [...sizes[name]]));
}
await show((params.get("demo") as "board" | "chart" | "grid" | null) ?? "board", [
  ...sizes[(params.get("demo") as "board") ?? "board"],
]);
