//! Native configuration owner to real browser parser compatibility acceptance.

use std::path::Path;

use bcode_tui_web::BrowserPolicy;
use serde_json::{Value, json};

fn roundtrip(label: &str, config: &bcode_config::TuiConfig) -> Value {
    let encoded = bcode_tui::browser_presentation::export(config, Path::new("."))
        .expect("native resolved policy must export without truncation");
    let mut native: Value = serde_json::from_str(&encoded).expect("native policy JSON");
    let parsed = BrowserPolicy::parse(&encoded).expect("real browser parser accepts native policy");
    let bindings = serde_json::to_value(&parsed.bindings).expect("parsed bindings serialize");
    let actual = bindings.as_array().expect("parsed binding array");
    let expected = native["bindings"]
        .as_array_mut()
        .expect("native binding array");
    assert_eq!(actual.len(), expected.len(), "binding count must survive");
    for (index, (expected, actual)) in expected.iter_mut().zip(actual).enumerate() {
        // BrowserBinding documents serde(default) for this Option: omitted and
        // explicit null both mean no multi-tap policy. Normalize nothing else.
        if expected.get("multi_tap").is_none_or(Value::is_null) {
            assert!(
                parsed.bindings[index].multi_tap.is_none(),
                "binding {index}: missing/null multi_tap must parse as None"
            );
            expected
                .as_object_mut()
                .expect("binding object")
                .insert("multi_tap".into(), Value::Null);
        }
        assert!(
            &*expected == actual,
            "binding {index}: roundtrip mismatch at scope={} chord={}",
            expected["scope"],
            expected["chord"]
        );
    }
    assert_eq!(parsed.schema_version, 1);
    assert_eq!(parsed.revision, native["revision"]);
    eprintln!(
        "NATIVE_BROWSER_ROUNDTRIP {label} bindings={} bytes={}",
        parsed.bindings.len(),
        encoded.len()
    );
    native
}

#[test]
fn native_browser_policy_optional_multi_tap_missing_and_null_are_none() {
    let encoded = bcode_tui::browser_presentation::export(
        &bcode_config::TuiConfig::default(),
        Path::new("."),
    )
    .expect("native policy");
    let mut native: Value = serde_json::from_str(&encoded).expect("native JSON");
    let first = native["bindings"][0]
        .as_object_mut()
        .expect("first binding");
    first.remove("multi_tap");
    let missing = BrowserPolicy::parse(&native.to_string()).expect("omitted optional multi_tap");
    assert!(missing.bindings[0].multi_tap.is_none());
    native["bindings"][0]["multi_tap"] = Value::Null;
    let null = BrowserPolicy::parse(&native.to_string()).expect("null optional multi_tap");
    assert!(null.bindings[0].multi_tap.is_none());
    assert_eq!(missing.bindings.len(), null.bindings.len());
    for (index, (missing, null)) in missing.bindings.iter().zip(&null.bindings).enumerate() {
        assert!(
            serde_json::to_value(missing).expect("missing binding serializes")
                == serde_json::to_value(null).expect("null binding serializes"),
            "binding {index}: optional representation changed semantics"
        );
    }
}

#[test]
fn native_browser_policy_roundtrip_defaults_and_configured_bindings() {
    let defaults = bcode_config::TuiConfig::default();
    let native = roundtrip("defaults", &defaults);
    let mut configured = defaults;
    configured
        .keybindings
        .chat
        .insert("ctrl+g".into(), "tui.editor.moveCursorLeft".into());
    configured
        .keybindings
        .chat
        .insert("enter".into(), String::new());
    let configured = roundtrip("override-and-unbind", &configured);
    let bindings = configured["bindings"].as_array().expect("binding array");
    let enter = json!([{"key":"Enter","ctrl":false,"alt":false,"shift":false,"meta":false}]);
    let control_g = json!([{"key":"g","ctrl":true,"alt":false,"shift":false,"meta":false}]);
    assert!(bindings.iter().any(|binding| binding["scope"] == "chat"
        && binding["chord"] == enter
        && binding["binding"] == json!({"type":"ignored"})));
    assert!(bindings.iter().any(|binding| binding["scope"] == "chat"
        && binding["chord"] == control_g
        && binding["binding"] == json!({"type":"editor","command":{"Move":"Left"}})));
    // Native overrides replace an action's keys, so ctrl+g legitimately replaces
    // chat ArrowLeft. Check a genuinely unrelated inherited action instead.
    let right = json!([{"key":"ArrowRight","ctrl":false,"alt":false,"shift":false,"meta":false}]);
    let inherited = native["bindings"]
        .as_array()
        .expect("default bindings")
        .iter()
        .find(|binding| binding["scope"] == "chat" && binding["chord"] == right)
        .expect("native defaults include chat ArrowRight");
    assert_eq!(
        inherited["binding"],
        json!({"type":"editor","command":{"Move":"Right"}})
    );
    assert!(
        bindings.contains(inherited),
        "unrelated inherited chat ArrowRight must survive override/unbind"
    );
}
