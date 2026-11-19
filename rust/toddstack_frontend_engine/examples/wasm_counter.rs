//! Build check for the wasm glue: `cargo build --target wasm32-unknown-unknown
//! --features wasm,example --example wasm_counter`.

toddstack_frontend_engine::export_wasm_core!(toddstack_frontend_engine::example::Counter);
