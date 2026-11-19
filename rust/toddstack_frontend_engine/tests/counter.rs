#![cfg(all(feature = "testing", feature = "example"))]

use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use toddstack_frontend_engine::example::Counter;
use toddstack_frontend_engine::testing::{
    HttpResponse, Options, Script, Session, factory, run_file, run_repl,
};
use toddstack_frontend_engine::{BRIDGE_PROTOCOL, Bridge, Runtime};

type Rt = Runtime<Counter>;

fn call(rt: &mut Rt, input: &Value) -> Value {
    serde_json::from_slice(&rt.dispatch(input.to_string().as_bytes())).unwrap()
}

fn session(script: Script) -> Session {
    let boot: Value = json!({"v": BRIDGE_PROTOCOL, "at": 100});
    Session::new(&factory::<Counter>, &boot, 7, Box::new(script)).unwrap()
}

#[test]
fn boot_effects_wait_for_the_first_flush_and_an_act_marks_its_view_dirty() {
    let mut rt: Rt = Rt::new(br#"{"v":1,"at":10}"#);
    let boot: Value = call(&mut rt, &json!({"flush": {}}));
    assert_eq!(
        json!({"id": 1, "Store": {"key": "example.count"}}),
        boot["effects"][0]
    );
    assert_eq!(json!(["counter"]), boot["dirty"]);
    let acted: Value = call(&mut rt, &json!({"at": 11, "act": {"Add": {"n": 5}}}));
    assert_eq!(json!(["counter"]), acted["dirty"]);
    let view: Value = call(&mut rt, &json!({"view": "counter"}));
    assert_eq!(5, view["json"]["count"]);
}

#[test]
fn an_unchanged_view_is_not_reported_dirty_again() {
    let mut rt: Rt = Rt::new(b"{}");
    call(&mut rt, &json!({"flush": {}}));
    let first: Value = call(&mut rt, &json!({"act": {"Add": {"n": 0}}}));
    let second: Value = call(&mut rt, &json!({"act": {"Add": {"n": 0}}}));
    assert_eq!(json!([]), first["dirty"]);
    assert_eq!(json!([]), second["dirty"]);
    let changed: Value = call(&mut rt, &json!({"act": {"Add": {"n": 1}}}));
    assert_eq!(json!(["counter"]), changed["dirty"]);
}

#[test]
fn the_typed_pointer_path_returns_a_mask_and_queues_until_flushed() {
    let mut rt: Rt = Rt::new(b"{}");
    call(&mut rt, &json!({"flush": {}}));
    assert_eq!(0, rt.pointer(0, 1.0, 2.0, 5.0));
    assert_eq!(1, rt.pointer(2, 1.0, 2.0, 6.0));
    let flushed: Value = call(&mut rt, &json!({"flush": {}}));
    assert_eq!(json!(["counter"]), flushed["dirty"]);
    assert_eq!(json!([]), flushed["effects"]);
}

#[test]
fn bad_input_is_an_error_response_not_a_panic() {
    let mut rt: Rt = Rt::new(b"{}");
    assert!(call(&mut rt, &json!({"act": "Nope"}))["error"].is_string());
    assert!(call(&mut rt, &json!({"nonsense": 1}))["error"].is_string());
    assert!(
        serde_json::from_slice::<Value>(&rt.dispatch(b"not json")).unwrap()["error"].is_string()
    );
}

#[test]
fn every_response_carries_the_bridge_protocol_version() {
    let mut rt: Rt = Rt::new(b"{}");
    assert_eq!(
        json!(BRIDGE_PROTOCOL),
        call(&mut rt, &json!({"flush": {}}))["v"]
    );
    assert_eq!(
        json!(BRIDGE_PROTOCOL),
        call(&mut rt, &json!({"act": "Nope"}))["v"]
    );
}

#[test]
fn introspection_lists_the_legal_actions_with_schemas_and_follows_the_state() {
    let mut rt: Rt = Rt::new(b"{}");
    let idle: Value = call(&mut rt, &json!({"introspect": {}}));
    let names = |v: &Value| -> Vec<String> {
        v["actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!("counter", idle["screen"]);
    assert_eq!(vec!["Inc", "Add", "Reset", "Fetch"], names(&idle));
    assert_eq!(
        json!({"type": "object", "properties": {"n": {"type": "integer"}}, "required": ["n"]}),
        idle["actions"][1]["schema"]
    );
    call(&mut rt, &json!({"act": "Fetch"}));
    let fetching: Value = call(&mut rt, &json!({"introspect": {}}));
    assert_eq!(vec!["Inc", "Add", "Reset"], names(&fetching));
}

#[test]
fn a_second_fetch_while_one_is_in_flight_is_ignored() {
    let mut rt: Rt = Rt::new(b"{}");
    call(&mut rt, &json!({"flush": {}}));
    let first: Value = call(&mut rt, &json!({"act": "Fetch"}));
    let second: Value = call(&mut rt, &json!({"act": "Fetch"}));
    assert_eq!(1, first["effects"].as_array().unwrap().len());
    assert_eq!(json!([]), second["effects"]);
}

#[test]
fn an_answer_to_an_unknown_effect_id_changes_nothing() {
    let mut rt: Rt = Rt::new(b"{}");
    call(&mut rt, &json!({"flush": {}}));
    let stale: Value = call(&mut rt, &json!({"resolve": {"id": 99, "ok": 5}}));
    assert_eq!(json!([]), stale["dirty"]);
    assert_eq!(json!([]), stale["effects"]);
}

#[test]
fn the_saved_count_restored_by_the_boot_read_shows_in_the_view() {
    let mut rt: Rt = Rt::new(b"{}");
    call(&mut rt, &json!({"flush": {}}));
    call(&mut rt, &json!({"resolve": {"id": 1, "ok": 7}}));
    assert_eq!(
        7,
        call(&mut rt, &json!({"view": "counter"}))["json"]["count"]
    );
}

#[test]
fn an_unknown_view_key_is_null() {
    let rt: Rt = Rt::new(b"{}");
    assert_eq!(b"null".to_vec(), rt.view("nope"));
}

#[test]
fn the_session_answers_store_effects_from_its_fake_world() {
    let mut s: Session = session(Script::default());
    s.send(json!({"act": {"Add": {"n": 3}}})).unwrap();
    assert_eq!(Some(&json!(3)), s.world.store.get("example.count"));
}

#[test]
fn an_answerer_resolves_the_apps_own_effects_and_unanswered_ones_are_only_recorded() {
    let mut recorded: Session = session(Script::default());
    recorded.send(json!({"act": "Inc"})).unwrap();
    assert!(recorded.effects.iter().any(|e| e["Vibrate"]["ms"] == 10));

    let mut answered: Session = session(Script::default());
    answered.answerer = Some(|effect, world| {
        effect.get("Vibrate").map(|_| {
            world.store.insert("vibrated".to_owned(), json!(true));
            Ok(Value::Null)
        })
    });
    answered.send(json!({"act": "Inc"})).unwrap();
    assert_eq!(Some(&json!(true)), answered.world.store.get("vibrated"));
}

#[test]
fn an_unscripted_route_is_a_failure_in_strict_mode_and_an_error_answer_otherwise() {
    let mut lenient: Session = session(Script::default());
    lenient.send(json!({"act": "Fetch"})).unwrap();
    let mut strict: Session = session(Script::default());
    strict.strict = true;
    let err: String = strict.send(json!({"act": "Fetch"})).unwrap_err();
    assert_eq!("no scripted http for `GET /api/count`", err);
}

#[test]
fn a_slow_answer_reaches_the_app_only_when_the_clock_catches_up() {
    let mut script: Script = Script::default();
    script.add(
        "GET /api/count",
        vec![HttpResponse {
            status: 200,
            body: json!({"count": 9}),
            latency_ms: 0,
            delay_ms: 300,
        }],
    );
    let mut s: Session = session(script);
    s.send(json!({"act": "Fetch"})).unwrap();
    s.advance_ms(299).unwrap();
    let before: Value = serde_json::from_slice(&s.bridge().view("counter")).unwrap();
    assert_eq!(json!({"Fetching": {"attempt": 0}}), before["sync"]);
    s.advance_ms(1).unwrap();
    let after: Value = serde_json::from_slice(&s.bridge().view("counter")).unwrap();
    assert_eq!(json!({"count": 9, "taps": 0, "sync": "Loaded"}), after);
}

#[test]
fn a_scripted_body_can_follow_the_fake_clock() {
    let mut script: Script = Script::default();
    script.add(
        "GET /api/count",
        vec![HttpResponse {
            status: 200,
            body: json!({"count": "$now+5"}),
            latency_ms: 0,
            delay_ms: 0,
        }],
    );
    let mut s: Session = session(script);
    s.send(json!({"act": "Fetch"})).unwrap();
    let view: Value = serde_json::from_slice(&s.bridge().view("counter")).unwrap();
    assert_eq!(json!(105), view["count"]);
}

#[test]
fn a_session_with_the_same_inputs_logs_the_same_responses() {
    let run = || -> Vec<Value> {
        let mut s: Session = session(Script::default());
        s.send(json!({"act": "Inc"})).unwrap();
        s.advance_ms(10).unwrap();
        s.log
    };
    assert_eq!(run(), run());
}

#[test]
fn the_repl_speaks_one_response_per_line() {
    let input: &[u8] = b"{\"boot\":{\"v\":1,\"at\":0}}\n\n{\"act\":\"Inc\"}\nnot json\n";
    let mut out: Vec<u8> = Vec::new();
    run_repl(
        &factory::<Counter>,
        &|| Box::new(Script::default()),
        1,
        input,
        &mut out,
    )
    .unwrap();
    let lines: Vec<Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    // The act, the world's answer to its `Store` effect, then the bad line.
    assert_eq!(3, lines.len());
    assert_eq!(json!(["counter"]), lines[0]["dirty"]);
    assert_eq!(json!([]), lines[1]["dirty"]);
    assert!(lines[2]["error"].as_str().unwrap().starts_with("bad JSON"));
}

fn scenario_options(golden: bool) -> Options {
    Options {
        golden,
        snapshot_root: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots"),
        app_version: "0.0.0".to_owned(),
        answerer: None,
    }
}

fn scenarios() -> Vec<PathBuf> {
    let dir: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/scenarios");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
    files.sort();
    files
}

#[test]
fn every_scenario_passes_and_replays_byte_identically() {
    let golden: bool = std::env::var_os("TFE_GOLDEN").is_some();
    let files: Vec<PathBuf> = scenarios();
    assert_eq!(3, files.len());
    for file in files {
        let first: Vec<Value> = run_file(&file, &factory::<Counter>, &scenario_options(golden))
            .unwrap_or_else(|f| panic!("{f}"));
        let second: Vec<Value> = run_file(&file, &factory::<Counter>, &scenario_options(false))
            .unwrap_or_else(|f| panic!("{f}"));
        assert_eq!(first, second, "{}", file.display());
    }
}

#[test]
fn a_failing_expectation_names_the_file_line_and_difference() {
    let dir: PathBuf = std::env::temp_dir().join("tfe-failing-scenario");
    std::fs::create_dir_all(&dir).unwrap();
    let file: PathBuf = dir.join("bad.jsonl");
    std::fs::write(
        &file,
        "{\"boot\":{\"v\":1}}\n{\"expect\":{\"view\":{\"counter\":{\"count\":9}}}}\n",
    )
    .unwrap();
    let failure = run_file(&file, &factory::<Counter>, &scenario_options(false)).unwrap_err();
    std::fs::remove_dir_all(&dir).unwrap();
    assert_eq!(2, failure.line);
    assert_eq!("expect", failure.step);
    assert_eq!("view `counter` $.count: expected 9, got 0", failure.message);
}
