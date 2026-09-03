//! Parity tests for terminal-image (pi v0.84.3 terminal-image.ts):
//! capability detection, dimension probing, Kitty/iTerm2 encoding,
//! cell sizing, and fallbacks.

use pillar_tui::terminal_image::{
    CapabilityOverrides, CellDimensions, DetectionEnv, ImageProtocol, TerminalCapabilities,
    allocate_image_id, calculate_image_cell_size, calculate_image_rows, delete_all_kitty_images,
    delete_all_kitty_placements, delete_kitty_image, detect_capabilities, encode_iterm2,
    encode_kitty, get_capabilities, get_cell_dimensions, get_image_dimensions,
    get_kitty_image_metadata, hyperlink, image_fallback, is_image_line,
    register_kitty_image_metadata, reset_capabilities_cache, set_capabilities, set_cell_dimensions,
    set_overrides,
};

fn no_caps() -> TerminalCapabilities {
    TerminalCapabilities {
        images: None,
        true_color: false,
        hyperlinks: false,
    }
}

// --- capability detection ---------------------------------------------------------------------

#[test]
fn detect_unknown_terminal_is_conservative() {
    let env = DetectionEnv::default();
    let caps = detect_capabilities(&env, &CapabilityOverrides::default());
    assert_eq!(caps.images, None);
    assert!(!caps.hyperlinks);
    assert!(!caps.true_color);
}

#[test]
fn detect_kitty_from_term_program() {
    let env = DetectionEnv {
        term_program: Some("kitty".to_string()),
        ..DetectionEnv::default()
    };
    let caps = detect_capabilities(&env, &CapabilityOverrides::default());
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
    assert!(caps.true_color);
    assert!(caps.hyperlinks);
}

#[test]
fn detect_ghostty_by_resources_dir() {
    let env = DetectionEnv {
        ghostty: true,
        ..DetectionEnv::default()
    };
    let caps = detect_capabilities(&env, &CapabilityOverrides::default());
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
}

#[test]
fn detect_iterm2() {
    let env = DetectionEnv {
        iterm_session: true,
        ..DetectionEnv::default()
    };
    let caps = detect_capabilities(&env, &CapabilityOverrides::default());
    assert_eq!(caps.images, Some(ImageProtocol::Iterm2));
}

#[test]
fn detect_tmux_disables_images_and_gates_hyperlinks() {
    let env = DetectionEnv {
        tmux: true,
        tmux_forwards_hyperlinks: false,
        colorterm: Some("truecolor".to_string()),
        ..DetectionEnv::default()
    };
    let caps = detect_capabilities(&env, &CapabilityOverrides::default());
    assert_eq!(caps.images, None);
    assert!(!caps.hyperlinks);
    assert!(caps.true_color);
    // With forwarding confirmed, hyperlinks turn on.
    let env = DetectionEnv {
        tmux: true,
        tmux_forwards_hyperlinks: true,
        ..DetectionEnv::default()
    };
    let caps = detect_capabilities(&env, &CapabilityOverrides::default());
    assert!(caps.hyperlinks);
}

#[test]
fn detect_screen_disables_hyperlinks() {
    let env = DetectionEnv {
        term: Some("screen".to_string()),
        ..DetectionEnv::default()
    };
    let caps = detect_capabilities(&env, &CapabilityOverrides::default());
    assert_eq!(caps.images, None);
    assert!(!caps.hyperlinks);
}

#[test]
fn detect_truecolor_hint() {
    let env = DetectionEnv {
        colorterm: Some("24bit".to_string()),
        ..DetectionEnv::default()
    };
    let caps = detect_capabilities(&env, &CapabilityOverrides::default());
    assert!(caps.true_color);
}

#[test]
fn detect_vscode_alacritty_wt_no_images_but_hyperlinks() {
    for env in [
        DetectionEnv {
            vscode: true,
            ..DetectionEnv::default()
        },
        DetectionEnv {
            alacritty: true,
            ..DetectionEnv::default()
        },
        DetectionEnv {
            wt_session: true,
            ..DetectionEnv::default()
        },
    ] {
        let caps = detect_capabilities(&env, &CapabilityOverrides::default());
        assert_eq!(caps.images, None);
        assert!(caps.hyperlinks);
    }
}

