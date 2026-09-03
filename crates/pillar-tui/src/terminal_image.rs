//! Port of packages/tui/src/terminal-image.ts (pi v0.84.3): terminal
//! capability detection, image dimension probing, Kitty/iTerm2 escape
//! encoding, image cell sizing, and text fallbacks.
//!
//! divergences: environment detection is supplied by the host via
//! [`DetectionEnv`] instead of reading `process.env` directly (the
//! tmux hyperlink probe becomes a closure); base64 decoding uses the
//! `base64` crate.

use std::cell::RefCell;
use std::collections::VecDeque;

use base64::Engine;

/// Image protocol support (upstream `ImageProtocol`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    Iterm2,
}

/// Terminal capabilities (upstream `TerminalCapabilities`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    pub images: Option<ImageProtocol>,
    pub true_color: bool,
    pub hyperlinks: bool,
}

/// Cell dimensions in pixels (upstream `CellDimensions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

impl Default for CellDimensions {
    fn default() -> Self {
        Self {
            width_px: 9,
            height_px: 18,
        }
    }
}

/// Image dimensions in pixels (upstream `ImageDimensions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions {
    pub width_px: u32,
    pub height_px: u32,
}

impl Default for ImageDimensions {
    fn default() -> Self {
        Self {
            width_px: 800,
            height_px: 600,
        }
    }
}

/// Host-supplied environment snapshot for detection (upstream
/// `process.env` reads plus the tmux probe).
#[derive(Debug, Clone, Default)]
pub struct DetectionEnv {
    pub term_program: Option<String>,
    pub terminal_emulator: Option<String>,
    pub term: Option<String>,
    pub colorterm: Option<String>,
    pub is_windows: bool,
    pub tmux: bool,
    /// Whether the attached tmux client forwards OSC 8 hyperlinks.
    pub tmux_forwards_hyperlinks: bool,
    pub kitty_window_id: bool,
    pub ghostty: bool,
    pub wezterm: bool,
    pub warp: bool,
    pub iterm_session: bool,
    pub wt_session: bool,
    pub vscode: bool,
    pub alacritty: bool,
}

fn env_lowercase(env: &DetectionEnv, key: fn(&DetectionEnv) -> &Option<String>) -> String {
    key(env).as_deref().unwrap_or("").to_lowercase()
}

fn detect_from_environment(env: &DetectionEnv) -> TerminalCapabilities {
    let term_program = env_lowercase(env, |e| &e.term_program);
    let terminal_emulator = env_lowercase(env, |e| &e.terminal_emulator);
    let term = env_lowercase(env, |e| &e.term);
    let colorterm = env_lowercase(env, |e| &e.colorterm);
    let has_true_color_hint = colorterm == "truecolor" || colorterm == "24bit";

    // tmux: image protocols unreliable; hyperlinks only if forwarded.
    if env.tmux || term.starts_with("tmux") {
        return TerminalCapabilities {
            images: None,
            true_color: has_true_color_hint,
            hyperlinks: env.tmux_forwards_hyperlinks,
        };
    }
    // screen does not forward OSC 8.
    if term.starts_with("screen") {
        return TerminalCapabilities {
            images: None,
            true_color: has_true_color_hint,
            hyperlinks: false,
        };
    }
    let kitty_caps = TerminalCapabilities {
        images: Some(ImageProtocol::Kitty),
        true_color: true,
        hyperlinks: true,
    };
    if env.kitty_window_id || term_program == "kitty" {
        return kitty_caps;
    }
    if term_program == "ghostty" || term.contains("ghostty") || env.ghostty {
        return kitty_caps;
    }
    if env.wezterm || term_program == "wezterm" {
        return kitty_caps;
    }
    if term_program == "warpterminal" || env.warp {
        return kitty_caps;
    }
    if env.iterm_session || term_program == "iterm.app" {
        return TerminalCapabilities {
            images: Some(ImageProtocol::Iterm2),
            true_color: true,
            hyperlinks: true,
        };
    }
    if env.wt_session
        || env.vscode
        || env.alacritty
        || term_program == "vscode"
        || term_program == "alacritty"
    {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: true,
        };
    }
    if terminal_emulator == "jetbrains-jediterm" {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        };
    }
    if env.is_windows {
        return TerminalCapabilities {
            images: None,
            true_color: true,
            hyperlinks: false,
        };
    }
    // Unknown terminal: conservative defaults.
    TerminalCapabilities {
        images: None,
        true_color: has_true_color_hint,
        hyperlinks: false,
    }
}

