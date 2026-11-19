use std::io::{BufRead, Write};

use serde_json::{Value, json};

use super::session::{Factory, HttpHandler, Session};

/// The NDJSON debug protocol over any reader and writer (a CLI wires stdin and stdout).
///
/// Lines: `{"boot":{...}}` (optional; default `{"v":1}`), `{"advance_ms":n}`, or any protocol
/// input. Every response, including those caused by answered effects, is one output line.
///
/// # Errors
/// When reading or writing fails.
pub fn run(
    factory: Factory<'_>,
    new_http: &dyn Fn() -> Box<dyn HttpHandler>,
    seed: u64,
    input: impl BufRead,
    mut out: impl Write,
) -> Result<(), String> {
    let mut session: Option<Session> = None;
    for line in input.lines() {
        let line: String = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let input: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                emit(&mut out, &json!({"error": format!("bad JSON: {e}")}))?;
                continue;
            }
        };
        let boot_arg: Option<Value> = input.get("boot").cloned();
        if session.is_none() || boot_arg.is_some() {
            let boot: Value = boot_arg
                .clone()
                .unwrap_or_else(|| json!({"v": crate::BRIDGE_PROTOCOL}));
            match Session::new(factory, &boot, seed, new_http()) {
                Ok(s) => session = Some(s),
                Err(e) => {
                    emit(&mut out, &json!({"error": e}))?;
                    continue;
                }
            }
            if boot_arg.is_some() {
                continue;
            }
        }
        let Some(s) = session.as_mut() else { continue };
        let result: Result<Vec<Value>, String> =
            match input.get("advance_ms").and_then(Value::as_u64) {
                Some(ms) => s.advance_ms(ms),
                None => s.send(input),
            };
        match result {
            Ok(responses) => {
                for r in &responses {
                    emit(&mut out, r)?;
                }
            }
            Err(e) => emit(&mut out, &json!({"error": e}))?,
        }
    }
    Ok(())
}

fn emit(out: &mut impl Write, v: &Value) -> Result<(), String> {
    writeln!(out, "{v}")
        .and_then(|()| out.flush())
        .map_err(|e| e.to_string())
}
