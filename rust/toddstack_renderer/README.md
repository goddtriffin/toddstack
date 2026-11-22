# toddstack-renderer

One GPU renderer on [wgpu](https://wgpu.rs) for Todd's apps, with a [tiny-skia](https://crates.io/crates/tiny-skia)
CPU backend that is both the last fallback and the pixel reference. Part of
[toddstack](https://github.com/goddtriffin/toddstack). v0, not published.

Two layers share one `Renderer`:

- **High:** a `Scene` of rounded rects, lines, text (one shared glyph atlas), image quads and filled or stroked paths.
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

**Images**: `r.add_image(w, h, rgba8, Sampling::Linear | Nearest)` takes straight RGBA8; `scene.image(id, x, y, w, h,
opacity)` stretches it over a rect (bilinear for avatars and scaled icons, nearest for pixel art and exact-size icons;
no mipmaps, so pre-size icons that shrink a lot). `r.remove_image(id)` frees it.

**Kernel output as a scene image**: `r.kernel_image(&kernel, sampling)` runs the kernel (compute, then fragment, then
CPU) and keeps the output as an image. On a GPU form the texture stays on the GPU (no readback); the CPU form uploads
its bytes. `kernel_image_with` forces a form. Draw it with `scene.image`. A kernel's output is a texture, not a buffer.

**Paths**: the path type is `tiny_skia::Path` (re-exported; `PathBuilder` has move, line, quad, cubic and close);
`toddstack_renderer::rounded_rect(x, y, w, h, r)` makes a rounded-rect path. `scene.fill_path(&path, color)` fills
non-zero winding (opposite winding makes a hole); `scene.stroke_path(&path, &Stroke, color)` strokes with any
`tiny_skia::Stroke` (width, butt/round/square caps, miter/round/bevel joins, dashes). Both are antialiased. The CPU
fills tiny-skia's way. The GPU strokes with tiny-skia's stroker (so outlines are identical), flattens curves to 0.05 px,
and fills by stencil-then-cover into a 2x-size, 4x-multisampled transparent layer (16 samples a pixel; 4 when the
target is over half the device's texture limit) that is averaged down and composited as one quad. Each run of
consecutive paths is one layer pass; text, shapes, images and batches drawn between runs keep paint order.

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

Not in v0: drawing straight to a canvas surface (the web wrapper reads back and `putImageData`s), resizing a batch's
data, image sub-rects (sprite sheets), even-odd fills, gradients, clipping.

## Demos and the parity check

One example, every demo on a GPU backend and on the CPU. It prints the adapter, frame time (render plus
readback) p50 and max over `--frames N` (default 30), writes `out/<demo>-<backend>.png` and
`out/<demo>-cpu.png`, and prints the GPU-vs-CPU diff.

```sh
cd rust
stokehold cargo run --release -p toddstack-renderer --example demo -- all          # board chart grid images paths texture quantize parity
stokehold cargo run --release -p toddstack-renderer --example demo -- board --backend vulkan --frames 100
stokehold cargo run --release -p toddstack-renderer --example demo -- parity paths images texture   # several, in order
WGPU_BACKEND=dx12 stokehold cargo run --release -p toddstack-renderer --example demo -- parity
stokehold cargo test --release -p toddstack-renderer -- --nocapture                 # parity + batch + kernel tests
```

- `--backend` (or `WGPU_BACKEND`; the flag wins) is `vulkan`, `dx12`, `metal`, `gl` or `cpu`; an unknown name exits 2.
- Name several demos to run them in order. `images`, `paths` and `texture` check their budget like `parity` and exit 1 on a breach.
- `parity` prints `PASS` or `FAIL` plus the diff numbers per backend, `SKIP` if there is no adapter, and exits
  1 on a failure. Without `--backend` it tries every GPU backend in the platform's chain.
- `grid` is a 1000x1000 instanced grid (1M tiles, one packed u32 each) as a custom batch.
- `images`, `paths` and `texture` cover image quads, paths and a kernel's output as a scene image; the parity test
  holds each to a budget (`Budget::ARENA` for images, `Budget::PATHS` and `Budget::KERNEL`, in `diff.rs`).
- `quantize` is a nearest-palette quantize (cross-stitch style) as a compute kernel, a fragment shader and a
  CPU function; it prints how many pixels differ from the CPU form.
- Windows `x86_64-pc-windows-gnu` needs nothing beyond the Rust toolchain: wgpu loads Vulkan and D3D12 at run time.

## Web

```sh
rust/toddstack_renderer/web/build.sh        # wasm + glue (web/pkg) + example bundle (web/dist), prints sizes
deno run -A jsr:@std/http/file-server rust/toddstack_renderer/web   # then open /index.html
```

The wasm (WebGPU + WebGL2 + CPU, `opt-level z`, `wasm-opt -Oz`, fontdue and tiny-skia included) is 2,278,206 bytes
raw, 705,859 bytes brotli (689,062 before images, paths and kernel textures). `web/renderer.ts` is the wrapper (`Renderer.create(canvas, { font })`, `begin`, `rrect`, `line`, `text`,
`demo`, `resize`, `render`; demos `board chart grid images paths texture`); `web/index.html` is the example page. `?backend=cpu|webgl2|webgpu` forces a backend.
