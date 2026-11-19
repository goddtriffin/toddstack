//! wasm-bindgen glue. [`export_wasm_core!`] defines the `Core` class a module worker instantiates
//! (see `js/worker.ts`), so every app gets the same surface: boot bytes in, `dispatch`,
//! `pointer`, `view` and `provide_blob`, bytes out.

#[doc(hidden)]
pub use wasm_bindgen as bindgen;

/// Export `Core` for an [`App`](crate::App), plus `bridge_protocol()`. Invoke once, in the crate
/// built as a `cdylib` for `wasm32-unknown-unknown`:
///
/// ```ignore
/// toddstack_frontend_engine::export_wasm_core!(my_app::App);
/// ```
#[macro_export]
macro_rules! export_wasm_core {
    ($app:ty) => {
        /// The shared core, as seen from JavaScript.
        #[$crate::wasm::bindgen::prelude::wasm_bindgen(wasm_bindgen = $crate::wasm::bindgen)]
        pub struct Core {
            inner: $crate::Runtime<$app>,
        }

        #[$crate::wasm::bindgen::prelude::wasm_bindgen(wasm_bindgen = $crate::wasm::bindgen)]
        impl Core {
            /// Boot facts from the shell (JSON bytes).
            #[wasm_bindgen(constructor)]
            #[must_use]
            pub fn new(boot: &[u8]) -> Core {
                Core {
                    inner: $crate::Runtime::<$app>::new(boot),
                }
            }

            /// One protocol message in (JSON), one response out.
            pub fn dispatch(&mut self, input: &[u8]) -> Vec<u8> {
                $crate::Bridge::dispatch(&mut self.inner, input)
            }

            /// Hot path, no JSON. Returns a bitmask: bit 0 dirty views, bit 1 effects pending.
            pub fn pointer(&mut self, phase: u8, x: f32, y: f32, at_ms: f64) -> u32 {
                $crate::Bridge::pointer(&mut self.inner, phase, x, y, at_ms)
            }

            /// The JSON of one view key.
            #[must_use]
            pub fn view(&self, key: &str) -> Vec<u8> {
                $crate::Bridge::view(&self.inner, key)
            }

            /// Large binary input off the JSON path.
            pub fn provide_blob(&mut self, name: &str, bytes: &[u8]) -> Vec<u8> {
                $crate::Bridge::provide_blob(&mut self.inner, name, bytes)
            }
        }

        /// The bridge message format this core speaks.
        #[$crate::wasm::bindgen::prelude::wasm_bindgen(wasm_bindgen = $crate::wasm::bindgen)]
        #[must_use]
        pub fn bridge_protocol() -> u32 {
            u32::try_from($crate::BRIDGE_PROTOCOL).unwrap_or(0)
        }
    };
}