#[test]
fn detect_overrides_take_precedence() {
    let env = DetectionEnv::default();
    let overrides = CapabilityOverrides {
        hyperlinks: Some(true),
        image_protocol: Some(Some(ImageProtocol::Kitty)),
        true_color: Some(true),
    };
    let caps = detect_capabilities(&env, &overrides);
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
    assert!(caps.true_color);
    assert!(caps.hyperlinks);
    // Force-disable image protocol.
    let overrides = CapabilityOverrides {
        image_protocol: Some(None),
        ..CapabilityOverrides::default()
    };
    let env = DetectionEnv {
        kitty_window_id: true,
        ..DetectionEnv::default()
    };
    let caps = detect_capabilities(&env, &overrides);
    assert_eq!(caps.images, None);
}

// --- cached capabilities -------------------------------------------------------------------------

#[test]
fn capabilities_cache_and_reset() {
    set_capabilities(no_caps());
    assert_eq!(get_capabilities(), no_caps());
    reset_capabilities_cache();
    // After reset without env detection the conservative default applies.
    let caps = get_capabilities();
    assert_eq!(caps.images, None);
    // Cleanup for other tests.
    set_capabilities(no_caps());
}

#[test]
fn set_overrides_invalidates_cache() {
    set_capabilities(TerminalCapabilities {
        images: Some(ImageProtocol::Kitty),
        true_color: true,
        hyperlinks: true,
    });
    assert_eq!(get_capabilities().images, Some(ImageProtocol::Kitty));
    set_overrides(CapabilityOverrides {
        image_protocol: Some(None),
        ..CapabilityOverrides::default()
    });
    assert_eq!(get_capabilities().images, None);
    // Cleanup.
    set_capabilities(no_caps());
}

// --- cell dimensions ------------------------------------------------------------------------------

#[test]
fn default_cell_dimensions() {
    assert_eq!(
        get_cell_dimensions(),
        CellDimensions {
            width_px: 9,
            height_px: 18
        }
    );
    set_cell_dimensions(CellDimensions {
        width_px: 10,
        height_px: 20,
    });
    assert_eq!(
        get_cell_dimensions(),
        CellDimensions {
            width_px: 10,
            height_px: 20
        }
    );
    set_cell_dimensions(CellDimensions {
        width_px: 9,
        height_px: 18,
    });
}

// --- is_image_line ----------------------------------------------------------------------------------

#[test]
fn is_image_line_kitty_and_iterm2() {
    assert!(is_image_line("\u{1b}_Ga=T,f=100;q=2;abc\u{1b}\\"));
    assert!(is_image_line("\u{1b}]1337;File=inline=1;abc\u{7}"));
    // Multi-row images have a cursor-up prefix before the sequence.
    assert!(is_image_line("\u{1b}[5A\u{1b}_Ga=T;abc\u{1b}\\"));
    assert!(!is_image_line("plain text"));
}

// --- kitty encoding ---------------------------------------------------------------------------------

#[test]
fn encode_kitty_small_payload() {
    let seq = encode_kitty("QUJD", Some(10), Some(5), Some(42), Some(false));
    assert_eq!(seq, "\u{1b}_Ga=T,f=100,q=2,C=1,c=10,r=5,i=42;QUJD\u{1b}\\");
}

#[test]
fn encode_kitty_without_move_cursor() {
    let seq = encode_kitty("QUJD", None, None, None, None);
    assert_eq!(seq, "\u{1b}_Ga=T,f=100,q=2;QUJD\u{1b}\\");
}

#[test]
fn encode_kitty_chunks_long_payload() {
    let payload = "A".repeat(10_000);
    let seq = encode_kitty(&payload, None, None, None, None);
    // 10000 bytes → 3 chunks (4096 + 4096 + 1808).
    let chunk_count = seq.matches("\u{1b}_G").count();
    assert_eq!(chunk_count, 3);
    assert!(seq.contains("m=1;"), "{seq:?}");
    assert!(seq.contains("m=0;"), "{seq:?}");
    // First chunk carries the params with m=1.
    assert!(seq.starts_with("\u{1b}_Ga=T,f=100,q=2,m=1;"), "{seq:?}");
    // Ends with the m=0 terminator chunk.
    assert!(seq.contains("\u{1b}_Gm=0;"), "{seq:?}");
    assert!(seq.ends_with("\u{1b}\\"), "{seq:?}");
}

#[test]
fn encode_kitty_exact_chunk_boundary() {
    let payload = "A".repeat(4096);
    let seq = encode_kitty(&payload, None, None, None, None);
    // 4096 ≤ CHUNK_SIZE → single chunk, no m flags.
    assert_eq!(
        seq,
        "\u{1b}_Ga=T,f=100,q=2;AAAA\u{1b}\\".replace("AAAA", &payload)
    );
}

