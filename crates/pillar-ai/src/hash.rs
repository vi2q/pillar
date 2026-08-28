//! Port of packages/ai/src/utils/hash.ts (pi v0.84.3).
//!
//! Fast deterministic hash to shorten long strings. `Math.imul` maps to
//! wrapping `u32` multiplication, and the >>> shifts to logical shifts.

/// Port of upstream `shortHash` — output is base36 of two u32 halves and
/// matches the JS implementation bit-for-bit.
pub fn short_hash(input: &str) -> String {
    let mut h1: u32 = 0xdead_beef;
    let mut h2: u32 = 0x41c6_ce57;
    for ch in input.encode_utf16() {
        h1 = (h1 ^ u32::from(ch)).wrapping_mul(2_654_435_761);
        h2 = (h2 ^ u32::from(ch)).wrapping_mul(1_597_334_677);
    }
    h1 = ((h1 ^ (h1 >> 16)).wrapping_mul(2_246_822_507))
        ^ ((h2 ^ (h2 >> 13)).wrapping_mul(3_266_489_909));
    h2 = ((h2 ^ (h2 >> 16)).wrapping_mul(2_246_822_507))
        ^ ((h1 ^ (h1 >> 13)).wrapping_mul(3_266_489_909));
    base36(h2) + &base36(h1)
}

fn base36(value: u32) -> String {
    // Mirrors (value >>> 0).toString(36).
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_owned();
    }
    let mut out = Vec::new();
    let mut v = value;
    while v > 0 {
        out.push(digits[(v % 36) as usize]);
        v /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("base36 digits are ASCII")
}
