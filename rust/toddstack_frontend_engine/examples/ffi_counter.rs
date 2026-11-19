//! Build check for the `UniFFI` glue: `cargo build --features ffi,example --example ffi_counter`.

toddstack_frontend_engine::export_uniffi_core!(toddstack_frontend_engine::example::Counter);
