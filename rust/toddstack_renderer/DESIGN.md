# toddstack-renderer: fallback design

## Chain per platform

| platform | order |
|---|---|
| web | WebGPU, WebGL2, CPU (tiny-skia to a 2D canvas) |
| Apple | Metal (through wgpu), CPU |
| Android, Linux | Vulkan, GLES, CPU |
| Windows | Vulkan, DX12, GLES, CPU |

`Backend::chain()` holds the order. `Options::with_force(b)` runs only `b`.

## When a backend fails

Only on a real failure, never on a preference or a heuristic:

| reason | meaning |
|---|---|
| `NoAdapter` | the backend has no adapter |
| `Timeout` | WebGPU `requestAdapter` did not answer in the adapter timeout |
| `RequestDevice` | the adapter refused the device |
| `Surface` | the surface or canvas context could not be created or configured |
| `DeviceLost` | the device was lost while running |
| `Rejected` | an adapter exists but cannot be used (software adapter, surface cannot take the format) |

Bad input (an empty or oversized target) is not a backend failure: it is returned as is, with no fallback.

## Events

`Options::with_fallback_callback` is called for every step: `FallbackEvent { from, to, reason }`. When the whole
chain fails the last event has `to: None` and the call returns `Error::NoRendererAvailable { attempts }`. The crate
has no Sentry dependency; the host maps events to Sentry (warning for a step down, high severity for `to: None`).

## Timeout

The WebGPU adapter request races a `setTimeout` promise (no new dependency; works in workers). Default 2 s,
`Options::with_adapter_timeout`. Only WebGPU on the web is bounded. On timeout: `Timeout`, then WebGL2.

## Memory

The crate persists nothing. `Options::with_preferred(b)` starts the chain at `b` if the platform has it; if it
fails, the full chain runs from the top (`b` is not tried twice). The host stores `renderer.backend()` per device and
drops it when `MEMORY_VERSION` (`major.minor`) changes. Key and format: README.

## Device loss (#96)

wgpu's device-lost callback sets a flag. The next draw returns `Error::DeviceLost`, and one `DeviceLost` event
(`to: None`) is sent. The renderer does not rebuild itself: its fonts, images, batches and canvases would all need
re-creating, and the host owns that. The host builds a new `Renderer` (same options after `destroyed`; the next
backend via `with_preferred` after `unknown`). In-renderer rebuild is future work.

## Tests

The chain walk is generic over the attempt, so unit tests inject failing backends: forced failures, a simulated
timeout, a preferred start, all failing (`NoRendererAvailable` plus the final event). `REQUIRE_GPU=1` makes the parity
test fail if no GPU backend had an adapter. A browser test of the real web fallbacks (#97) is future work.