/// Capability override values (upstream the `PI_*` env vars plus
/// `setCapabilityOverrides`).
#[derive(Debug, Clone, Copy, Default)]
pub struct CapabilityOverrides {
    /// PI_HYPERLINKS / override: Some(true) = force on, Some(false) = force off.
    pub hyperlinks: Option<bool>,
    /// PI_IMAGE_PROTOCOL: kitty / iterm2 / none.
    pub image_protocol: Option<Option<ImageProtocol>>,
    /// PI_TRUE_COLOR.
    pub true_color: Option<bool>,
}

thread_local! {
    static CELL_DIMENSIONS: RefCell<CellDimensions> = RefCell::new(CellDimensions::default());
    static CACHED_CAPABILITIES: RefCell<Option<TerminalCapabilities>> = const { RefCell::new(None) };
    static OVERRIDES: RefCell<CapabilityOverrides> = RefCell::new(CapabilityOverrides::default());
}

/// Current cell dimensions (upstream `getCellDimensions`).
pub fn get_cell_dimensions() -> CellDimensions {
    CELL_DIMENSIONS.with_borrow(|d| *d)
}

/// Set cell dimensions (upstream `setCellDimensions`); called by the
/// TUI when the terminal responds to the dimension query.
pub fn set_cell_dimensions(dims: CellDimensions) {
    CELL_DIMENSIONS.with_borrow_mut(|d| *d = dims);
}

/// Detect capabilities from a host-supplied environment (upstream
/// `detectCapabilities` + `detectCapabilitiesFromEnvironment`).
pub fn detect_capabilities(
    env: &DetectionEnv,
    overrides: &CapabilityOverrides,
) -> TerminalCapabilities {
    let mut detected = detect_from_environment(env);
    if let Some(hyperlinks) = overrides.hyperlinks {
        detected.hyperlinks = hyperlinks;
    }
    if let Some(images) = overrides.image_protocol {
        detected.images = images;
    }
    if let Some(true_color) = overrides.true_color {
        detected.true_color = true_color;
    }
    detected
}

/// Cached capabilities (upstream `getCapabilities`). Uses the current
/// overrides; call [`set_overrides`] to invalidate.
///
/// [`set_overrides`]: set_overrides
pub fn get_capabilities() -> TerminalCapabilities {
    CACHED_CAPABILITIES.with_borrow_mut(|cached| {
        if cached.is_none() {
            let overrides = OVERRIDES.with_borrow(|o| *o);
            // Cache is computed by the host after detection; without an
            // environment here the conservative default applies.
            let env = DetectionEnv::default();
            *cached = Some(detect_capabilities(&env, &overrides));
        }
        cached.unwrap()
    })
}

/// Override cached capabilities directly (upstream `setCapabilities`,
/// useful in tests).
pub fn set_capabilities(caps: TerminalCapabilities) {
    CACHED_CAPABILITIES.with_borrow_mut(|cached| *cached = Some(caps));
}

/// Override selected auto-detected capabilities (upstream
/// `setCapabilityOverrides`); invalidates the cache.
pub fn set_overrides(overrides: CapabilityOverrides) {
    OVERRIDES.with_borrow_mut(|o| *o = overrides);
    reset_capabilities_cache();
}

/// Clear the capability cache (upstream `resetCapabilitiesCache`).
pub fn reset_capabilities_cache() {
    CACHED_CAPABILITIES.with_borrow_mut(|cached| *cached = None);
}

const KITTY_PREFIX: &str = "\u{1b}_G";
const ITERM2_PREFIX: &str = "\u{1b}]1337;File=";

/// Whether a line carries an image escape sequence (upstream
/// `isImageLine`).
pub fn is_image_line(line: &str) -> bool {
    line.starts_with(KITTY_PREFIX)
        || line.starts_with(ITERM2_PREFIX)
        || line.contains(KITTY_PREFIX)
        || line.contains(ITERM2_PREFIX)
}

/// Allocate a random Kitty image ID in [1, 0xffffffff] (upstream
/// `allocateImageId`).
pub fn allocate_image_id() -> u32 {
    // Random source unavailable without a dependency; use a cheap
    // time-based counter seeded from the address entropy.
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(1);
    nanos.max(1)
}

const CHUNK_SIZE: usize = 4096;

