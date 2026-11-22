//! The example page's wasm: re-exports `WebRenderer` and `WebBoards`, built as a cdylib by `web/build.sh`.
#![cfg(target_arch = "wasm32")]

pub use toddstack_renderer::{WebBoards, WebRenderer};