#[test]
fn delete_kitty_commands() {
    assert_eq!(delete_kitty_image(7), "\u{1b}_Ga=d,d=I,i=7,q=2\u{1b}\\");
    assert_eq!(delete_all_kitty_images(), "\u{1b}_Ga=d,d=A,q=2\u{1b}\\");
    assert_eq!(delete_all_kitty_placements(), "\u{1b}_Ga=d,d=a,q=2\u{1b}\\");
}

// --- iterm2 encoding ---------------------------------------------------------------------------------

#[test]
fn encode_iterm2_basic() {
    let seq = encode_iterm2("QUJD", Some("40"), Some("auto"), None, true, true);
    assert_eq!(
        seq,
        "\u{1b}]1337;File=inline=1;size=4;width=40;height=auto:QUJD\u{7}"
    );
}

#[test]
fn encode_iterm2_name_and_aspect() {
    let seq = encode_iterm2("QQ==", None, None, Some("x.png"), false, true);
    // name is base64-encoded.
    assert!(seq.contains("name=eC5wbmc="), "{seq:?}");
    assert!(seq.contains("preserveAspectRatio=0"), "{seq:?}");
}

// --- image cell sizing ---------------------------------------------------------------------------------

fn dims(w: u32, h: u32) -> pillar_tui::terminal_image::ImageDimensions {
    pillar_tui::terminal_image::ImageDimensions {
        width_px: w,
        height_px: h,
    }
}

#[test]
fn cell_size_wide_image_fits_width() {
    // 1800x900 px in 9x18 cells: natural width 200 cells, height 50.
    let size = calculate_image_cell_size(
        dims(1800, 900),
        40,
        None,
        CellDimensions {
            width_px: 9,
            height_px: 18,
        },
    );
    // Width-limited: scale = 40*9/1800 = 0.2 → 360x180 px → 40x10 cells.
    assert_eq!(size.columns, 40);
    assert_eq!(size.rows, 10);
}

#[test]
fn cell_size_tall_image_limited_by_height() {
    let size = calculate_image_cell_size(
        dims(900, 3600),
        80,
        Some(10),
        CellDimensions {
            width_px: 9,
            height_px: 18,
        },
    );
    // widthScale = 80*9/900 = 0.8; heightScale = 10*18/3600 = 0.05.
    // Height wins: 45x180 px → 5x10 cells.
    assert_eq!(size.columns, 5);
    assert_eq!(size.rows, 10);
}

#[test]
fn cell_size_square_natural() {
    let size = calculate_image_cell_size(
        dims(90, 180),
        80,
        None,
        CellDimensions {
            width_px: 9,
            height_px: 18,
        },
    );
    // widthScale = heightScale = 80*9/90 = 8 → scaled 720x1440 px →
    // 80x80 cells (both clamped at the max).
    assert_eq!((size.columns, size.rows), (80, 80));
}

#[test]
fn image_rows_at_width() {
    assert_eq!(
        calculate_image_rows(
            dims(1800, 900),
            40,
            CellDimensions {
                width_px: 9,
                height_px: 18
            }
        ),
        10
    );
}

#[test]
fn image_id_allocation_positive() {
    let id = allocate_image_id();
    assert!(id >= 1);
}

// --- dimension probing ---------------------------------------------------------------------------

use base64::Engine;

fn png_base64(width: u32, height: u32) -> String {
    let mut buf = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    buf.extend_from_slice(&13u32.to_be_bytes());
    buf.extend_from_slice(b"IHDR");
    buf.extend_from_slice(&width.to_be_bytes());
    buf.extend_from_slice(&height.to_be_bytes());
    buf.extend_from_slice(&[8, 6, 0, 0, 0]);
    base64::engine::general_purpose::STANDARD.encode(buf)
}

fn jpeg_base64(width: u16, height: u16) -> String {
    let mut buf = vec![0xff, 0xd8];
    // SOF0 marker: FFC0, length 17 (8+2+2+2+3), precision 8, h, w, components 3.
    buf.extend_from_slice(&[0xff, 0xc0]);
    buf.extend_from_slice(&17u16.to_be_bytes());
    buf.extend_from_slice(&[8]);
    buf.extend_from_slice(&height.to_be_bytes());
    buf.extend_from_slice(&width.to_be_bytes());
    buf.extend_from_slice(&[3, 1, 0x22, 0, 2, 0x11, 1, 3, 0x11]);
    base64::engine::general_purpose::STANDARD.encode(buf)
}

