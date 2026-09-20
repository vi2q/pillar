//! Port of packages/protocol/src/cbor/encoder.ts (pi v0.84.3).
//!
//! Encodes the protocol's strict, definite-length RFC 8949 subset over
//! [`serde_json::Value`] inputs: null, bool, safe integers, finite f64,
//! UTF-8 strings, byte strings, arrays, and string-keyed maps. `Value::Null`
//! doubles as JSON `null`; omitted-`undefined` semantics live at the schema
//! layer (fields set to `Value::Null` with `skip_serializing_if` stay absent).

use serde_json::Value;

use super::options::{CborError, CborOptions, CborResult, ResolvedCborOptions, resolve_options};

const UINT32_BASE: u64 = 0x1_0000_0000;

struct CborWriter {
    buffer: Vec<u8>,
    max_byte_length: usize,
}

impl CborWriter {
    fn new(max_byte_length: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(256.min(max_byte_length)),
            max_byte_length,
        }
    }

    fn write_byte(&mut self, value: u8) -> CborResult<()> {
        self.ensure_capacity(1)?;
        self.buffer.push(value);
        Ok(())
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> CborResult<()> {
        self.ensure_capacity(bytes.len())?;
        self.buffer.extend_from_slice(bytes);
        Ok(())
    }

    fn write_uint16(&mut self, value: u16) -> CborResult<()> {
        self.ensure_capacity(2)?;
        self.buffer.extend_from_slice(&value.to_be_bytes());
        Ok(())
    }

    fn write_uint32(&mut self, value: u32) -> CborResult<()> {
        self.ensure_capacity(4)?;
        self.buffer.extend_from_slice(&value.to_be_bytes());
        Ok(())
    }

    fn write_uint64(&mut self, value: u64) -> CborResult<()> {
        self.write_uint32((value / UINT32_BASE) as u32)?;
        self.write_uint32((value % UINT32_BASE) as u32)
    }

    fn write_float64(&mut self, value: f64) -> CborResult<()> {
        self.write_byte(0xfb)?;
        self.buffer
            .extend_from_slice(&value.to_bits().to_be_bytes());
        Ok(())
    }

    fn finish(self) -> Vec<u8> {
        self.buffer
    }

    fn ensure_capacity(&self, additional_bytes: usize) -> CborResult<()> {
        let required = self.buffer.len() + additional_bytes;
        if required > self.max_byte_length {
            return Err(CborError(format!(
                "CBOR byte length exceeds configured limit of {}",
                self.max_byte_length
            )));
        }
        Ok(())
    }
}

fn write_argument(writer: &mut CborWriter, major_type: u8, value: u64) -> CborResult<()> {
    let prefix = major_type << 5;
    // The byte-length limits guarantee `value` fits; the cast is safe because
    // ensure_capacity rejects anything beyond max_byte_length <= MAX_UINT32.
    if value < 24 {
        writer.write_byte(prefix | value as u8)
    } else if value <= 0xff {
        writer.write_byte(prefix | 24)?;
        writer.write_byte(value as u8)
    } else if value <= 0xffff {
        writer.write_byte(prefix | 25)?;
        writer.write_uint16(value as u16)
    } else if value <= u32::MAX as u64 {
        writer.write_byte(prefix | 26)?;
        writer.write_uint32(value as u32)
    } else {
        writer.write_byte(prefix | 27)?;
        writer.write_uint64(value)
    }
}

fn encode_text(
    writer: &mut CborWriter,
    value: &str,
    options: &ResolvedCborOptions,
) -> CborResult<()> {
    let bytes = value.as_bytes();
    if bytes.len() > options.max_byte_length {
        return Err(CborError(format!(
            "CBOR text string length exceeds configured limit of {}",
            options.max_byte_length
        )));
    }
    // Rust strings are always valid UTF-8, so the lossy-round-trip check the
    // TypeScript encoder performs (rejecting lone surrogates) cannot trigger.
    write_argument(writer, 3, bytes.len() as u64)?;
    writer.write_bytes(bytes)
}

fn encode_value(
    writer: &mut CborWriter,
    value: &Value,
    options: &ResolvedCborOptions,
    depth: usize,
    ancestors: &mut Vec<*const Value>,
) -> CborResult<()> {
    if depth > options.max_depth {
        return Err(CborError(format!(
            "CBOR nesting depth exceeds configured limit of {}",
            options.max_depth
        )));
    }

    match value {
        Value::Null => writer.write_byte(0xf6),
        Value::Bool(b) => writer.write_byte(if *b { 0xf5 } else { 0xf4 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64()
                && n.as_f64() == Some(i as f64)
            {
                // Safe-integer check mirrors Number.isSafeInteger: the
                // safe range is -(2^53-1)..(2^53-1) inclusive.
                if !(-(2i64.pow(53) - 1)..=(2i64.pow(53) - 1)).contains(&i) {
                    return Err(CborError(
                        "CBOR integers must be safe JavaScript integers".into(),
                    ));
                }
                return if i >= 0 {
                    write_argument(writer, 0, i as u64)
                } else {
                    write_argument(writer, 1, (-1 - i) as u64)
                };
            }
            let f = n
                .as_f64()
                .ok_or_else(|| CborError("CBOR numbers must be finite".into()))?;
            if !f.is_finite() {
                return Err(CborError("CBOR numbers must be finite".into()));
            }
            if f == f.trunc() && f.abs() < 9.007_199_254_740_992e15 {
                // JSON cannot represent -0, so the Object.is(value, -0) branch
                // from upstream has no serde_json equivalent; integral floats
                // keep float64 encoding only when they exceed the safe range.
                if f <= -(1i64 << 53) as f64 || f >= (1i64 << 53) as f64 {
                    return writer.write_float64(f);
                }
            }
            writer.write_float64(f)
        }
        Value::String(s) => encode_text(writer, s, options),
        Value::Array(items) => {
            let ptr = value as *const Value;
            if ancestors.contains(&ptr) {
                return Err(CborError("CBOR values must not contain cycles".into()));
            }
            if items.len() > options.max_container_length {
                return Err(CborError(format!(
                    "CBOR array length exceeds configured limit of {}",
                    options.max_container_length
                )));
            }
            ancestors.push(ptr);
            let result = (|| {
                write_argument(writer, 4, items.len() as u64)?;
                for item in items {
                    encode_value(writer, item, options, depth + 1, ancestors)?;
                }
                Ok(())
            })();
            ancestors.pop();
            result
        }
        Value::Object(map) => {
            let ptr = value as *const Value;
            if ancestors.contains(&ptr) {
                return Err(CborError("CBOR values must not contain cycles".into()));
            }
            if map.len() > options.max_container_length {
                return Err(CborError(format!(
                    "CBOR map length exceeds configured limit of {}",
                    options.max_container_length
                )));
            }
            ancestors.push(ptr);
            let result = (|| {
                write_argument(writer, 5, map.len() as u64)?;
                for (key, entry_value) in map {
                    encode_text(writer, key, options)?;
                    encode_value(writer, entry_value, options, depth + 1, ancestors)?;
                }
                Ok(())
            })();
            ancestors.pop();
            result
        }
    }
}

/// Encodes the protocol's strict, definite-length RFC 8949 subset.
pub fn encode_cbor(value: &Value, options: CborOptions) -> CborResult<Vec<u8>> {
    let resolved = resolve_options(options)?;
    let mut writer = CborWriter::new(resolved.max_byte_length);
    encode_value(&mut writer, value, &resolved, 0, &mut Vec::new())?;
    Ok(writer.finish())
}