/// Encode a Kitty graphics transmission (upstream `encodeKitty`):
/// `a=T,f=100,q=2` with optional cursor movement suppression, cell
/// size, image ID, and 4096-byte chunking with m=1/m=0 continuation.
pub fn encode_kitty(
    base64_data: &str,
    columns: Option<u32>,
    rows: Option<u32>,
    image_id: Option<u32>,
    move_cursor: Option<bool>,
) -> String {
    let mut params: Vec<String> = vec!["a=T".to_string(), "f=100".to_string(), "q=2".to_string()];
    if move_cursor == Some(false) {
        params.push("C=1".to_string());
    }
    if let Some(columns) = columns {
        params.push(format!("c={columns}"));
    }
    if let Some(rows) = rows {
        params.push(format!("r={rows}"));
    }
    if let Some(image_id) = image_id {
        params.push(format!("i={image_id}"));
    }
    let params = params.join(",");

    if base64_data.len() <= CHUNK_SIZE {
        return format!("\u{1b}_G{params};{base64_data}\u{1b}\\");
    }

    let mut chunks: Vec<String> = Vec::new();
    let mut offset = 0usize;
    let mut is_first = true;
    while offset < base64_data.len() {
        let end = (offset + CHUNK_SIZE).min(base64_data.len());
        let chunk = &base64_data[offset..end];
        let is_last = offset + CHUNK_SIZE >= base64_data.len();
        if is_first {
            chunks.push(format!("\u{1b}_G{params},m=1;{chunk}\u{1b}\\"));
            is_first = false;
        } else if is_last {
            chunks.push(format!("\u{1b}_Gm=0;{chunk}\u{1b}\\"));
        } else {
            chunks.push(format!("\u{1b}_Gm=1;{chunk}\u{1b}\\"));
        }
        offset += CHUNK_SIZE;
    }
    chunks.join("")
}

/// Delete a Kitty image by ID, freeing the data (upstream
/// `deleteKittyImage`).
pub fn delete_kitty_image(image_id: u32) -> String {
    format!("\u{1b}_Ga=d,d=I,i={image_id},q=2\u{1b}\\")
}

/// Delete all visible Kitty images, freeing the data (upstream
/// `deleteAllKittyImages`).
pub fn delete_all_kitty_images() -> String {
    "\u{1b}_Ga=d,d=A,q=2\u{1b}\\".to_string()
}

/// Delete all visible Kitty placements, retaining the data (upstream
/// `deleteAllKittyPlacements`).
pub fn delete_all_kitty_placements() -> String {
    "\u{1b}_Ga=d,d=a,q=2\u{1b}\\".to_string()
}

/// Encode an iTerm2 inline image (upstream `encodeITerm2`).
pub fn encode_iterm2(
    base64_data: &str,
    width: Option<&str>,
    height: Option<&str>,
    name: Option<&str>,
    preserve_aspect_ratio: bool,
    inline: bool,
) -> String {
    let size = base64_data.len();
    let mut params: Vec<String> = vec![
        format!("inline={}", if inline { 1 } else { 0 }),
        format!("size={size}"),
    ];
    if let Some(width) = width {
        params.push(format!("width={width}"));
    }
    if let Some(height) = height {
        params.push(format!("height={height}"));
    }
    if let Some(name) = name {
        let name_base64 = base64::engine::general_purpose::STANDARD.encode(name.as_bytes());
        params.push(format!("name={name_base64}"));
    }
    if !preserve_aspect_ratio {
        params.push("preserveAspectRatio=0".to_string());
    }
    format!("\u{1b}]1337;File={}:{base64_data}\u{7}", params.join(";"))
}

/// Cell size an image occupies (upstream `ImageCellSize`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCellSize {
    pub columns: usize,
    pub rows: usize,
}

/// Compute the cell size for an image within bounds, preserving aspect
/// ratio (upstream `calculateImageCellSize`).
pub fn calculate_image_cell_size(
    image_dimensions: ImageDimensions,
    max_width_cells: usize,
    max_height_cells: Option<usize>,
    cell_dimensions: CellDimensions,
) -> ImageCellSize {
    let max_width = max_width_cells.max(1);
    let max_height = max_height_cells.map(|h| h.max(1));
    let image_width = image_dimensions.width_px.max(1) as f64;
    let image_height = image_dimensions.height_px.max(1) as f64;

    let width_scale = (max_width as f64 * cell_dimensions.width_px as f64) / image_width;
    let height_scale = match max_height {
        None => width_scale,
        Some(max_height) => (max_height as f64 * cell_dimensions.height_px as f64) / image_height,
    };
    let scale = width_scale.min(height_scale);

    let scaled_width_px = image_width * scale;
    let scaled_height_px = image_height * scale;
    let columns = (scaled_width_px / cell_dimensions.width_px as f64).ceil() as usize;
    let rows = (scaled_height_px / cell_dimensions.height_px as f64).ceil() as usize;

    ImageCellSize {
        columns: columns.clamp(1, max_width),
        rows: match max_height {
            None => rows.max(1),
            Some(max_height) => rows.clamp(1, max_height),
        },
    }
}

