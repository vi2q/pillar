//! Port of packages/ai/src/utils/uuid.ts (pi v0.84.3).
//!
//! Time-ordered UUIDv7 with monotonic sequence: same-millisecond calls
//! increment a 32-bit sequence seeded from random bytes; sequence wrap
//! bumps the timestamp. The Rust `uuid` crate does not guarantee
//! monotonicity across same-ms calls, so the sequence logic is ported.

use std::sync::Mutex;

struct UuidState {
    last_timestamp: i64,
    sequence: u64,
}

static STATE: Mutex<UuidState> = Mutex::new(UuidState {
    last_timestamp: i64::MIN,
    sequence: 0,
});

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn fill_random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    // /dev/urandom is the portable-ish entropy source; fall back to a
    // time-seeded weak source when unavailable (non-Unix).
    if read_urandom(&mut bytes).is_err() {
        let seed = now_millis() as u64 ^ (&bytes as *const _ as u64);
        let mut x = seed | 1;
        for byte in bytes.iter_mut() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *byte = (x & 0xff) as u8;
        }
    }
    bytes
}

fn read_urandom(bytes: &mut [u8]) -> Result<(), ()> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom").map_err(|_| ())?;
    file.read_exact(bytes).map_err(|_| ())
}

/// Generate a time-ordered UUIDv7 with the RFC 9562 layout and pi's
/// monotonic same-millisecond sequencing.
pub fn uuidv7() -> String {
    let random = fill_random::<16>();
    let timestamp = now_millis();

    let (ts, sequence) = {
        let mut state = STATE.lock().expect("uuid state lock");
        let sequence: u64 = if timestamp > state.last_timestamp {
            state.last_timestamp = timestamp;
            u32::from_be_bytes([0, random[6], random[7], random[8]]) as u64 * 0x100
                + random[9] as u64
        } else {
            let next = state.sequence.wrapping_add(1);
            if next == 0 {
                state.last_timestamp += 1;
            }
            next
        };
        state.sequence = sequence;
        (state.last_timestamp, sequence)
    };

    let bytes = build_bytes(ts, sequence, &random);
    format_uuid(&bytes)
}

fn build_bytes(timestamp: i64, sequence: u64, random: &[u8; 16]) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    let ts = timestamp as u64;
    bytes[0] = ((ts / 0x0100_0000_0000) & 0xff) as u8;
    bytes[1] = ((ts / 0x1_0000_0000) & 0xff) as u8;
    bytes[2] = ((ts / 0x100_0000) & 0xff) as u8;
    bytes[3] = ((ts / 0x1_0000) & 0xff) as u8;
    bytes[4] = ((ts / 0x100) & 0xff) as u8;
    bytes[5] = (ts & 0xff) as u8;
    bytes[6] = 0x70 | ((sequence >> 28) & 0x0f) as u8;
    bytes[7] = ((sequence >> 20) & 0xff) as u8;
    bytes[8] = 0x80 | ((sequence >> 14) & 0x3f) as u8;
    bytes[9] = ((sequence >> 6) & 0xff) as u8;
    bytes[10] = (((sequence & 0x3f) << 2) as u8) | (random[10] & 0x03);
    bytes[11] = random[11];
    bytes[12] = random[12];
    bytes[13] = random[13];
    bytes[14] = random[14];
    bytes[15] = random[15];
    bytes
}

fn format_uuid(bytes: &[u8; 16]) -> String {
    let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        hex[0..4].join(""),
        hex[4..6].join(""),
        hex[6..8].join(""),
        hex[8..10].join(""),
        hex[10..16].join("")
    )
}