fn gif_base64(width: u16, height: u16) -> String {
    let mut buf = b"GIF89a".to_vec();
    buf.extend_from_slice(&width.to_le_bytes());
    buf.extend_from_slice(&height.to_le_bytes());
    buf.extend_from_slice(&[0, 0, 0]);
    base64::engine::general_purpose::STANDARD.encode(buf)
}

fn webp_vp8_base64(width: u16, height: u16) -> String {
    let mut buf = b"RIFF".to_vec();
    buf.extend_from_slice(&40u32.to_le_bytes());
    buf.extend_from_slice(b"WEBP");
    buf.extend_from_slice(b"VP8 ");
    buf.extend_from_slice(&20u32.to_le_bytes());
    // VP8 bitstream header: frame tag (3 bytes), then 3-byte start code,
    // then 2-byte width / 2-byte height (LE, 14-bit + scale bits).
    buf.extend_from_slice(&[0x30, 0x01, 0x00, 0x9d, 0x01, 0x2a]);
    buf.extend_from_slice(&(width & 0x3fff).to_le_bytes());
    buf.extend_from_slice(&(height & 0x3fff).to_le_bytes());
    base64::engine::general_purpose::STANDARD.encode(buf)
}

#[test]
fn png_dimensions() {
    let dims = get_image_dimensions(&png_base64(640, 480), "image/png").unwrap();
    assert_eq!((dims.width_px, dims.height_px), (640, 480));
}

#[test]
fn png_rejects_non_png() {
    assert!(get_image_dimensions(&png_base64(640, 480), "image/jpeg").is_none());
    assert!(get_image_dimensions("bm90LWEtcG5n", "image/png").is_none());
}

#[test]
fn jpeg_dimensions() {
    let dims = get_image_dimensions(&jpeg_base64(320, 240), "image/jpeg").unwrap();
    assert_eq!((dims.width_px, dims.height_px), (320, 240));
}

#[test]
fn gif_dimensions() {
    let dims = get_image_dimensions(&gif_base64(100, 50), "image/gif").unwrap();
    assert_eq!((dims.width_px, dims.height_px), (100, 50));
}

#[test]
fn webp_vp8_dimensions() {
    let dims = get_image_dimensions(&webp_vp8_base64(200, 100), "image/webp").unwrap();
    assert_eq!((dims.width_px, dims.height_px), (200, 100));
}

#[test]
fn unknown_mime_returns_none() {
    assert!(get_image_dimensions(&png_base64(10, 10), "image/bmp").is_none());
}

// --- kitty metadata registry ------------------------------------------------------------------------

#[test]
fn kitty_metadata_register_and_lookup() {
    register_kitty_image_metadata(pillar_tui::terminal_image::KittyImageMetadata {
        image_id: 5,
        columns: 10,
        rows: 4,
        width_px: 90,
        height_px: 72,
        transmission_generation: 0,
    });
    let line = "\u{1b}_Ga=T,f=100,q=2,i=5,c=10,r=4;QQ==\u{1b}\\".replace("\\u{1b}", "\u{1b}");
    let line = line.replace("\\", "");
    let meta = get_kitty_image_metadata(&line).unwrap();
    assert_eq!(meta.image_id, 5);
    assert_eq!(meta.rows, 4);
    assert!(meta.transmission_generation >= 1);
    // Unknown id → None.
    let line_unknown = "\u{1b}_Ga=T,i=999;QQ==\u{1b}\\"
        .replace("\\u{1b}", "\u{1b}")
        .replace("\\", "");
    assert!(get_kitty_image_metadata(&line_unknown).is_none());
}

// --- hyperlink / fallback ---------------------------------------------------------------------------

#[test]
fn hyperlink_wraps_osc8() {
    let out = hyperlink("text", "https://example.com");
    assert_eq!(
        out,
        "\u{1b}]8;;https://example.com\u{1b}\\text\u{1b}]8;;\u{1b}\\"
    );
}

#[test]
fn image_fallback_with_dimensions() {
    let out = image_fallback("image/png", Some(dims(800, 600)), Some("/tmp/a.png"), false);
    assert_eq!(out, "[Image: /tmp/a.png [image/png] 800x600]");
}

#[test]
fn image_fallback_without_filename() {
    let out = image_fallback("image/jpeg", None, None, false);
    assert_eq!(out, "[Image: [image/jpeg]]");
}

#[test]
fn image_fallback_hyperlinked_absolute_path() {
    let out = image_fallback("image/png", None, Some("/tmp/a.png"), true);
    assert!(out.starts_with("[Image: \u{1b}]8;;file://"), "{out:?}");
    assert!(out.contains("a.png"), "{out:?}");
}
