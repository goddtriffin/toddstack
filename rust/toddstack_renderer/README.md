# toddstack-renderer

One GPU renderer on [wgpu](https://wgpu.rs) for Todd's apps, with a [tiny-skia](https://crates.io/crates/tiny-skia)
CPU backend that is both the last fallback and the pixel reference. Part of
[toddstack](https://github.com/goddtriffin/toddstack). Version 0.2.0 (0.1.0 is on crates.io).

Two layers share one `Renderer`:

- **High:** a `Scene` of rounded rects, lines, text (one shared glyph atlas), images and sprites (sub-rects, rotated, tinted) and filled or stroked paths, to one target or many canvases, opaque or transparent.
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
`Options::new().with_force(backend).with_data_texture(bool)` pins a backend (`Options`, `Error`, `Backend` and `Capabilities` are `#[non_exhaustive]`); `Options::from_env()` reads `WGPU_BACKEND`
(`vulkan|dx12|metal|gl|cpu`). Tested: Metal and CPU (Apple M3 Max), WebGPU and WebGL2 in Chrome, Vulkan, DX12 and GLES (RTX 3080). `x86_64-pc-windows-gnu` is clippy-checked, not linked or run.

## API

```rust
let mut r = Renderer::new(Target::offscreen(800, 520)).await?;
let font = r.add_font(ttf_bytes)?;
let mut scene = Scene::new([239, 233, 221, 255]);
scene.rrect(16.0, 16.0, 200.0, 80.0, 12.0, [91, 74, 58, 255]);
scene.line(20.0, 20.0, 200.0, 90.0, 4.0, [214, 90, 60, 255]);
scene.text(font, "Qu", 116.0, 70.0, 46.0, [255, 255, 255, 255]);
let rgba: Vec<u8> = r.render(&scene).await?; // width * height * 4, readback included
// On the web, draw straight to a canvas instead: no readback, no await.
// let mut r = Renderer::new(Target::canvas(html_canvas)).await?; r.present(&scene)?;
```

**Sprites**: `scene.image_rect(image, src, dst, rotation_radians, tint)` draws `src` (`[x, y, w, h]` in image pixels,
whole pixels inside the image; it is clipped to the image) into `dst` (`[x, y, w, h]` in target pixels), rotated clockwise
(y is down) about the centre of `dst`, times `tint` (straight RGBA: colour into colour, alpha into alpha). Sampling never
leaves `src` (no bleed from a neighbouring sprite on a sheet). Consecutive sprites of one image are one instanced draw, so
thousands per frame are fine; `scene.image` is the whole-image shorthand (no rotation, tint white with the opacity as alpha).
Rotated quad edges are not antialiased (tiny-skia's non-AA fill on the CPU, pixel-centre coverage on the GPU), so a few edge
pixels differ between backends (`Budget::SPRITES`).

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
an instance count, and a `CpuBatch` (any `Fn(&[u32], u32, [f32; 4], &mut Pixmap)`: data, instance count, params, target; draw instances `0..count`, as the GPU's `instance_index` does). The renderer prepends a
prelude with `globals` (target size, four params), `to_clip(px)` and `data_at(i)`; `data_at` reads a storage
buffer, or a data texture where storage buffers are missing (WebGL2), so one WGSL runs on both.
`add_batch` is `async` (see below). `update_batch(id, offset, words)` uploads only that range (a data texture, whole rows holding it); `set_batch_params`, `set_batch_instances` (the count is not checked against the data length: a batch may draw from `instance_index` and `params` alone).
Draw it in order with `scene.batch(id)`.

**Bad WGSL is an `Err`, not a panic.** `add_batch`, `kernel_image`, `kernel_image_with`, `run_kernel` and
`run_kernel_with` compile user WGSL inside a wgpu validation error scope, so a rejected shader or pipeline comes back
as `Error::Shader(message)`. The GPU reports it later, so these are `async` on every platform (native callers
block with `pollster::block_on`).

**Kernel**: WGSL compute form (inputs at `@binding(0..n)`, output storage texture at `n`), an optional
fragment form (render-to-texture; `Binding::Storage` becomes a uniform there), an optional CPU function.
`run_kernel` picks compute, then fragment, then CPU; `run_kernel_with` forces one. Output is RGBA8 read back.

`Diff::between(a, b)` and `Budget::ARENA` are the pixel-diff budget: more than 1 level on at most 2% of
pixels, more than 8 on 0.6%, more than 32 on 0.05%, max 48 (from boggledygook's renderer arena, which measured
0.909% / 0.258% / 0% / 31 for wgpu on Metal).

**Transparent output**: `Options::new().with_transparent(true)`. Canvas surfaces are configured premultiplied (WebGPU),
falling back to inherit, then to whatever the surface offers (WebGL2 offers only one mode and keeps its alpha anyway);
scenes may clear to alpha 0; all GPU blends and the CPU glyph blit keep alpha correct. The CPU `putImageData` gets straight
alpha (unpremultiplied); an opaque output ignores alpha, like an opaque surface. **Readback (`render`) is always
premultiplied RGBA8** on every backend and on native; it equals straight RGBA wherever alpha is 255, and
`unpremultiply_rgba8` converts. `Diff::between_rgba` compares all four channels.

**Many canvases on one renderer**: a page with dozens of boards cannot have a device each.

```rust
let mut r = Renderer::with_options(Target::device_only(), Options::new().with_transparent(true)).await?;
let id: SurfaceId = r.attach(Target::canvas(html_canvas))?;   // or Target::offscreen_canvas
r.present_to(id, &scene)?;
r.resize_surface(id, w, h)?;
r.detach(id)?;
```

`Target::device_only()` (or an offscreen target) picks the backend on a probe before any real canvas is touched; fonts,
images, batches and the glyph atlas are shared by every canvas. WebGPU: one device, one surface per canvas. CPU: each
canvas gets a 2D context and `putImageData`. **WebGL2**: one GL context cannot drive many canvases, so the renderer owns one
internal `OffscreenCanvas` sheet (the probe's surface), draws each frame at its top-left (grown to the largest frame, never
shrunk) and copies it into the canvas with a 2D `drawImage` (composite `copy`, alpha kept). A WebGL2 renderer whose primary
target is a canvas draws to that canvas only and refuses `attach`. The single-target API is unchanged. Ids are never reused.

**Presenting**: `r.present(&scene)` draws with no readback. `Target::canvas(HtmlCanvasElement)` and
`Target::offscreen_canvas(OffscreenCanvas)` (wasm32 only) make a canvas target: the GPU backends draw into the
canvas surface and present it; the CPU backend `putImageData`s into its 2D context. `resize` reconfigures the
surface and sets the canvas size, so pass device pixels. A lost or outdated surface is reconfigured and retried once,
then `Error::Surface`. `render` (readback) is for offscreen targets only, for tests and callers that want pixels.
A canvas has one context for life, so the chain is decided before the real canvas is touched: WebGPU needs no
canvas to probe, WebGL2 is probed on a throwaway 1x1 `OffscreenCanvas` (adapter, software check, device, format), and
only an accepted backend creates its surface on the canvas. Every rejection therefore falls through to the CPU. The one
remaining failure is an accepted backend whose surface (or WebGL2 adapter and device, requested again on the canvas)
then fails on the real canvas: `Error::Surface` or `Error::Device`. Native surfaces from a raw window
handle are left to the iOS and Android integrations.

Not yet: resizing a batch's
data, even-odd fills, gradients, clipping.

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
rust/toddstack_renderer/web/build.sh        # wasm (the `web` example, a cdylib) + glue (web/pkg) + example bundle (web/dist), prints sizes
deno run -A jsr:@std/http/file-server rust/toddstack_renderer/web   # then open /index.html
```

The `WebRenderer` wasm-bindgen class and the `demo` scenes sit behind the cargo feature `web-demo`, off by default, so
consumers compile neither (the crate is `rlib` only, and `wgpu` is not re-exported). The example, tests and `demo`
example get the feature through a self dev-dependency; `build.sh` builds `examples/web.rs` as a cdylib.

Sizes: the wasm (WebGPU + WebGL2 + CPU, `opt-level z`, `wasm-opt -Oz`, fontdue and tiny-skia included) is 2,225,138 bytes
raw, 690,792 bytes brotli (0.1.0: 2,205,571 and 683,190). `web/renderer.ts` is the wrapper (`Renderer.create(canvas, { font })`, `begin`, `rrect`, `line`, `text`,
`demo` (async), `resize`, `render` (presents to the canvas; `{ readback: true }` reads back and `putImageData`s instead); demos `board chart grid images paths texture`); `web/index.html` is the example page. `?backend=cpu|webgl2|webgpu` forces a backend, `?readback=1` times the readback path. `?boards=24` draws 24 transparent board-like tiles with rotated, tinted sprites onto 24 canvases over a CSS gradient on one renderer (`Boards` in `web/renderer.ts`) and prints the median frame time; on an Apple M3 Max in Chrome: WebGPU 1.9 ms, WebGL2 (sheet copy) 2.2 ms, CPU 282 ms per frame of 24 tiles (381 px).

## Changelog

### 0.2.0

Added: `Options::with_transparent`; `Target::device_only`, `Renderer::attach`, `present_to`, `resize_surface`,
`surface_size`, `detach`, `SurfaceId`, `Error::UnknownSurface`; `Scene::image_rect` and `Prim::Sprite`;
`unpremultiply_rgba8`; `Diff::between_rgba`, `Budget::SPRITES`; the `?boards=N` demo.

Breaking:

- `Prim` is now `#[non_exhaustive]` and gained a variant (`Sprite`): matches need a wildcard arm.
- `CpuBatch::draw` (and the closure form) takes the instance count as its second argument:
  `Fn(&[u32], u32, [f32; 4], &mut Pixmap)`. Draw instances `0..count`.
- `Renderer::set_batch_instances` no longer rejects a count above the data length (it returned `BadInput`); it now only
  fails for an unknown batch.
- GPU alpha now accumulates (premultiplied "over"): a target cleared to alpha below 255 is no longer left at the clear
  alpha, the clear colour is premultiplied as on the CPU, and readback is documented premultiplied. Opaque scenes are
  byte-for-byte unchanged in alpha (255).
- The CPU canvas `present` unpremultiplies (transparent) or forces alpha 255 (opaque) before `putImageData`; the WebGL2
  sheet copy forces alpha 255 on an opaque target.
- `Renderer::resize`, `render` and `present` on a `Target::device_only()` renderer return `BadInput`.
- `Renderer::attach` returns `BadInput` for a canvas that is already the target or attached.
- A sprite or image `dst` with a zero or negative width or height draws nothing on every backend.

Notes:

- `Run`/`Inst` internals changed (private): an image draw's `Inst.p.x` is now rotation, not opacity.
- Wasm grew by 7,602 bytes brotli.
