//! Port of packages/protocol/src/cbor/decoder.ts (pi v0.84.3).
//!
//! Decodes exactly one item from the protocol's strict RFC 8949 subset into
//! [`serde_json::Value`]. Byte strings become base64-free placeholder values:
//! the protocol schemas never admit byte strings inside JSON-valued fields,
//! so [`Value`] has no native slot for them. Wire-format byte strings decode
//! via [`ByteString`] and are rejected by schema validation upstream of this
//! decoder only when they appear where objects are required; callers that
//! need raw bytes interoperate with `serde_json::Value::Array(u8)` encoding
//! used by the TS side (`Uint8Array` == CBOR byte string) through
//! [`Value`] byte-string round trips in tests.

use serde_json::{Map, Number, Value};

use super::options::{CborError, CborOptions, CborResult, resolve_options};

pub(crate) const UINT32_BASE: u64 = 0x1_0000_0000;

struct CborReader<'a> {
    bytes: &'a [u8],
    offset: usize,
    options: super::options::ResolvedCborOptions,
}

impl<'a> CborReader<'a> {
    fn decode(&mut self) -> CborResult<Value> {
        let value = self.read_item(0)?;
        if self.offset != self.bytes.len() {
            return Err(CborError("CBOR payload contains trailing data".into()));
        }
        Ok(value)
    }

    fn read_item(&mut self, depth: usize) -> CborResult<Value> {
        if depth > self.options.max_depth {
            return Err(CborError(format!(
                "CBOR nesting depth exceeds configured limit of {}",
                self.options.max_depth
            )));
        }
        let initial = self.read_byte()?;
        let major_type = initial >> 5;
        let additional_information = initial & 0x1f;

        match major_type {
            0 => {
                let value = self.read_argument(additional_information)?;
                let value = i64::try_from(value).map_err(|_| {
                    CborError("Decoded CBOR integer is outside the safe range".into())
                })?;
                if value > 2i64.pow(53) - 1 {
                    return Err(CborError(
                        "Decoded CBOR integer is outside the safe range".into(),
                    ));
                }
                Ok(Value::Number(Number::from(value)))
            }
            1 => {
                let raw = self.read_argument(additional_information)?;
                // -1 - raw must stay >= -(2^53-1): raw = 2^53-2 is the last
                // safe value (yields MIN_SAFE_INTEGER); 2^53-1 yields -2^53,
                // which Number.isSafeInteger rejects.
                if raw > 2u64.pow(53) - 2 {
                    return Err(CborError(
                        "Decoded CBOR integer is outside the safe range".into(),
                    ));
                }
                let value = -1
                    - i64::try_from(raw).map_err(|_| {
                        CborError("Decoded CBOR integer is outside the safe range".into())
                    })?;
                Ok(Value::Number(Number::from(value)))
            }
            2 => {
                let length = self.read_length(
                    additional_information,
                    "byte string",
                    self.options.max_byte_length,
                )?;
                let bytes = self.read_bytes(length)?;
                Ok(Value::String(encode_byte_string_as_value(bytes)))
            }
            3 => {
                let length = self.read_length(
                    additional_information,
                    "text string",
                    self.options.max_byte_length,
                )?;
                let bytes = self.read_bytes(length)?;
                let text = std::str::from_utf8(bytes)
                    .map_err(|_| CborError("CBOR text string contains invalid UTF-8".into()))?;
                Ok(Value::String(text.to_owned()))
            }
            4 => {
                let length = self.read_length(
                    additional_information,
                    "array",
                    self.options.max_container_length,
                )?;
                let mut result = Vec::with_capacity(length.min(1024));
                for _ in 0..length {
                    result.push(self.read_item(depth + 1)?);
                }
                Ok(Value::Array(result))
            }
            5 => {
                let length = self.read_length(
                    additional_information,
                    "map",
                    self.options.max_container_length,
                )?;
                let mut result = Map::new();
                let mut keys = std::collections::HashSet::with_capacity(length.min(1024));
                for _ in 0..length {
                    let key = self.read_item(depth + 1)?;
                    let key = match key {
                        Value::String(s) => s,
                        _ => return Err(CborError("CBOR map keys must be strings".into())),
                    };
                    if !keys.insert(key.clone()) {
                        return Err(CborError("CBOR map contains a duplicate key".into()));
                    }
                    let value = self.read_item(depth + 1)?;
                    result.insert(key, value);
                }
                Ok(Value::Object(result))
            }
            6 => Err(CborError("CBOR tags are not supported".into())),
            7 => self.read_simple(additional_information),
            _ => Err(CborError("Malformed CBOR major type".into())),
        }
    }

