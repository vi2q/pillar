//! Port of packages/protocol/test/framing.test.ts (pi v0.84.3).
//!
//! One Rust test per upstream vitest test, same names in comments.

use pillar_protocol::{
    assert_complete_frame, encode_frame, FrameDecoder, FrameDecoderOptions, FrameError,
    DEFAULT_MAX_FRAME_LENGTH,
};

fn concatenate(chunks: &[&[u8]]) -> Vec<u8> {
    let total: usize = chunks.iter().map(|c| c.len()).sum();
    let mut result = Vec::with_capacity(total);
    for chunk in chunks {
        result.extend_from_slice(chunk);
    }
    result
}

#[test]
fn prefixes_payloads_with_a_four_byte_big_endian_length() {
    assert_eq!(
        encode_frame(&[0xaa, 0xbb, 0xcc]).unwrap(),
        vec![0x00, 0x00, 0x00, 0x03, 0xaa, 0xbb, 0xcc]
    );
    assert_eq!(encode_frame(&[]).unwrap(), vec![0, 0, 0, 0]);
}

#[test]
fn validates_one_complete_bounded_frame_without_accepting_trailing_or_partial_bytes() {
    assert!(assert_complete_frame(
        &[0, 0, 0, 2, 1, 2],
        FrameDecoderOptions::new().max_frame_length(2)
    )
    .is_ok());
    assert!(assert_complete_frame(&[0, 0, 0, 2, 1], FrameDecoderOptions::new()).is_err());
    assert!(assert_complete_frame(&[0, 0, 0, 1, 1, 2], FrameDecoderOptions::new()).is_err());
    assert!(assert_complete_frame(
        &[0, 0, 0, 3, 1, 2, 3],
        FrameDecoderOptions::new().max_frame_length(2)
    )
    .is_err());
}

#[test]
fn decodes_fragmented_coalesced_and_empty_frames_in_order() {
    let wire = concatenate(&[
        &encode_frame(&[1, 2, 3]).unwrap(),
        &encode_frame(&[]).unwrap(),
        &encode_frame(&[4]).unwrap(),
    ]);
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::new()).unwrap();
    let mut frames = Vec::new();
    for byte in &wire {
        frames.extend(decoder.push(&[*byte]).unwrap());
    }
    decoder.end().unwrap();
    assert_eq!(frames, vec![vec![1, 2, 3], vec![], vec![4]]);

    let mut coalesced = FrameDecoder::new(FrameDecoderOptions::new()).unwrap();
    assert_eq!(coalesced.push(&wire).unwrap(), frames);
    coalesced.end().unwrap();
}

#[test]
fn assembles_payloads_spanning_multiple_internal_blocks() {
    let payload: Vec<u8> = (0..70_000).map(|i| (i % 251) as u8).collect();
    let wire = encode_frame(&payload).unwrap();
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::new()).unwrap();
    let mut frames = Vec::new();
    frames.extend(decoder.push(&wire[0..101]).unwrap());
    frames.extend(decoder.push(&wire[101..65_541]).unwrap());
    frames.extend(decoder.push(&wire[65_541..]).unwrap());
    decoder.end().unwrap();
    assert_eq!(frames, vec![payload]);
}

#[test]
fn handles_every_split_point_across_a_frame() {
    let wire = encode_frame(&[10, 20, 30, 40]).unwrap();
    for split in 0..=wire.len() {
        let mut decoder = FrameDecoder::new(FrameDecoderOptions::new()).unwrap();
        let mut frames = Vec::new();
        frames.extend(decoder.push(&wire[..split]).unwrap());
        frames.extend(decoder.push(&wire[split..]).unwrap());
        decoder.end().unwrap();
        assert_eq!(frames, vec![vec![10, 20, 30, 40]], "split at {split}");
    }
}

#[test]
fn copies_payload_bytes_instead_of_retaining_or_aliasing_input_chunks() {
    let mut chunk = encode_frame(&[1, 2, 3]).unwrap();
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::new()).unwrap();
    let frames = decoder.push(&chunk).unwrap();
    chunk.fill(9);
    assert_eq!(frames, vec![vec![1, 2, 3]]);
}

#[test]
fn accepts_empty_chunks_and_a_clean_empty_stream() {
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::new()).unwrap();
    let empty: Vec<Vec<u8>> = Vec::new();
    assert_eq!(decoder.push(&[]).unwrap(), empty);
    assert!(decoder.end().is_ok());
}

#[test]
fn rejects_a_truncated_stream_at_end() {
    for wire in [&[0u8, 0, 0][..], &[0u8, 0, 0, 2, 1][..]] {
        let mut decoder = FrameDecoder::new(FrameDecoderOptions::new()).unwrap();
        let empty: Vec<Vec<u8>> = Vec::new();
        assert_eq!(decoder.push(wire).unwrap(), empty);
        assert!(decoder.end().is_err());
    }
}

#[test]
fn rejects_an_oversized_declared_length_as_soon_as_its_header_is_complete() {
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::new().max_frame_length(3)).unwrap();
    assert!(decoder.push(&[0, 0, 0, 4]).is_err());
    assert!(matches!(decoder.push(&[1]), Err(FrameError(msg)) if msg.contains("failed")));
}

#[test]
fn accepts_a_frame_exactly_at_the_configured_maximum() {
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::new().max_frame_length(3)).unwrap();
    assert_eq!(
        decoder.push(&encode_frame(&[1, 2, 3]).unwrap()).unwrap(),
        vec![vec![1, 2, 3]]
    );
    decoder.end().unwrap();
}

#[test]
fn cannot_be_pushed_after_end() {
    let mut decoder = FrameDecoder::new(FrameDecoderOptions::new()).unwrap();
    decoder.end().unwrap();
    assert!(decoder.push(&[]).unwrap_err().to_string().contains("ended"));
    assert!(decoder.end().unwrap_err().to_string().contains("ended"));
}

#[test]
fn rejects_invalid_maximum_frame_length() {
    // -1, 1.5, NaN have no usize form; the overflow case (u64 > MAX_UINT32)
    // maps to exceeding the 32-bit limit.
    let too_big = (DEFAULT_MAX_FRAME_LENGTH as u64) * 1_000;
    let options = FrameDecoderOptions::new().max_frame_length(too_big as usize);
    assert!(FrameDecoder::new(options).is_err());
    assert!(encode_frame(&[]).is_ok());
}
