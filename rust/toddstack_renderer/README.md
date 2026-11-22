# toddstack-renderer

One GPU renderer on [wgpu](https://wgpu.rs) for Todd's apps, with a [tiny-skia](https://crates.io/crates/tiny-skia)
CPU backend that is both the last fallback and the pixel reference. Part of
[toddstack](https://github.com/goddtriffin/toddstack). v0, not published.

Two layers share one `Renderer`:

- **High:** a `Scene` of rounded rects, lines and text (one shared glyph atlas).
- **Low:** custom instanced batches (your layout, WGSL and data) and compute `Kernel`s.

## Backend selection

`Renderer::new(Target::offscreen(w, h)).await` takes the first backend in the chain that has a real adapter
and falls through to the CPU. `renderer.backend()` and `renderer.capabilities()` (adapter, compute, storage
buffers, limits) report what you got.

| platform | chain |
|---|---|
| web (wasm) | WebGPU, WebGL2 (wgpu GL), CPU |
| Apple | Metal, CPU |
| Android, Linux | Vulkan, GLES, CPU |
| Windows | Vulkan, DX12, GLES, CPU |

A CPU-type adapter (a software Vulkan) is skipped: the tiny-skia path is the one CPU renderer.
`Options { force, data_texture }` pins a backend; `Options::from_env()` reads `WGPU_BACKEND`
(`vulkan|dx12|metal|gl|cpu`). Tested (Apple M3 Max): Metal, CPU. Untested: Vulkan, DX12, GLES, WebGPU and WebGL2 in a browser (the web
build compiles; no browser run yet). `x86_64-pc-windows-gnu` is clippy-checked, not linked or run.

## API

```rust
let mut r = Renderer::new(Target::offscreen(800, 520)).await?;
let font = r.add_font(ttf_bytes)?;
let mut scene = Scene::new([239, 233, 221, 255]);
scene.rrect(16.0, 16.0, 200.0, 80.0, 12.0, [91, 74, 58, 255]);
scene.line(20.0, 20.0, 200.0, 90.0, 4.0, [214, 90, 60, 255]);
scene.text(font, "Qu", 116.0, 70.0, 46.0, [255, 255, 255, 255]);
let rgba: Vec<u8> = r.render(&scene).await?; // width * height * 4, readback included
```

**Custom batch** (`BatchDesc`): `wgsl` with entry points `vs` and `fs`, a `Vec<u32>` of packed instance data,
an instance count, and a `CpuBatch` (any `Fn(&[u32], [f32; 4], &mut Pixmap)`). The renderer prepends a
prelude with `globals` (target size, four params), `to_clip(px)` and `data_at(i)`; `data_at` reads a storage
buffer, or a data texture where storage buffers are missing (WebGL2), so one WGSL runs on both.
`update_batch(id, offset, words)` uploads only that range; `set_batch_params`, `set_batch_instances`.
Draw it in order with `scene.batch(id)`.

**Kernel**: WGSL compute form (inputs at `@binding(0..n)`, output storage texture at `n`), an optional
fragment form (render-to-texture; `Binding::Storage` becomes a uniform there), an optional CPU function.
`run_kernel` picks compute, then fragment, then CPU; `run_kernel_with` forces one. Output is RGBA8 read back.

`Diff::between(a, b)` and `Budget::ARENA` are the pixel-diff budget: more than 1 level on at most 2% of
pixels, more than 8 on 0.6%, more than 32 on 0.05%, max 48 (from boggledygook's renderer arena, which measured
0.909% / 0.258% / 0% / 31 for wgpu on Metal).

Not in v0: image quads, paths and fills, drawing straight to a canvas surface (the web wrapper reads back and
`putImageData`s), reading a kernel output as a scene texture, resizing a batch's data.

## Demos and the parity check

One example, every demo on a GPU backend and on the CPU. It prints the adapter, frame time (render plus
readback) p50 and max over `--frames N` (default 30), writes `out/<demo>-<backend>.png` and
`out/<demo>-cpu.png`, and prints the GPU-vs-CPU diff.

```sh
cd rust
stokehold cargo run --release -p toddstack-renderer --example demo -- all          # board chart grid quantize parity
stokehold cargo run --release -p toddstack-renderer --example demo -- board --backend vulkan --frames 100
WGPU_BACKEND=dx12 stokehold cargo run --release -p toddstack-renderer --example demo -- parity
stokehold cargo test --release -p toddstack-renderer -- --nocapture                 # parity + batch + kernel tests
```

- `--backend` (or `WGPU_BACKEND`) is `vulkan`, `dx12`, `metal`, `gl` or `cpu`.
- `parity` prints `PASS` or `FAIL` plus the diff numbers per backend, `SKIP` if there is no adapter, and exits
  1 on a failure. Without `--backend` it tries every GPU backend in the platform's chain.
- `grid` is a 1000x1000 instanced grid (1M tiles, one packed u32 each) as a custom batch.
- `quantize` is a nearest-palette quantize (cross-stitch style) as a compute kernel, a fragment shader and a
  CPU function; it prints how many pixels differ from the CPU form.
- Windows `x86_64-pc-windows-gnu` needs nothing beyond the Rust toolchain: wgpu loads Vulkan and D3D12 at run time.

## Web

```sh
rust/toddstack_renderer/web/build.sh        # wasm + glue (web/pkg) + example bundle (web/dist), prints sizes
deno run -A jsr:@std/http/file-server rust/toddstack_renderer/web   # then open /index.html
```

The wasm (WebGPU + WebGL2 + CPU, `opt-level z`, `wasm-opt -Oz`, fontdue and tiny-skia included) is 2,225,963 bytes
raw, 689,062 bytes brotli. `web/renderer.ts` is the wrapper (`Renderer.create(canvas, { font })`, `begin`, `rrect`, `line`, `text`,
`demo`, `resize`, `render`); `web/index.html` is the example page. `?backend=cpu|webgl2|webgpu` forces a backend.
