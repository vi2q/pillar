//! Regression: an SSE body whose chunk boundary splits a multi-byte character
//! must not corrupt the streamed text. The providers used to lossy-decode each
//! network chunk on its own (`String::from_utf8_lossy(&chunk)`), turning a torn
//! CJK character into U+FFFD; they now feed bytes through
//! `Utf8ChunkDecoder`. This drives the shared decoder the providers use.

#![cfg(feature = "providers")]

use pillar_ai::api::anthropic_messages::{SseDecoderState, decode_sse_chunk, finish_sse_body};

/// The shape an Anthropic-style stream carries: one content_block_delta whose
/// text mixes CJK and inline-code ASCII.
fn body() -> String {
    let text = "\u{5909}\u{66f4}\u{304c}\u{7121}\u{3044} chat `Container` \u{304c}\u{8fd4}\u{3059}";
    let json = serde_json::json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": { "type": "text_delta", "text": text },
    });
    format!("event: content_block_delta\ndata: {json}\n\n")
}

/// Mirror `decode_fetch_response` with the boundary-safe decoder in place of
/// the per-chunk lossy decode.
fn decoded_text(body: &str, split_at: usize) -> String {
    let bytes = body.as_bytes();
    let mut state = SseDecoderState::default();
    let mut buffer = String::new();
    let mut decoder = pillar_ai::api::Utf8ChunkDecoder::new();
    let mut events = decode_sse_chunk(&decoder.push(&bytes[..split_at]), &mut state, &mut buffer);
    events.extend(decode_sse_chunk(
        &decoder.push(&bytes[split_at..]),
        &mut state,
        &mut buffer,
    ));
    events.extend(decode_sse_chunk(&decoder.finish(), &mut state, &mut buffer));
    events.extend(finish_sse_body(&mut state, &mut buffer));
    events
        .iter()
        .map(|e| e.data.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A split that cuts a multi-byte character must still yield the intact text.
#[test]
fn splitting_a_multibyte_char_keeps_the_stream_intact() {
    let body = body();
    let bytes = body.as_bytes();
    for (split, byte) in bytes.iter().enumerate().skip(1) {
        if byte & 0b1100_0000 != 0b1000_0000 {
            continue; // only splits inside a character
        }
        let out = decoded_text(&body, split);
        assert!(
            !out.contains('\u{fffd}'),
            "split={split}: text corrupted: {out:?}"
        );
        assert!(
            out.contains("Container"),
            "split={split}: inline code lost: {out:?}"
        );
    }
}
