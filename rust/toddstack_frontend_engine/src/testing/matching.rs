use serde_json::Value;

/// Differences where `expected` is a subset of `actual`: objects match by the keys `expected`
/// names, arrays elementwise with equal length, scalars exactly.
#[must_use]
pub fn partial_diff(expected: &Value, actual: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(expected, actual, "$", false, &mut out);
    out
}

/// Differences between two values that must be identical.
#[must_use]
pub fn exact_diff(expected: &Value, actual: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(expected, actual, "$", true, &mut out);
    out
}

fn walk(exp: &Value, act: &Value, path: &str, exact: bool, out: &mut Vec<String>) {
    match (exp, act) {
        (Value::Object(e), Value::Object(a)) => {
            for (k, ev) in e {
                match a.get(k) {
                    Some(av) => walk(ev, av, &format!("{path}.{k}"), exact, out),
                    None => out.push(format!("{path}.{k}: expected {ev}, missing")),
                }
            }
            if exact {
                for (k, av) in a {
                    if !e.contains_key(k) {
                        out.push(format!("{path}.{k}: unexpected {av}"));
                    }
                }
            }
        }
        (Value::Array(e), Value::Array(a)) => {
            if e.len() != a.len() {
                out.push(format!(
                    "{path}: expected {} items, got {} ({act})",
                    e.len(),
                    a.len()
                ));
                return;
            }
            for (i, (ev, av)) in e.iter().zip(a).enumerate() {
                walk(ev, av, &format!("{path}[{i}]"), exact, out);
            }
        }
        _ if exp == act => {}
        _ => out.push(format!("{path}: expected {exp}, got {act}")),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn partial_ignores_extra_keys_but_exact_does_not() {
        let act = json!({"a": 1, "b": [1, 2]});
        assert_eq!(
            partial_diff(&json!({"b": [1, 2]}), &act),
            Vec::<String>::new()
        );
        assert_eq!(
            exact_diff(&json!({"b": [1, 2]}), &act),
            ["$.a: unexpected 1"]
        );
        assert_eq!(
            partial_diff(&json!({"b": [1, 3]}), &act),
            ["$.b[1]: expected 3, got 2"]
        );
    }
}
