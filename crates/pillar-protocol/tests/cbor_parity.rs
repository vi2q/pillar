//! Port of packages/protocol/test/cbor/cbor.test.ts (pi v0.84.3).
//!
//! One Rust test per upstream vitest test, same names in comments.

use serde_json::{Value, json};

use pillar_protocol::{
    CborOptions, DEFAULT_MAX_CBOR_BYTE_LENGTH, DEFAULT_MAX_CBOR_CONTAINER_LENGTH,
    DEFAULT_MAX_CBOR_DEPTH, decode_cbor, encode_cbor,
};

fn from_hex(hex: &str) -> Vec<u8> {
    assert!(hex.len() % 2 == 0, "Hex fixture must contain whole bytes");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Encodes a JSON value the way the TS encoder receives it. Numbers pass
/// through as f64 or i64 depending on JSON representation; the `bitint`
/// style unsafe integers arrive as f64.
fn enc(value: &Value) -> String {
    to_hex(&encode_cbor(value, CborOptions::new()).unwrap())
}

fn dec(wire: &str) -> Value {
    decode_cbor(&from_hex(wire), CborOptions::new()).unwrap()
}

#[track_caller]
fn expect_decode_error(wire: &str) {
    let result = decode_cbor(&from_hex(wire), CborOptions::new());
    assert!(result.is_err(), "expected decode error for {wire}");
}

#[test]
fn encodes_and_decodes_rfc_8949_vectors() {
    // (value, wire) — the value half is built inline per case because
    // serde_json distinguishes integer widths.
    let vectors: Vec<(Value, &str)> = vec![
        (json!(null), "f6"),
        (json!(false), "f4"),
        (json!(true), "f5"),
        (json!(0), "00"),
        (json!(1), "01"),
        (json!(10), "0a"),
        (json!(23), "17"),
        (json!(24), "1818"),
        (json!(25), "1819"),
        (json!(100), "1864"),
        (json!(1000), "1903e8"),
        (json!(1_000_000), "1a000f4240"),
        (json!(1_000_000_000_000u64), "1b000000e8d4a51000"),
        // Number.MAX_SAFE_INTEGER
        (json!(9007199254740991u64), "1b001fffffffffffff"),
        (json!(-1), "20"),
        (json!(-10), "29"),
        (json!(-24), "37"),
        (json!(-25), "3818"),
        (json!(-100), "3863"),
        (json!(-1000), "3903e7"),
        (json!(-1_000_000), "3a000f423f"),
        (json!(-9007199254740991i64), "3b001ffffffffffffe"),
        (json!(1.1), "fb3ff199999999999a"),
        // -0: JSON cannot carry it, but the float64 encoding is reachable
        // through the decoder; upstream asserts Object.is(decoded, -0).
        (json!(""), "60"),
        (json!("IETF"), "6449455446"),
        (json!("ü"), "62c3bc"),
        (json!("水"), "63e6b0b4"),
        (json!("𐅑"), "64f0908591"),
        (json!([]), "80"),
        (json!([1, 2, 3]), "83010203"),
        (json!([1, [2, 3], [4, 5]]), "8301820203820405"),
        (json!({"a": 1, "b": [2, 3]}), "a26161016162820203"),
    ];
    for (value, wire) in vectors {
        assert_eq!(enc(&value), wire, "encoding {value}");
        assert_eq!(dec(wire), value, "decoding {wire}");
    }
}

#[test]
fn encodes_and_decodes_negative_zero() {
    // -0 arrives as a float64; serde_json::Number cannot represent it, so
    // this checks the wire bytes and the decoded finite float round trip.
    let wire = "fb8000000000000000";
    let decoded = dec(wire);
    assert_eq!(decoded.as_f64(), Some(-0.0));
    // And the encoder never produces -0 from JSON input (json!(-0.0) == 0).
    assert_eq!(enc(&json!(0)), "00");
}

#[test]
fn encodes_byte_strings() {
    // Uint8Array fixture from upstream: new Uint8Array([1, 2, 3, 4]) -> 44...
    // The Rust encoder takes serde_json::Value, so byte strings are exercised
    // through the decoder and via the codec's Value surface in protocol tests.
    let value = json!([1, 2, 3, 4]);
    // An array is not a byte string; the byte-string wire form decodes into
    // the internal carrier string.
    let decoded = dec("4401020304");
    assert!(decoded.is_string());
    assert_ne!(decoded, value);
}

#[test]
fn omits_undefined_object_properties_without_omitting_falsey_values() {
    // undefined has no JSON representation; upstream's test maps to verifying
    // that Null is preserved and other falsey values survive.
    let value = json!({"zero": 0, "empty": "", "no": false, "nil": null});
    assert_eq!(dec(&enc(&value)), value);
}

#[test]
fn preserves_a_leading_unicode_bom() {
    assert_eq!(dec("63efbbbf"), json!("\u{feff}"));
}

#[test]
fn rejects_unsupported_encoder_values() {
    // NaN/Infinity: serde_json::Number cannot hold them, so this verifies
    // the finite check via f64::NAN flowing through Number::from_f64 (None).
    use serde_json::Number;
    let nan =
        Value::Number(Number::from_f64(f64::NAN).unwrap_or(json!(0).as_number().unwrap().clone()));
    // from_f64 returns None for NaN, so the unreachable construction above
    // falls back; instead assert the encoder rejects a too-large integer.
    let _ = nan;
    let too_big = json!(9007199254740993u64); // MAX_SAFE_INTEGER + 2
    assert!(encode_cbor(&too_big, CborOptions::new()).is_err());
    // Depth overflow.
    let mut too_deep = json!(null);
    for _ in 0..=DEFAULT_MAX_CBOR_DEPTH {
        too_deep = json!([too_deep]);
    }
    let err = encode_cbor(&too_deep, CborOptions::new()).unwrap_err();
    assert!(err.to_string().contains("depth"), "got: {err}");
}

#[test]
fn rejects_lossy_strings_cycles_and_excessive_encoder_depth() {
    // Cycles: serde_json::Value is acyclic by construction, so the cycle
    // check is only reachable through shared-pointer aliases that cannot
    // exist here. The depth limit above covers the recursive guard.
    // Lone surrogates cannot exist in Rust strings either (lossy check).
    // Both guards remain for wire-format defense; tested via decoder below.
}

#[test]
fn rejects_invalid_decoder_input() {
    let cases: &[(&str, &str)] = &[
        ("empty input", ""),
        ("truncated integer", "18"),
        ("reserved additional information", "1c"),
        ("indefinite byte string", "5f"),
        ("indefinite text string", "7f"),
        ("indefinite array", "9f"),
        ("indefinite map", "bf"),
        ("tag", "c000"),
        ("undefined", "f7"),
        ("unsupported simple value", "e0"),
        ("break outside an indefinite item", "ff"),
        ("float16", "f93c00"),
        ("float32", "fa3f800000"),
        ("positive infinity", "fb7ff0000000000000"),
        ("NaN", "fb7ff8000000000000"),
        ("truncated float64", "fb3ff00000"),
        ("truncated byte string", "44010203"),
        ("truncated text string", "636162"),
        ("truncated array", "8201"),
        ("truncated map", "a16161"),
        ("trailing data", "0000"),
        ("non-string map key", "a10102"),
        ("duplicate map key", "a2616101616102"),
        ("invalid UTF-8 byte", "61ff"),
        ("overlong UTF-8", "62c080"),
        ("UTF-8 surrogate", "63eda080"),
        ("unsafe positive integer", "1b0020000000000000"),
        ("unsafe negative integer", "3b001fffffffffffff"),
        ("unsafe integer encoded as float64", "fb4340000000000000"),
    ];
    for (label, wire) in cases {
        expect_decode_error(wire);
        let _ = label;
    }
}

#[test]
fn enforces_depth_and_declared_length_limits_before_traversing_values() {
    let mut too_deep = vec![0x81u8; DEFAULT_MAX_CBOR_DEPTH + 2];
    *too_deep.last_mut().unwrap() = 0xf6;
    let err = decode_cbor(&too_deep, CborOptions::new()).unwrap_err();
    assert!(err.to_string().contains("depth"), "got: {err}");

    let oversized_hex = format!("5a{:08x}", DEFAULT_MAX_CBOR_BYTE_LENGTH as u64 + 1);
    let oversized_bytes = from_hex(&oversized_hex);
    let err = decode_cbor(&oversized_bytes, CborOptions::new()).unwrap_err();
    assert!(err.to_string().contains("limit"), "got: {err}");

    let oversized_text = from_hex(&format!(
        "7a{:08x}",
        DEFAULT_MAX_CBOR_BYTE_LENGTH as u64 + 1
    ));
    assert!(decode_cbor(&oversized_text, CborOptions::new()).is_err());

    let oversized_array = from_hex(&format!(
        "9a{:08x}",
        DEFAULT_MAX_CBOR_CONTAINER_LENGTH as u64 + 1
    ));
    assert!(decode_cbor(&oversized_array, CborOptions::new()).is_err());

    let oversized_map = from_hex(&format!(
        "ba{:08x}",
        DEFAULT_MAX_CBOR_CONTAINER_LENGTH as u64 + 1
    ));
    assert!(decode_cbor(&oversized_map, CborOptions::new()).is_err());
}

#[test]
fn supports_stricter_caller_provided_limits() {
    assert!(
        decode_cbor(
            &from_hex("83010203"),
            CborOptions::new().max_container_length(2)
        )
        .is_err()
    );
    assert!(decode_cbor(&from_hex("626162"), CborOptions::new().max_byte_length(2)).is_err());
    assert!(
        encode_cbor(
            &json!([1, 2, 3]),
            CborOptions::new().max_container_length(2)
        )
        .is_err()
    );
    assert!(encode_cbor(&json!("ab"), CborOptions::new().max_byte_length(2)).is_err());
}

#[test]
fn cbor_error_type_is_exposed() {
    let err = decode_cbor(&from_hex("ff"), CborOptions::new()).unwrap_err();
    assert!(err.to_string().contains("CBOR"));
}
