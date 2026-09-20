//! Temporary reproduction: what happens when a network chunk splits a
//! multi-byte UTF-8 character in an SSE body (the `from_utf8_lossy` per
//! chunk path used by the streaming providers).

use pillar_ai::api::anthropic_messages::{SseDecoderState, decode_sse_chunk, finish_sse_body};

/// The body an Anthropic-style stream would carry: one content_block_delta
/// whose text mixes CJK and inline-code ASCII.
fn body() -> String {
    let text = "\u{5909}\u{66f4}\u{304c}\u{7121}\u{3044} chat `Container` \u{304c}\u{8fd4}\u{3059}";
    let json = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": { "type": "text_delta", "text": text },
    });
    format!("event: content_block_delta\ndata: {json}\n\n")
}

fn decoded_text(body: &str, split_at: usize) -> String {
    // Mirror decode_fetch_response: each chunk is lossy-decoded on its own.
    let bytes = body.as_bytes();
    let mut state = SseDecoderState::default();
    let mut buffer = String::new();
    let mut events = Vec::new();
    for chunk in [&bytes[..split_at], &bytes[split_at..]] {
        let text = String::from_utf8_lossy(chunk);
        events.extend(decode_sse_chunk(&text, &mut state, &mut buffer));
    }
    events.extend(finish_sse_body(&mut state, &mut buffer));
    events
        .iter()
        .map(|e| e.data.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn splitting_a_multibyte_char_corrupts_the_stream() {
    let body = body();
    let bytes = body.as_bytes();
    let mut corrupted = 0;
    let mut dropped_code = 0;
    for split in 1..bytes.len() {
        // Only care about splits that cut a multi-byte char.
        if bytes[split].is_ascii_continuation() && !bytes[split - 1].is_ascii() {
            let out = decoded_text(&body, split);
            if out.contains('\u{fffd}') {
                corrupted += 1;
            }
            if !out.contains("Container") {
                dropped_code += 1;
                println!("split={split}: inline code dropped: {out:?}");
            }
        }
    }
    println!("split points cutting a char: corrupted={corrupted}, dropped_code={dropped_code}");
    assert_eq!(corrupted, 0, "lossy per-chunk decode must not corrupt");
}

trait Continuation {
    fn is_ascii_continuation(&self) -> bool;
}
impl Continuation for u8 {
    fn is_ascii_continuation(&self) -> bool {
        *self & 0b1100_0000 == 0b1000_0000
    }
}
