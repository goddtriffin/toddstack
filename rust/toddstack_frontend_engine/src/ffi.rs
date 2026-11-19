//! `UniFFI` glue. [`export_uniffi_core!`] defines the `Core` object the Swift and Kotlin shells
//! call: the same surface as the wasm class, bytes in, bytes out.

#[doc(hidden)]
pub use uniffi;

/// Export `Core` for an [`App`](crate::App), plus `bridge_protocol()`, and set up the `UniFFI`
/// scaffolding. Invoke once, at the root of the crate built as a `staticlib` / `cdylib`, which
/// must depend on `uniffi` at the same exact version as this crate:
///
/// ```ignore
/// toddstack_frontend_engine::export_uniffi_core!(my_app::App);
/// ```
#[macro_export]
macro_rules! export_uniffi_core {
    ($app:ty) => {
        ::uniffi::setup_scaffolding!();

        /// The shared core, as seen from Swift and Kotlin.
        #[derive(::uniffi::Object)]
        pub struct Core {
            inner: ::std::sync::Mutex<$crate::Runtime<$app>>,
        }

        impl Core {
            fn lock(&self) -> ::std::sync::MutexGuard<'_, $crate::Runtime<$app>> {
                self.inner
                    .lock()
                    .unwrap_or_else(::std::sync::PoisonError::into_inner)
            }
        }

        #[expect(
            clippy::needless_pass_by_value,
            reason = "UniFFI hands over owned values; the exported signatures cannot borrow"
        )]
        #[::uniffi::export]
        impl Core {
            /// Boot facts from the shell (JSON).
            #[uniffi::constructor]
            #[must_use]
            pub fn new(boot: Vec<u8>) -> ::std::sync::Arc<Self> {
                ::std::sync::Arc::new(Self {
                    inner: ::std::sync::Mutex::new($crate::Runtime::<$app>::new(&boot)),
                })
            }

            /// One protocol message in (JSON), one response out.
            #[must_use]
            pub fn dispatch(&self, input: Vec<u8>) -> Vec<u8> {
                $crate::Bridge::dispatch(&mut *self.lock(), &input)
            }

            /// Hot path, no JSON. Returns a bitmask: bit 0 dirty views, bit 1 effects pending.
            #[must_use]
            pub fn pointer(&self, phase: u8, x: f32, y: f32, at_ms: f64) -> u32 {
                $crate::Bridge::pointer(&mut *self.lock(), phase, x, y, at_ms)
            }

            /// The JSON of one view key.
            #[must_use]
            pub fn view(&self, key: String) -> Vec<u8> {
                $crate::Bridge::view(&*self.lock(), &key)
            }

            /// Large binary input off the JSON path.
            #[must_use]
            pub fn provide_blob(&self, name: String, bytes: Vec<u8>) -> Vec<u8> {
                $crate::Bridge::provide_blob(&mut *self.lock(), &name, &bytes)
            }
        }

        /// The bridge message format this core speaks.
        #[must_use]
        #[::uniffi::export]
        pub fn bridge_protocol() -> u32 {
            u32::try_from($crate::BRIDGE_PROTOCOL).unwrap_or(0)
        }
    };
}