/// Row count an image occupies at a target width (upstream
/// `calculateImageRows`).
pub fn calculate_image_rows(
    image_dimensions: ImageDimensions,
    target_width_cells: usize,
    cell_dimensions: CellDimensions,
) -> usize {
    calculate_image_cell_size(image_dimensions, target_width_cells, None, cell_dimensions).rows
}

fn decode_base64(base64_data: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(base64_data)
        .ok()
}

/// PNG dimension probe (upstream `getPngDimensions`): IHDR at byte 16.
pub fn get_png_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = decode_base64(base64_data)?;
    if buffer.len() < 24 {
        return None;
    }
    if buffer[0] != 0x89 || buffer[1] != 0x50 || buffer[2] != 0x4e || buffer[3] != 0x47 {
        return None;
    }
    let width = u32::from_be_bytes([buffer[16], buffer[17], buffer[18], buffer[19]]);
    let height = u32::from_be_bytes([buffer[20], buffer[21], buffer[22], buffer[23]]);
    Some(ImageDimensions {
        width_px: width,
        height_px: height,
    })
}

/// JPEG dimension probe (upstream `getJpegDimensions`): scans for the
/// first SOF0–SOF2 marker.
pub fn get_jpeg_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = decode_base64(base64_data)?;
    if buffer.len() < 2 {
        return None;
    }
    if buffer[0] != 0xff || buffer[1] != 0xd8 {
        return None;
    }
    let mut offset = 2usize;
    while offset < buffer.len().saturating_sub(9) {
        if buffer[offset] != 0xff {
            offset += 1;
            continue;
        }
        let marker = buffer[offset + 1];
        if (0xc0..=0xc2).contains(&marker) {
            let height = u16::from_be_bytes([buffer[offset + 5], buffer[offset + 6]]);
            let width = u16::from_be_bytes([buffer[offset + 7], buffer[offset + 8]]);
            return Some(ImageDimensions {
                width_px: width as u32,
                height_px: height as u32,
            });
        }
        if offset + 3 >= buffer.len() {
            return None;
        }
        let length = u16::from_be_bytes([buffer[offset + 2], buffer[offset + 3]]);
        if length < 2 {
            return None;
        }
        offset += 2 + length as usize;
    }
    None
}

/// GIF dimension probe (upstream `getGifDimensions`): little-endian
/// size at bytes 6/8.
pub fn get_gif_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = decode_base64(base64_data)?;
    if buffer.len() < 10 {
        return None;
    }
    let sig = &buffer[..6];
    if sig != b"GIF87a" && sig != b"GIF89a" {
        return None;
    }
    let width = u16::from_le_bytes([buffer[6], buffer[7]]);
    let height = u16::from_le_bytes([buffer[8], buffer[9]]);
    Some(ImageDimensions {
        width_px: width as u32,
        height_px: height as u32,
    })
}

/// WebP dimension probe (upstream `getWebpDimensions`): VP8 / VP8L /
/// VP8X chunk variants.
pub fn get_webp_dimensions(base64_data: &str) -> Option<ImageDimensions> {
    let buffer = decode_base64(base64_data)?;
    if buffer.len() < 30 {
        return None;
    }
    if &buffer[..4] != b"RIFF" || &buffer[8..12] != b"WEBP" {
        return None;
    }
    match &buffer[12..16] {
        b"VP8 " => {
            if buffer.len() < 30 {
                return None;
            }
            let width = (u16::from_le_bytes([buffer[26], buffer[27]]) & 0x3fff) as u32;
            let height = (u16::from_le_bytes([buffer[28], buffer[29]]) & 0x3fff) as u32;
            Some(ImageDimensions {
                width_px: width,
                height_px: height,
            })
        }
        b"VP8L" => {
            if buffer.len() < 25 {
                return None;
            }
            let bits = u32::from_le_bytes([buffer[21], buffer[22], buffer[23], buffer[24]]);
            let width = (bits & 0x3fff) + 1;
            let height = ((bits >> 14) & 0x3fff) + 1;
            Some(ImageDimensions {
                width_px: width,
                height_px: height,
            })
        }
        b"VP8X" => {
            if buffer.len() < 30 {
                return None;
            }
            let width =
                (buffer[24] as u32 | (buffer[25] as u32) << 8 | (buffer[26] as u32) << 16) + 1;
            let height =
                (buffer[27] as u32 | (buffer[28] as u32) << 8 | (buffer[29] as u32) << 16) + 1;
            Some(ImageDimensions {
                width_px: width,
                height_px: height,
            })
        }
        _ => None,
    }
}

