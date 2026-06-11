//! Regression test for lossy float rendering (diagnosed 2026-06-11): without
//! serde_json's `float_roundtrip` feature, parsing a serialized f64 can land
//! 1 ULP off, so the host's parse→re-serialize of component-rendered state
//! JSON printed 30.599999999999998 as 30.6. The feature makes the round trip
//! bit-exact; `state_json` additionally serves the component's JSON verbatim.
#[test]
fn serde_json_float_roundtrip_is_exact() {
    let original = f64::from_bits(0x403e999999999999);
    let serialized = serde_json::to_string(&original).unwrap();
    assert_eq!(serialized, "30.599999999999998");
    let reparsed: f64 = serde_json::from_str(&serialized).unwrap();
    assert_eq!(
        reparsed.to_bits(),
        0x403e999999999999,
        "lossy parse: reprints as {}",
        serde_json::to_string(&reparsed).unwrap()
    );
}