    fn read_simple(&mut self, additional_information: u8) -> CborResult<Value> {
        match additional_information {
            20 => Ok(Value::Bool(false)),
            21 => Ok(Value::Bool(true)),
            22 => Ok(Value::Null),
            27 => {
                let bytes = self.read_bytes(8)?;
                let mut raw = [0u8; 8];
                raw.copy_from_slice(bytes);
                let value = f64::from_bits(u64::from_be_bytes(raw));
                if !value.is_finite() {
                    return Err(CborError("Decoded CBOR number must be finite".into()));
                }
                // Mirror Number.isInteger && !Number.isSafeInteger rejection:
                // integral floats outside the +-2^53 safe range are errors,
                // and -0 is a non-integer float that stays representable.
                if value == value.trunc() && value.abs() >= 9.007_199_254_740_992e15 {
                    return Err(CborError(
                        "Decoded CBOR integer is outside the safe range".into(),
                    ));
                }
                Number::from_f64(value)
                    .map(Value::Number)
                    .ok_or_else(|| CborError("Decoded CBOR number must be finite".into()))
            }
            31 => Err(CborError("CBOR break marker is not supported".into())),
            _ => Err(CborError(
                "Unsupported CBOR simple value or floating-point width".into(),
            )),
        }
    }

    fn read_length(
        &mut self,
        additional_information: u8,
        kind: &str,
        limit: usize,
    ) -> CborResult<usize> {
        if additional_information == 31 {
            return Err(CborError(format!(
                "Indefinite-length CBOR {kind}s are not supported"
            )));
        }
        let length = self.read_argument(additional_information)?;
        let length = usize::try_from(length).map_err(|_| {
            CborError(format!(
                "CBOR {kind} length exceeds configured limit of {limit}"
            ))
        })?;
        if length > limit {
            return Err(CborError(format!(
                "CBOR {kind} length exceeds configured limit of {limit}"
            )));
        }
        Ok(length)
    }

    fn read_argument(&mut self, additional_information: u8) -> CborResult<u64> {
        if additional_information < 24 {
            return Ok(u64::from(additional_information));
        }
        match additional_information {
            24 => Ok(u64::from(self.read_byte()?)),
            25 => {
                let bytes = self.read_bytes(2)?;
                Ok(u64::from(bytes[0]) * 0x100 + u64::from(bytes[1]))
            }
            26 => {
                let bytes = self.read_bytes(4)?;
                Ok(u64::from(bytes[0]) * 0x1_000_000
                    + u64::from(bytes[1]) * 0x1_0000
                    + u64::from(bytes[2]) * 0x100
                    + u64::from(bytes[3]))
            }
            27 => {
                let high = self.read_argument(26)?;
                let low = self.read_argument(26)?;
                if high > 0x1f_ffff {
                    return Err(CborError(
                        "Decoded CBOR integer or length is outside the safe range".into(),
                    ));
                }
                Ok(high * UINT32_BASE + low)
            }
            31 => Err(CborError(
                "Indefinite-length CBOR items are not supported".into(),
            )),
            _ => Err(CborError("Malformed CBOR additional information".into())),
        }
    }

    fn read_byte(&mut self) -> CborResult<u8> {
        if self.offset >= self.bytes.len() {
            return Err(CborError("Truncated CBOR payload".into()));
        }
        let value = self.bytes[self.offset];
        self.offset += 1;
        Ok(value)
    }

    fn read_bytes(&mut self, length: usize) -> CborResult<&'a [u8]> {
        if length > self.bytes.len() - self.offset {
            return Err(CborError("Truncated CBOR payload".into()));
        }
        let value = &self.bytes[self.offset..self.offset + length];
        self.offset += length;
        Ok(value)
    }
}

/// Lossless in-`Value` carrier for CBOR byte strings: a 4-char prefix plus
/// base64url-free raw bytes rendered as latin-1 code points. Only used to
/// round-trip `Uint8Array` fixtures; schema validation rejects it wherever
/// JSON values are expected.
pub(crate) fn encode_byte_string_as_value(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() + 1);
    out.push('\u{0}');
    for &b in bytes {
        out.push(b as char);
    }
    out
}

/// Decodes exactly one item from the protocol's strict RFC 8949 subset.
pub fn decode_cbor(bytes: &[u8], options: CborOptions) -> CborResult<Value> {
    let resolved = resolve_options(options)?;
    if bytes.len() > resolved.max_byte_length {
        return Err(CborError(format!(
            "CBOR byte length exceeds configured limit of {}",
            resolved.max_byte_length
        )));
    }
    CborReader {
        bytes,
        offset: 0,
        options: resolved,
    }
    .decode()
}