/// Dimension probe by MIME type (upstream `getImageDimensions`).
pub fn get_image_dimensions(base64_data: &str, mime_type: &str) -> Option<ImageDimensions> {
    match mime_type {
        "image/png" => get_png_dimensions(base64_data),
        "image/jpeg" => get_jpeg_dimensions(base64_data),
        "image/gif" => get_gif_dimensions(base64_data),
        "image/webp" => get_webp_dimensions(base64_data),
        _ => None,
    }
}

/// Wrap text in an OSC 8 hyperlink sequence (upstream `hyperlink`).
pub fn hyperlink(text: &str, url: &str) -> String {
    format!("\u{1b}]8;;{url}\u{1b}\\{text}\u{1b}]8;;\u{1b}\\")
}

/// Text fallback when the terminal cannot render inline images
/// (upstream `imageFallback`): `[Image: <filename> [<mime>] WxH]`.
pub fn image_fallback(
    mime_type: &str,
    dimensions: Option<ImageDimensions>,
    filename: Option<&str>,
    hyperlinks: bool,
) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(filename) = filename {
        if hyperlinks && filename.starts_with('/') {
            parts.push(hyperlink(filename, &format!("file://{filename}")));
        } else {
            parts.push(filename.to_string());
        }
    }
    parts.push(format!("[{mime_type}]"));
    if let Some(dimensions) = dimensions {
        parts.push(format!("{}x{}", dimensions.width_px, dimensions.height_px));
    }
    format!("[Image: {}]", parts.join(" "))
}

// ============================================================================
// Kitty placement metadata (upstream registerKittyImageMetadata et al)
// ============================================================================

/// Registered Kitty image metadata (upstream `KittyImageMetadata` plus
/// the transmission generation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyImageMetadata {
    pub image_id: u32,
    pub columns: usize,
    pub rows: usize,
    pub width_px: u32,
    pub height_px: u32,
    pub transmission_generation: u64,
}

thread_local! {
    static KITTY_METADATA: RefCell<VecDeque<KittyImageMetadata>> = const { RefCell::new(VecDeque::new()) };
    static TRANSMISSION_GENERATION: RefCell<u64> = const { RefCell::new(0) };
}

const KITTY_METADATA_MAX: usize = 1000;

/// Register image metadata for placement extraction (upstream
/// `registerKittyImageMetadata`): bumps the generation, replaces an
/// existing entry, and evicts the oldest past 1000 entries.
pub fn register_kitty_image_metadata(metadata: KittyImageMetadata) {
    TRANSMISSION_GENERATION.with_borrow_mut(|g| *g += 1);
    let generation = TRANSMISSION_GENERATION.with_borrow(|g| *g);
    KITTY_METADATA.with_borrow_mut(|registry| {
        registry.retain(|m| m.image_id != metadata.image_id);
        let mut entry = metadata;
        entry.transmission_generation = generation;
        registry.push_back(entry);
        while registry.len() > KITTY_METADATA_MAX {
            registry.pop_front();
        }
    });
}

/// Look up metadata for a rendered image line (upstream
/// `getKittyImageMetadata`).
pub fn get_kitty_image_metadata(line: &str) -> Option<KittyImageMetadata> {
    let controls_start = line.find(KITTY_PREFIX)? + KITTY_PREFIX.len();
    let controls_end = line[controls_start..].find(';')? + controls_start;
    let controls = &line[controls_start..controls_end];
    let image_id = extract_control_value(controls, "i")?.parse::<u32>().ok()?;
    KITTY_METADATA.with_borrow(|registry| registry.iter().find(|m| m.image_id == image_id).cloned())
}

fn extract_control_value(controls: &str, key: &str) -> Option<String> {
    for control in controls.split(',') {
        let mut parts = control.splitn(2, '=');
        if parts.next() == Some(key) {
            return parts.next().map(str::to_string);
        }
    }
    None
}
