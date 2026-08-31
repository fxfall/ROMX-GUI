//! ROMX 0.2.0 core container implementation.
//!
//! The writer and reader follow the active ROMX 0.2.0 wire format: a raw
//! payload, RIDX index, optional strict metadata, optional strict PNG cover,
//! and a fixed 128-byte footer. No ROMX 0.1.x layout is accepted or emitted.

use image::ImageReader;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

mod frontend;
mod identity;
mod libretro;
mod lpl;
mod probe;
mod save;

pub use frontend::{check_frontend_compatibility, FrontendCompatibilityReport};
pub use identity::{identity_from_document, identity_from_path, RomxIdentity};
pub use libretro::{
    download_libretro_thumbnail, libretro_dat_url, libretro_lookup, libretro_lookup_candidates,
    libretro_lookup_result, libretro_match_mode, libretro_playlist_name, LibretroLookup,
};
pub use lpl::{
    export_lpl, export_lpl_with_output_handling, import_lpl, import_lpl_with_error_handling,
    import_lpl_with_output_handling, import_lpl_with_progress, is_official_lpl_item_field,
    is_official_lpl_root_field, plan_lpl_import, ExportLplOptions, ExportLplReport,
    ImportLplOptions, ImportLplPlan, ImportLplReport, PlannedLplItem, LPLX_METADATA_KEY,
    OFFICIAL_LPL_ITEM_FIELDS, OFFICIAL_LPL_ROOT_FIELDS, ROMX_LPLX_METADATA_FIELDS,
};
pub use probe::{
    extract_embedded_info, infer_payload_format, inspect_payload_profile, EmbeddedCover,
    EmbeddedInfo, PayloadProfile,
};
pub use save::{
    detect_save_bundles, extract_mutable_save_object, extract_mutable_save_objects,
    inspect_mutable_path, is_supported_save_file, read_mutable_save_objects, save_profile,
    MutableRegionInfo, MutableSaveFileData, MutableSaveObject, SaveGrouping, SaveInventory,
    SaveProfile,
};

pub const FOOTER_SIZE: usize = 128;
pub const RIDX_HEADER_SIZE: usize = 64;
pub const RIDX_ENTRY_SIZE: usize = 512;
pub const RIDX_PATH_CAPACITY: usize = 480;
pub const RIDX_VERSION: u16 = 1;
pub const VERSION: u32 = 2;
pub const SPEC_VERSION: &str = "0.2.0";
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
pub const DEFAULT_MAX_METADATA_SIZE: u64 = 1024 * 1024;
pub const DEFAULT_MAX_COVER_SIZE: u64 = 32 * 1024 * 1024;
pub const DEFAULT_MAX_COVER_DIMENSION: u32 = 8192;
pub const FLAG_METADATA: u32 = 1 << 0;
pub const FLAG_COVER: u32 = 1 << 1;
pub const FLAG_BODY_SHA256: u32 = 1 << 2;
pub const FLAG_ENTRY_CRC32: u32 = 1 << 3;
pub const ENTRYPOINT: u32 = 1;
pub const HAS_CRC32: u32 = 2;
pub const HASH_NONE: u32 = 0;
pub const HASH_SHA256: u32 = 1;
pub const MIN_MUTABLE_CAPACITY: u64 = 12 * 1024;
/// ROMX payload extensions defined by the 0.2.0 format registry.
///
/// Keeping this list in the core makes file dialogs and LPL validation use the
/// same registry instead of maintaining separate, easy-to-drift allowlists.
pub const SUPPORTED_FORMAT_EXTENSIONS: &[&str] = &[
    "gb", "gbc", "gba", "nes", "unf", "unif", "fds", "sfc", "smc", "nds", "3ds", "cci", "cxi",
    "app", "iso", "cso", "zso", "chd", "pbp", "cdi", "gcm", "wbfs", "rvz", "wia", "wad", "cue",
    "gdi", "m3u", "ccd", "mds", "toc", "bin", "wav", "flac", "img", "mdf", "sbi", "sub", "ecm",
    "z64", "n64", "v64", "md", "gen", "smd", "32x", "sms", "gg", "pce", "elf", "prx", "msu", "pcm",
];

pub const RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY: u64 = 512 * 1024;
pub const RECOMMENDED_UNKNOWN_CARTRIDGE_MUTABLE_CAPACITY: u64 = 1024 * 1024;
pub const RECOMMENDED_NDS_MUTABLE_CAPACITY: u64 = 16 * 1024 * 1024;
pub const RECOMMENDED_DISC_MUTABLE_CAPACITY: u64 = 2 * 1024 * 1024;
pub const RECOMMENDED_PS2_MUTABLE_CAPACITY: u64 = 32 * 1024 * 1024;
pub const RECOMMENDED_DIRECTORY_SAVE_MUTABLE_CAPACITY: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MUTABLE_ENTRY_CAPACITY: u32 = 8;
pub const DEFAULT_SAVE_OBJECT_CAPACITY: u64 = 512 * 1024;
const CARTRIDGE_MUTABLE_OVERHEAD: u64 = 256 * 1024;
const NDS_MUTABLE_OVERHEAD: u64 = 2 * 1024 * 1024;
const MUTABLE_ENTRY_SIZE: u64 = RIDX_ENTRY_SIZE as u64;
const MUTABLE_HEADER_SIZE: u64 = 4096;
const MUTABLE_ALIGNMENT: u64 = 4096;
const MUTABLE_KEY_CAPACITY: usize = 448;
const MUTABLE_BUNDLE_HEADER_SIZE: u64 = 64;
const MUTABLE_BUNDLE_ENTRY_SIZE: u64 = 64;
const MUTABLE_BUNDLE_PATH_CAPACITY: usize = 1024;
// Keep a small per-object margin so a later libromx write can change a
// bundle path (for example from a user supplied Chinese filename to the
// frontend's canonical `<rom-stem>.sav`) without consuming a new slot.
const MUTABLE_SAVE_OBJECT_HEADROOM: u64 = 64 * 1024;

/// One file stored inside a ROMX SAVE bundle.
///
/// `path` is the portable UTF-8 path exposed to the frontend.  `source` is
/// only used while packing and is never stored in the ROMX container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableSaveFile {
    pub path: String,
    pub source: PathBuf,
}

/// A named ROMX SAVE object.  The key is the user-visible save-slot label;
/// each object may contain one file (normal battery saves) or a directory
/// tree (for example a PSP savedata directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableSaveBundle {
    pub key: String,
    pub files: Vec<MutableSaveFile>,
}

/// Return the mutable directory slot count needed for a save directory.
///
/// ROMX mutable directories use eight-slot allocation units.  Two additional
/// slots are kept available for the standard CHEAT and STATS objects, so a
/// directory containing N save slots is rounded up to the next eight-slot
/// boundary without ever shrinking the default eight slots.
pub fn recommended_mutable_entry_capacity(save_count: usize) -> u32 {
    let required = save_count
        .saturating_add(2)
        .max(DEFAULT_MUTABLE_ENTRY_CAPACITY as usize);
    let rounded = required.saturating_add(7) / 8 * 8;
    u32::try_from(rounded).unwrap_or(u32::MAX - (u32::MAX % 8))
}

/// Calculate a mutable capacity from the measured local SAVE bytes.
///
/// ROMX 0.2.0's guidance reserves the detected save capacity plus a profile
/// overhead and the mutable header/directory.  The platform recommendation is
/// still a floor, so a normal NDS save set (for example seven 512 KiB saves)
/// remains within the standard 16 MiB reservation instead of multiplying that
/// reservation once per file.
pub fn recommended_mutable_capacity_for_save_bytes(
    base: u64,
    save_count: usize,
    save_bytes: u64,
) -> u64 {
    let estimated_bytes = DEFAULT_SAVE_OBJECT_CAPACITY.saturating_mul(save_count as u64);
    let detected_capacity = save_bytes.max(estimated_bytes);
    let overhead = if base == RECOMMENDED_NDS_MUTABLE_CAPACITY {
        NDS_MUTABLE_OVERHEAD
    } else {
        CARTRIDGE_MUTABLE_OVERHEAD
    };
    let entry_capacity = u64::from(recommended_mutable_entry_capacity(save_count));
    let directory_end =
        MUTABLE_HEADER_SIZE.saturating_add(entry_capacity.saturating_mul(MUTABLE_ENTRY_SIZE));
    let minimum = detected_capacity
        .saturating_add(overhead)
        .saturating_add(directory_end);
    let capacity = base.max(minimum).max(MIN_MUTABLE_CAPACITY);
    let remainder = capacity % MUTABLE_ALIGNMENT;
    if remainder == 0 {
        capacity
    } else {
        capacity
            .checked_add(MUTABLE_ALIGNMENT - remainder)
            .unwrap_or(u64::MAX - (u64::MAX % MUTABLE_ALIGNMENT))
    }
}

/// Estimate a mutable capacity when only a save-object count is available.
/// Callers that can inspect the files should prefer
/// [`recommended_mutable_capacity_for_save_bytes`].
pub fn recommended_mutable_capacity_for_save_count(base: u64, save_count: usize) -> u64 {
    recommended_mutable_capacity_for_save_bytes(base, save_count, 0)
}

/// Return the non-normative mutable capacity recommended by ROMX 0.2.0 §7.
///
/// This is the default floor used when no measured local save directory is
/// selected. A caller with local data can raise it with
/// [`recommended_mutable_capacity_for_save_bytes`].
pub fn recommended_mutable_capacity(platform: &str, extension: &str) -> u64 {
    let platform = platform.trim().to_ascii_lowercase();
    let extension = extension.trim_start_matches('.').to_ascii_lowercase();

    match platform.as_str() {
        "ps2" => return RECOMMENDED_PS2_MUTABLE_CAPACITY,
        "psp" | "gamecube" | "wii" | "3ds" => return RECOMMENDED_DIRECTORY_SAVE_MUTABLE_CAPACITY,
        "playstation" | "ps1" | "pce-cd" | "sega-cd" | "saturn" | "dreamcast" => {
            return RECOMMENDED_DISC_MUTABLE_CAPACITY
        }
        "arcade" => return RECOMMENDED_UNKNOWN_CARTRIDGE_MUTABLE_CAPACITY,
        _ => {}
    }

    match extension.as_str() {
        "nds" => RECOMMENDED_NDS_MUTABLE_CAPACITY,
        "3ds" | "cci" | "cxi" | "app" => RECOMMENDED_DIRECTORY_SAVE_MUTABLE_CAPACITY,
        "iso" | "cso" | "zso" | "chd" | "pbp" | "cdi" | "gcm" | "wbfs" | "rvz" | "wia" | "wad"
        | "cue" | "gdi" | "m3u" | "ccd" | "mds" | "toc" | "wav" | "flac" | "img" | "mdf"
        | "sbi" | "sub" | "ecm" => RECOMMENDED_DISC_MUTABLE_CAPACITY,
        "gb" | "gbc" | "gba" | "nes" | "unf" | "unif" | "fds" | "sfc" | "smc" | "z64" | "n64"
        | "v64" | "md" | "gen" | "smd" | "32x" | "sms" | "gg" | "pce" => {
            RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY
        }
        _ => match platform.as_str() {
            "gb" | "gbc" | "gba" | "nes" | "snes" | "genesis" | "md" | "sms" | "gg" | "pce"
            | "n64" => RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY,
            _ => RECOMMENDED_UNKNOWN_CARTRIDGE_MUTABLE_CAPACITY,
        },
    }
}
const MAGIC: &[u8; 4] = b"ROMX";
const RIDX_MAGIC: &[u8; 4] = b"RIDX";
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum RomxError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid JSON metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("image processing error: {0}")]
    Image(#[from] image::ImageError),
    #[error("invalid ROMX: {0}")]
    Invalid(String),
    #[error("ROMX immutable SHA-256 mismatch")]
    BodyHashMismatch,
    #[error("metadata is invalid: {0}")]
    Metadata(String),
    #[error("cover is invalid: {0}")]
    Cover(String),
    #[error("output already exists: {0}")]
    Exists(PathBuf),
    #[error("operation cancelled")]
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    pub version: u32,
    pub rom: Region,
    pub metadata: Region,
    pub cover: Region,
    pub mutable_capacity: u64,
    pub platform_id: u16,
    pub launch_format_id: u16,
    pub immutable_hash_algorithm: u32,
    pub immutable_sha256: [u8; 32],
    pub footer_crc32: u32,
    pub reserved: [u8; 44],
    // Derived compatibility flags, never serialized in the v2 footer.
    pub flags: u32,
    pub body_sha256: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RidxEntry {
    pub flags: u32,
    pub format_id: u16,
    pub path: String,
    pub data_offset: u64,
    pub data_size: u64,
    pub crc32: Option<String>,
    pub entrypoint: bool,
}

#[derive(Debug, Clone)]
pub struct RomxDocument {
    pub footer: Footer,
    pub rom: Vec<u8>,
    pub metadata: Option<Value>,
    pub cover: Option<Vec<u8>>,
    pub entries: Vec<RidxEntry>,
}

#[derive(Debug, Clone)]
pub struct RomxPreview {
    pub footer: Footer,
    pub metadata: Option<Value>,
    pub cover: Option<Vec<u8>>,
    pub entries: Vec<RidxEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverInfo {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationStatus {
    NotChecked,
    Valid,
    Invalid,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crc32Status {
    NotChecked,
    Absent,
    ValidLookup,
    Invalid,
}

#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub structure: ValidationStatus,
    pub payload_hashes: ValidationStatus,
    pub body_sha256: ValidationStatus,
    pub metadata: ValidationStatus,
    pub cover: ValidationStatus,
    pub cover_hashes: ValidationStatus,
    pub metadata_result: Option<String>,
    pub cover_result: Option<String>,
    pub metadata_crc32: Crc32Status,
    pub computed_payload_crc32: Option<String>,
    pub computed_payload_sha256: [u8; 32],
    pub computed_body_sha256: [u8; 32],
    pub computed_cover_sha256: [u8; 32],
    pub cover_info: Option<CoverInfo>,
}

#[derive(Debug, Clone)]
pub struct RomxInspection {
    pub identity: RomxIdentity,
    pub validation: ValidationReport,
    pub mutable: MutableRegionInfo,
    pub payload_size: u64,
    pub entry_count: usize,
    pub has_metadata: bool,
    pub has_cover: bool,
}
impl Default for ValidationReport {
    fn default() -> Self {
        Self {
            structure: ValidationStatus::NotChecked,
            payload_hashes: ValidationStatus::NotChecked,
            body_sha256: ValidationStatus::NotChecked,
            metadata: ValidationStatus::NotChecked,
            cover: ValidationStatus::NotChecked,
            cover_hashes: ValidationStatus::NotChecked,
            metadata_result: None,
            cover_result: None,
            metadata_crc32: Crc32Status::NotChecked,
            computed_payload_crc32: None,
            computed_payload_sha256: [0; 32],
            computed_body_sha256: [0; 32],
            computed_cover_sha256: [0; 32],
            cover_info: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PackOptions {
    pub body_sha256: bool,
    pub replace_existing: bool,
    pub crc32_override: Option<String>,
    pub cover_target: Option<(u32, u32)>,
    pub platform_id: u16,
    pub launch_format_id: u16,
    pub entry_format_id: u16,
    pub include_entry_crc32: bool,
    pub mutable_capacity: u64,
    /// Number of mutable directory entries to reserve. Zero uses the
    /// standard eight-entry profile.
    pub mutable_entry_capacity: u32,
    /// SAVE objects to initialize in the mutable region.  The source files
    /// are read while packing; an empty vector keeps the region's directory
    /// empty so a later client can allocate objects through libromx.
    pub mutable_save_bundles: Vec<MutableSaveBundle>,
    /// An existing complete mutable region to preserve byte-for-byte while
    /// editing metadata or cover. Its length must equal mutable_capacity.
    /// This keeps namespaces unknown to this crate (for example CHEAT/STATS)
    /// intact during frontend edits.
    pub mutable_region: Option<Vec<u8>>,
}
impl Default for PackOptions {
    fn default() -> Self {
        Self {
            body_sha256: false,
            replace_existing: true,
            crc32_override: None,
            cover_target: None,
            platform_id: 0,
            launch_format_id: 1,
            entry_format_id: 0,
            include_entry_crc32: true,
            mutable_capacity: 0,
            mutable_entry_capacity: 0,
            mutable_save_bundles: Vec::new(),
            mutable_region: None,
        }
    }
}

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < 256 {
        let mut value = index as u32;
        let mut bit = 0;
        while bit < 8 {
            value = if value & 1 != 0 {
                (value >> 1) ^ 0xedb8_8320
            } else {
                value >> 1
            };
            bit += 1;
        }
        table[index] = value;
        index += 1;
    }
    table
}
pub(crate) const CRC32_TABLE: [u32; 256] = build_crc32_table();

pub fn crc32(value: &[u8]) -> String {
    format!("{:08x}", crc32_u32(value))
}

/// Calculate a ROM CRC32 without loading the complete payload into memory.
///
/// This is used by the GUI's online matcher and by large-file workflows where
/// a streaming checksum avoids an additional payload-sized allocation.
pub fn crc32_path(path: &Path) -> Result<String, RomxError> {
    let mut file = fs::File::open(path)?;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut checksum = 0xffff_ffffu32;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        for byte in &buffer[..count] {
            let index = ((checksum ^ u32::from(*byte)) & 0xff) as usize;
            checksum = (checksum >> 8) ^ CRC32_TABLE[index];
        }
    }
    Ok(format!("{:08x}", checksum ^ 0xffff_ffff))
}
fn crc32_u32(value: &[u8]) -> u32 {
    let mut crc = 0xffff_ffff;
    for byte in value {
        crc = (crc >> 8) ^ CRC32_TABLE[((crc ^ u32::from(*byte)) & 0xff) as usize];
    }
    crc ^ 0xffff_ffff
}
pub fn payload_sha256(value: &[u8]) -> [u8; 32] {
    Sha256::digest(value).into()
}
pub fn normalize_crc32(value: &str) -> Result<String, RomxError> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RomxError::Invalid(
            "CRC32 must be exactly eight hexadecimal characters".into(),
        ));
    }
    Ok(value.to_ascii_lowercase())
}

pub fn normalize_cover_bytes(
    value: &[u8],
    target: Option<(u32, u32)>,
) -> Result<Vec<u8>, RomxError> {
    if value.is_empty() {
        return Err(RomxError::Cover("cover must not be empty".into()));
    }
    if target.is_none() && value.starts_with(PNG_SIGNATURE) {
        validate_png_bytes(value)?;
        return Ok(value.to_vec());
    }
    let decoded = ImageReader::new(Cursor::new(value))
        .with_guessed_format()?
        .decode()?;
    let decoded = target
        .map(|(width, height)| {
            decoded.resize_exact(width, height, image::imageops::FilterType::Lanczos3)
        })
        .unwrap_or(decoded);
    let mut output = Cursor::new(Vec::new());
    decoded.write_to(&mut output, image::ImageFormat::Png)?;
    validate_png_bytes(output.get_ref())?;
    Ok(output.into_inner())
}
pub fn normalize_cover_path(path: &Path, target: Option<(u32, u32)>) -> Result<Vec<u8>, RomxError> {
    normalize_cover_bytes(&fs::read(path)?, target)
}

fn png_error(message: impl Into<String>) -> RomxError {
    RomxError::Cover(message.into())
}
fn be_u32(value: &[u8]) -> u32 {
    u32::from_be_bytes(value.try_into().expect("four byte PNG field"))
}
fn validate_png_ihdr(data: &[u8]) -> Option<CoverInfo> {
    if data.len() != 13 {
        return None;
    }
    let width = be_u32(&data[..4]);
    let height = be_u32(&data[4..8]);
    let depth = data[8];
    let color = data[9];
    if width == 0
        || height == 0
        || width > DEFAULT_MAX_COVER_DIMENSION
        || height > DEFAULT_MAX_COVER_DIMENSION
        || data[10] != 0
        || data[11] != 0
        || data[12] > 1
    {
        return None;
    }
    let valid = match color {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        2 => matches!(depth, 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8),
        4 | 6 => matches!(depth, 8 | 16),
        _ => false,
    };
    valid.then_some(CoverInfo { width, height })
}
pub fn validate_png_bytes(value: &[u8]) -> Result<CoverInfo, RomxError> {
    if value.len() > DEFAULT_MAX_COVER_SIZE as usize {
        return Err(png_error("cover exceeds the 32 MiB limit"));
    }
    if !value.starts_with(PNG_SIGNATURE) {
        return Err(png_error("cover has an invalid PNG signature"));
    }
    let mut position = 8;
    let mut info = None;
    let mut color = 0;
    let mut saw_idat = false;
    let mut ended_idat = false;
    let mut saw_plte = false;
    let mut saw_iend = false;
    while position < value.len() {
        if value.len() - position < 12 {
            return Err(png_error("PNG chunk is truncated"));
        }
        let length = be_u32(&value[position..position + 4]) as usize;
        if length > value.len() - position - 12 {
            return Err(png_error("PNG chunk exceeds cover bounds"));
        }
        let kind = &value[position + 4..position + 8];
        let data_start = position + 8;
        let data_end = data_start + length;
        if !kind.iter().all(|byte| byte.is_ascii_alphabetic()) || kind[2].is_ascii_lowercase() {
            return Err(png_error("PNG chunk type is invalid"));
        }
        if crc32_u32(&value[position + 4..data_end]) != be_u32(&value[data_end..data_end + 4]) {
            return Err(png_error("PNG chunk CRC mismatch"));
        }
        if info.is_none() {
            if kind != b"IHDR" {
                return Err(png_error("PNG IHDR must be first"));
            }
            let header = validate_png_ihdr(&value[data_start..data_end])
                .ok_or_else(|| png_error("PNG IHDR fields are invalid"))?;
            color = value[data_start + 9];
            info = Some(header);
        } else if kind == b"IHDR" {
            return Err(png_error("PNG contains multiple IHDR chunks"));
        }
        match kind {
            b"PLTE" => {
                if saw_plte
                    || saw_idat
                    || matches!(color, 0 | 4)
                    || length == 0
                    || !length.is_multiple_of(3)
                    || length > 768
                {
                    return Err(png_error("PNG PLTE chunk is invalid"));
                }
                saw_plte = true;
            }
            b"IHDR" => {}
            b"IDAT" => {
                if ended_idat {
                    return Err(png_error("PNG IDAT chunks must be consecutive"));
                }
                saw_idat = true;
            }
            b"IEND" => {
                if length != 0 || !saw_idat || (color == 3 && !saw_plte) {
                    return Err(png_error("PNG IEND or required chunks are invalid"));
                }
                saw_iend = true;
            }
            _ if kind[0].is_ascii_uppercase() => {
                return Err(png_error("PNG contains an unknown critical chunk"))
            }
            _ => {}
        }
        if saw_idat && kind != b"IDAT" && kind != b"IEND" {
            ended_idat = true;
        }
        position = data_end + 4;
        if saw_iend {
            break;
        }
    }
    if !saw_iend || position != value.len() || !saw_idat {
        return Err(png_error("PNG must end with IEND and contain IDAT"));
    }
    info.ok_or_else(|| png_error("PNG has no IHDR"))
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, RomxError> {
    bytes
        .get(offset..offset + 2)
        .and_then(|v| v.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| RomxError::Invalid("truncated ROMX field".into()))
}
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, RomxError> {
    bytes
        .get(offset..offset + 4)
        .and_then(|v| v.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| RomxError::Invalid("truncated ROMX field".into()))
}
fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, RomxError> {
    bytes
        .get(offset..offset + 8)
        .and_then(|v| v.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| RomxError::Invalid("truncated ROMX field".into()))
}
fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}
fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

impl Footer {
    pub fn encode(&self) -> [u8; FOOTER_SIZE] {
        let mut output = [0u8; FOOTER_SIZE];
        output[..4].copy_from_slice(MAGIC);
        put_u32(&mut output, 4, VERSION);
        put_u64(&mut output, 8, self.rom.size);
        put_u64(&mut output, 0x10, self.metadata.size);
        put_u64(&mut output, 0x18, self.cover.size);
        put_u64(&mut output, 0x20, self.mutable_capacity);
        put_u16(&mut output, 0x28, self.platform_id);
        put_u16(&mut output, 0x2a, self.launch_format_id);
        put_u32(&mut output, 0x2c, self.immutable_hash_algorithm);
        output[0x30..0x50].copy_from_slice(&self.immutable_sha256);
        output[0x54..].copy_from_slice(&self.reserved);
        put_u32(&mut output, 0x50, 0);
        let checksum = crc32_u32(&output);
        put_u32(&mut output, 0x50, checksum);
        output
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, RomxError> {
        if bytes.len() != FOOTER_SIZE || &bytes[..4] != MAGIC {
            return Err(RomxError::Invalid("invalid ROMX footer".into()));
        }
        if read_u32(bytes, 4)? != VERSION {
            return Err(RomxError::Invalid(
                "unsupported ROMX footer wire version".into(),
            ));
        }
        let stored = read_u32(bytes, 0x50)?;
        let mut check = [0u8; FOOTER_SIZE];
        check.copy_from_slice(bytes);
        put_u32(&mut check, 0x50, 0);
        if crc32_u32(&check) != stored {
            return Err(RomxError::Invalid("footer CRC32 mismatch".into()));
        }
        let reserved: [u8; 44] = bytes[0x54..].try_into().unwrap();
        if reserved.iter().any(|v| *v != 0) {
            return Err(RomxError::Invalid(
                "footer reserved bytes are non-zero".into(),
            ));
        }
        let hash = read_u32(bytes, 0x2c)?;
        let immutable: [u8; 32] = bytes[0x30..0x50].try_into().unwrap();
        if hash == HASH_NONE && immutable != [0; 32] || !matches!(hash, HASH_NONE | HASH_SHA256) {
            return Err(RomxError::Invalid("invalid immutable hash fields".into()));
        }
        let payload = read_u64(bytes, 8)?;
        let metadata = read_u64(bytes, 0x10)?;
        let cover = read_u64(bytes, 0x18)?;
        let flags = (if metadata > 0 { FLAG_METADATA } else { 0 })
            | (if cover > 0 { FLAG_COVER } else { 0 })
            | (if hash == HASH_SHA256 {
                FLAG_BODY_SHA256
            } else {
                0
            });
        Ok(Self {
            version: VERSION,
            rom: Region {
                offset: 0,
                size: payload,
            },
            metadata: Region {
                offset: 0,
                size: metadata,
            },
            cover: Region {
                offset: 0,
                size: cover,
            },
            mutable_capacity: read_u64(bytes, 0x20)?,
            platform_id: read_u16(bytes, 0x28)?,
            launch_format_id: read_u16(bytes, 0x2a)?,
            immutable_hash_algorithm: hash,
            immutable_sha256: immutable,
            footer_crc32: stored,
            reserved,
            flags,
            body_sha256: immutable,
        })
    }
}

struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("strict JSON")
            }
            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue(Value::Null))
            }
            fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue(Value::Bool(v)))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue(Value::Number(v.into())))
            }
            fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue(Value::Number(v.into())))
            }
            fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Number::from_f64(v)
                    .map(|n| StrictValue(Value::Number(n)))
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue(Value::String(v.to_owned())))
            }
            fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue(Value::String(v)))
            }
            fn visit_seq<A>(self, mut a: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut v = Vec::new();
                while let Some(x) = a.next_element::<StrictValue>()? {
                    v.push(x.0);
                }
                Ok(StrictValue(Value::Array(v)))
            }
            fn visit_map<A>(self, mut a: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut v = Map::new();
                while let Some(k) = a.next_key::<String>()? {
                    if v.contains_key(&k) {
                        return Err(de::Error::custom(format!("duplicate JSON object key: {k}")));
                    }
                    v.insert(k, a.next_value::<StrictValue>()?.0);
                }
                Ok(StrictValue(Value::Object(v)))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}
fn parse_json_strict(bytes: &[u8]) -> Result<Value, RomxError> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(RomxError::Metadata(
            "metadata must not contain a UTF-8 BOM".into(),
        ));
    }
    std::str::from_utf8(bytes).map_err(|_| RomxError::Metadata("metadata is not UTF-8".into()))?;
    let mut d = serde_json::Deserializer::from_slice(bytes);
    let value = StrictValue::deserialize(&mut d)
        .map_err(|e| RomxError::Metadata(format!("metadata JSON is invalid: {e}")))?
        .0;
    d.end()
        .map_err(|e| RomxError::Metadata(format!("metadata JSON is invalid: {e}")))?;
    Ok(value)
}
fn string_len(v: &str) -> usize {
    v.chars().count()
}
/// Classify a Game Boy payload using its CGB flag, while preserving an
/// explicit `gb`/`gbc` format selection for headers that do not identify it.
pub fn classify_gb_payload(
    rom: &[u8],
    payload_format: Option<&str>,
) -> Result<&'static str, RomxError> {
    let explicit = || match payload_format {
        Some("gb") => Ok("gb"),
        Some("gbc") => Ok("gbc"),
        _ => Err(RomxError::Invalid(
            "GB ROM requires payload_format gb or gbc".into(),
        )),
    };
    if rom.len() <= 0x143 {
        return explicit();
    }
    match rom[0x143] {
        0xc0 => Ok("gbc"),
        0x80 => explicit(),
        _ => explicit(),
    }
}

pub fn format_id_for_extension(extension: &str) -> u16 {
    match extension
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .as_str()
    {
        "gb" => 1,
        "gbc" => 2,
        "gba" => 3,
        "nes" => 4,
        "unf" => 5,
        "unif" => 6,
        "fds" => 7,
        "sfc" => 8,
        "smc" => 9,
        "nds" => 0x0a,
        "3ds" => 0x0b,
        "cci" => 0x0c,
        "cxi" => 0x0d,
        "app" => 0x0e,
        "iso" => 0x10,
        "cso" => 0x11,
        "zso" => 0x12,
        "chd" => 0x13,
        "pbp" => 0x14,
        "cdi" => 0x15,
        "gcm" => 0x16,
        "wbfs" => 0x17,
        "rvz" => 0x18,
        "wia" => 0x19,
        "wad" => 0x1a,
        "cue" => 0x20,
        "gdi" => 0x21,
        "m3u" => 0x22,
        "ccd" => 0x23,
        "mds" => 0x24,
        "toc" => 0x25,
        "bin" => 0x30,
        "wav" => 0x31,
        "flac" => 0x32,
        "img" => 0x33,
        "mdf" => 0x34,
        "sbi" => 0x40,
        "sub" => 0x41,
        "ecm" => 0x42,
        "z64" => 0x50,
        "n64" => 0x51,
        "v64" => 0x52,
        "md" => 0x60,
        "gen" => 0x61,
        "smd" => 0x62,
        "32x" => 0x63,
        "sms" => 0x64,
        "gg" => 0x65,
        "pce" => 0x66,
        "elf" => 0x70,
        "prx" => 0x71,
        "msu" => 0x80,
        "pcm" => 0x81,
        _ => 0,
    }
}
pub fn format_extension(format_id: u16) -> Option<&'static str> {
    Some(match format_id {
        1 => "gb",
        2 => "gbc",
        3 => "gba",
        4 => "nes",
        5 => "unf",
        6 => "unif",
        7 => "fds",
        8 => "sfc",
        9 => "smc",
        0x0a => "nds",
        0x0b => "3ds",
        0x0c => "cci",
        0x0d => "cxi",
        0x0e => "app",
        0x10 => "iso",
        0x11 => "cso",
        0x12 => "zso",
        0x13 => "chd",
        0x14 => "pbp",
        0x15 => "cdi",
        0x16 => "gcm",
        0x17 => "wbfs",
        0x18 => "rvz",
        0x19 => "wia",
        0x1a => "wad",
        0x20 => "cue",
        0x21 => "gdi",
        0x22 => "m3u",
        0x23 => "ccd",
        0x24 => "mds",
        0x25 => "toc",
        0x30 => "bin",
        0x31 => "wav",
        0x32 => "flac",
        0x33 => "img",
        0x34 => "mdf",
        0x40 => "sbi",
        0x41 => "sub",
        0x42 => "ecm",
        0x50 => "z64",
        0x51 => "n64",
        0x52 => "v64",
        0x60 => "md",
        0x61 => "gen",
        0x62 => "smd",
        0x63 => "32x",
        0x64 => "sms",
        0x65 => "gg",
        0x66 => "pce",
        0x70 => "elf",
        0x71 => "prx",
        0x80 => "msu",
        0x81 => "pcm",
        _ => return None,
    })
}
pub fn platform_id_for_name(name: &str) -> u16 {
    match name {
        "gb" => 1,
        "gbc" => 2,
        "gba" => 3,
        "nes" => 4,
        "snes" => 5,
        "n64" => 6,
        "nds" => 7,
        "3ds" => 8,
        "sms" => 0x10,
        "gg" => 0x11,
        "genesis" | "md" => 0x12,
        "pce" => 0x20,
        "ps1" | "playstation" => 0x30,
        "ps2" => 0x31,
        "psp" => 0x32,
        "gamecube" => 0x40,
        "wii" => 0x41,
        "arcade" => 0x50,
        "scummvm" => 0x60,
        "dos" => 0x61,
        "amiga" => 0x62,
        _ => 0,
    }
}
pub fn platform_name_from_id(id: u16) -> Option<&'static str> {
    Some(match id {
        1 => "gb",
        2 => "gbc",
        3 => "gba",
        4 => "nes",
        5 => "snes",
        6 => "n64",
        7 => "nds",
        8 => "3ds",
        0x10 => "sms",
        0x11 => "gg",
        0x12 => "genesis",
        0x20 => "pce",
        0x30 => "playstation",
        0x31 => "ps2",
        0x32 => "psp",
        0x40 => "gamecube",
        0x41 => "wii",
        0x50 => "arcade",
        0x60 => "scummvm",
        0x61 => "dos",
        0x62 => "amiga",
        _ => return None,
    })
}
pub fn launch_format_id_for_extension(extension: &str) -> u16 {
    match extension
        .trim_start_matches('.')
        .to_ascii_lowercase()
        .as_str()
    {
        "cue" => 2,
        "gdi" => 3,
        "m3u" => 4,
        "ccd" => 5,
        "mds" => 6,
        "toc" => 7,
        _ => 1,
    }
}
fn validate_string_array(v: &Value, max: usize, limit: usize) -> bool {
    let Some(a) = v.as_array() else { return false };
    if a.len() > max {
        return false;
    };
    let mut seen = Vec::<&str>::new();
    for x in a {
        let Some(s) = x.as_str() else { return false };
        if string_len(s) > limit || seen.contains(&s) {
            return false;
        }
        seen.push(s);
    }
    true
}
fn valid_date(v: &str) -> bool {
    matches!(v.len(), 4 | 7 | 10)
        && v.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        })
}
fn valid_cover_descriptor(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    object.iter().all(|(key, value)| match key.as_str() {
        "mime_type" => value.as_str() == Some("image/png"),
        "width" | "height" => value
            .as_u64()
            .is_some_and(|n| (1..=DEFAULT_MAX_COVER_DIMENSION as u64).contains(&n)),
        _ => false,
    })
}

pub(crate) fn validate_metadata_template(metadata: &Value) -> Result<(), RomxError> {
    let Some(object) = metadata.as_object() else {
        return Err(RomxError::Metadata(
            "metadata top level must be an object".into(),
        ));
    };
    let allowed = [
        "schema_version",
        "name",
        "serial",
        "developer",
        "publisher",
        "origin",
        "franchise",
        "release_date",
        "genre",
        "region",
        "language",
        "users",
        "coop",
        "rumble",
        "analog",
        "enhancement_hw",
        "category",
        "media",
        "description",
        "crc32",
        "origin_crc32",
        "dump_status",
        "cover",
    ];
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(RomxError::Metadata(
            "metadata contains an unknown property".into(),
        ));
    }
    if object.get("schema_version").and_then(Value::as_str) != Some(SPEC_VERSION) {
        return Err(RomxError::Metadata(
            "metadata schema_version must be 0.2.0".into(),
        ));
    }
    if !object.contains_key("name") {
        return Err(RomxError::Metadata("metadata name is required".into()));
    }
    let string_limits = [
        ("name", 1usize, 512usize),
        ("serial", 0, 128),
        ("developer", 0, 256),
        ("publisher", 0, 256),
        ("origin", 0, 128),
        ("franchise", 0, 256),
        ("language", 0, 256),
        ("enhancement_hw", 0, 256),
        ("category", 0, 128),
        ("media", 0, 64),
        ("description", 0, 32768),
    ];
    for (key, minimum, maximum) in string_limits {
        if let Some(value) = object.get(key) {
            let Some(text) = value.as_str() else {
                return Err(RomxError::Metadata(format!(
                    "metadata {key} must be a string"
                )));
            };
            let length = string_len(text);
            if length < minimum || length > maximum {
                return Err(RomxError::Metadata(format!(
                    "metadata {key} violates its string length"
                )));
            }
        }
    }
    if let Some(value) = object.get("release_date") {
        let Some(text) = value.as_str() else {
            return Err(RomxError::Metadata(
                "metadata release_date must be a string".into(),
            ));
        };
        if !valid_date(text) {
            return Err(RomxError::Metadata(
                "metadata release_date is invalid".into(),
            ));
        }
    }
    for (key, maximum, item_maximum) in [("genre", 32usize, 64usize), ("region", 32, 32)] {
        if let Some(value) = object.get(key) {
            if !validate_string_array(value, maximum, item_maximum) {
                return Err(RomxError::Metadata(format!(
                    "metadata {key} violates its array schema"
                )));
            }
        }
    }
    for key in ["crc32", "origin_crc32"] {
        if let Some(value) = object.get(key) {
            let Some(text) = value.as_str() else {
                return Err(RomxError::Metadata(format!(
                    "metadata {key} must be a string"
                )));
            };
            if text.len() != 8
                || !text
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
            {
                return Err(RomxError::Metadata(format!(
                    "metadata {key} must be eight lower-case hex digits"
                )));
            }
        }
    }
    if let Some(value) = object.get("users") {
        if value.as_u64().is_none_or(|n| !(1..=255).contains(&n)) {
            return Err(RomxError::Metadata(
                "metadata users must be an integer from 1 to 255".into(),
            ));
        }
    }
    for key in ["coop", "rumble", "analog"] {
        if object.get(key).is_some_and(|value| !value.is_boolean()) {
            return Err(RomxError::Metadata(format!(
                "metadata {key} must be boolean"
            )));
        }
    }
    if let Some(value) = object.get("dump_status") {
        if ![
            "unknown",
            "good",
            "bad",
            "overdump",
            "hack",
            "translation",
            "homebrew",
        ]
        .contains(&value.as_str().unwrap_or_default())
        {
            return Err(RomxError::Metadata(
                "metadata dump_status is invalid".into(),
            ));
        }
    }
    if let Some(value) = object.get("cover") {
        if !valid_cover_descriptor(value) {
            return Err(RomxError::Metadata(
                "metadata cover descriptor is invalid".into(),
            ));
        }
    }
    Ok(())
}

pub fn validate_metadata(metadata: &Value) -> Result<(), RomxError> {
    validate_metadata_template(metadata)
}

fn metadata_bytes(
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    crc: &str,
    override_crc: Option<&str>,
) -> Result<Option<Vec<u8>>, RomxError> {
    let Some(metadata) = metadata else {
        return Ok(None);
    };
    validate_metadata_template(metadata)?;
    let mut object = metadata
        .as_object()
        .cloned()
        .ok_or_else(|| RomxError::Metadata("metadata top level must be an object".into()))?;
    object.insert("schema_version".into(), Value::String(SPEC_VERSION.into()));
    object.insert(
        "crc32".into(),
        Value::String(normalize_crc32(override_crc.unwrap_or(crc))?),
    );
    if let Some(cover) = cover {
        let info = validate_png_bytes(cover)?;
        object.insert(
            "cover".into(),
            serde_json::json!({"mime_type":"image/png", "width":info.width, "height":info.height}),
        );
    }
    let value = Value::Object(object);
    validate_metadata_template(&value)?;
    Ok(Some(serde_json::to_vec(&value)?))
}

fn validate_virtual_path(path: &str) -> Result<Vec<u8>, RomxError> {
    let bytes = path.as_bytes();
    if bytes.is_empty()
        || bytes.len() > RIDX_PATH_CAPACITY
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(RomxError::Invalid(format!(
            "invalid RIDX virtual path: {path}"
        )));
    }
    Ok(bytes.to_vec())
}

fn format_id_from_path(path: &str) -> u16 {
    match Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "gb" => 1,
        "gbc" => 2,
        "gba" => 3,
        "nes" => 4,
        "unf" => 5,
        "unif" => 6,
        "fds" => 7,
        "sfc" => 8,
        "smc" => 9,
        "nds" => 0x0a,
        "3ds" => 0x0b,
        "cci" => 0x0c,
        "cxi" => 0x0d,
        "app" => 0x0e,
        "iso" => 0x10,
        "cso" => 0x11,
        "zso" => 0x12,
        "chd" => 0x13,
        "pbp" => 0x14,
        "cdi" => 0x15,
        "gcm" => 0x16,
        "wbfs" => 0x17,
        "rvz" => 0x18,
        "wia" => 0x19,
        "wad" => 0x1a,
        "cue" => 0x20,
        "gdi" => 0x21,
        "m3u" => 0x22,
        "ccd" => 0x23,
        "mds" => 0x24,
        "toc" => 0x25,
        "bin" => 0x30,
        "wav" => 0x31,
        "flac" => 0x32,
        "img" => 0x33,
        "mdf" => 0x34,
        "sbi" => 0x40,
        "sub" => 0x41,
        "ecm" => 0x42,
        "z64" => 0x50,
        "n64" => 0x51,
        "v64" => 0x52,
        "md" => 0x60,
        "gen" => 0x61,
        "smd" => 0x62,
        "32x" => 0x63,
        "sms" => 0x64,
        "gg" => 0x65,
        "pce" => 0x66,
        "elf" => 0x70,
        "prx" => 0x71,
        "msu" => 0x80,
        "pcm" => 0x81,
        _ => 0,
    }
}

fn make_ridx(
    path: &str,
    format: u16,
    size: u64,
    crc: u32,
    include_crc: bool,
) -> Result<Vec<u8>, RomxError> {
    let path = validate_virtual_path(path)?;
    if format == 0 {
        return Err(RomxError::Invalid(
            "entrypoint format_id must be registered".into(),
        ));
    }
    let mut index = vec![0u8; RIDX_HEADER_SIZE + RIDX_ENTRY_SIZE];
    index[..4].copy_from_slice(RIDX_MAGIC);
    put_u16(&mut index, 4, RIDX_VERSION);
    put_u16(&mut index, 6, RIDX_HEADER_SIZE as u16);
    put_u32(&mut index, 8, 1);
    put_u32(&mut index, 12, RIDX_ENTRY_SIZE as u32);
    let base = RIDX_HEADER_SIZE;
    put_u32(
        &mut index,
        base,
        ENTRYPOINT | if include_crc { HAS_CRC32 } else { 0 },
    );
    put_u16(&mut index, base + 4, format);
    put_u16(&mut index, base + 6, path.len() as u16);
    put_u64(&mut index, base + 8, 0);
    put_u64(&mut index, base + 16, size);
    put_u32(&mut index, base + 24, if include_crc { crc } else { 0 });
    index[base + 0x20..base + 0x20 + path.len()].copy_from_slice(&path);
    put_u32(&mut index, 0x14, 0);
    let checksum = crc32_u32(&index);
    put_u32(&mut index, 0x14, checksum);
    Ok(index)
}

fn align_mutable_bundle(value: u64) -> Result<u64, RomxError> {
    value
        .checked_add(63)
        .map(|value| value & !63)
        .ok_or_else(|| RomxError::Invalid("mutable bundle alignment overflow".into()))
}

fn validate_mutable_save_key(key: &str) -> Result<(), RomxError> {
    if key.is_empty() {
        return Err(RomxError::Invalid(
            "mutable SAVE key must not be empty".into(),
        ));
    }
    if key.len() > MUTABLE_KEY_CAPACITY {
        return Err(RomxError::Invalid(
            "mutable SAVE key exceeds the 448-byte limit".into(),
        ));
    }
    if key == "."
        || key == ".."
        || key.contains('/')
        || key.contains('\\')
        || key.as_bytes().contains(&0)
    {
        return Err(RomxError::Invalid(
            "mutable SAVE key contains a path separator or dot component".into(),
        ));
    }
    Ok(())
}

fn validate_mutable_bundle_path(path: &str) -> Result<(), RomxError> {
    if path.is_empty()
        || path.len() > MUTABLE_BUNDLE_PATH_CAPACITY
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.as_bytes().contains(&0)
    {
        return Err(RomxError::Invalid(
            "mutable SAVE bundle path is not portable".into(),
        ));
    }
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(RomxError::Invalid(
                "mutable SAVE bundle path contains an unsafe component".into(),
            ));
        }
    }
    Ok(())
}

struct PackedMutableSaveBundle {
    key: String,
    bytes: Vec<u8>,
    crc32: u32,
}

fn build_mutable_save_bundle(
    bundle: &MutableSaveBundle,
) -> Result<PackedMutableSaveBundle, RomxError> {
    validate_mutable_save_key(&bundle.key)?;
    if bundle.files.len() > u32::MAX as usize {
        return Err(RomxError::Invalid(
            "mutable SAVE bundle has too many files".into(),
        ));
    }

    struct Input {
        path: String,
        bytes: Vec<u8>,
        crc32: u32,
        data_offset: u64,
    }
    let mut inputs = Vec::with_capacity(bundle.files.len());
    for file in &bundle.files {
        validate_mutable_bundle_path(&file.path)?;
        let metadata = fs::symlink_metadata(&file.source)?;
        if !metadata.is_file() {
            return Err(RomxError::Invalid(format!(
                "mutable SAVE source is not a regular file: {}",
                file.source.display()
            )));
        }
        let bytes = fs::read(&file.source)?;
        inputs.push(Input {
            path: file.path.clone(),
            crc32: crc32_u32(&bytes),
            bytes,
            data_offset: 0,
        });
    }
    inputs.sort_by(|left, right| left.path.as_bytes().cmp(right.path.as_bytes()));
    for pair in inputs.windows(2) {
        if pair[0].path.eq_ignore_ascii_case(&pair[1].path) {
            return Err(RomxError::Invalid(
                "mutable SAVE bundle paths collide after ASCII case folding".into(),
            ));
        }
    }

    let entry_count = inputs.len() as u64;
    let path_table_offset = MUTABLE_BUNDLE_HEADER_SIZE
        .checked_add(
            entry_count
                .checked_mul(MUTABLE_BUNDLE_ENTRY_SIZE)
                .ok_or_else(|| RomxError::Invalid("mutable bundle directory overflow".into()))?,
        )
        .ok_or_else(|| RomxError::Invalid("mutable bundle path table overflow".into()))?;
    let mut path_cursor = path_table_offset;
    for input in &inputs {
        path_cursor = path_cursor
            .checked_add(input.path.len() as u64)
            .ok_or_else(|| RomxError::Invalid("mutable bundle path table overflow".into()))?;
    }
    let data_offset = align_mutable_bundle(path_cursor)?;
    let mut data_cursor = data_offset;
    for input in &mut inputs {
        input.data_offset = data_cursor;
        data_cursor = data_cursor
            .checked_add(input.bytes.len() as u64)
            .ok_or_else(|| RomxError::Invalid("mutable bundle data overflow".into()))?;
        data_cursor = align_mutable_bundle(data_cursor)?;
    }
    let bundle_size = data_cursor;
    let output_size = usize::try_from(bundle_size)
        .map_err(|_| RomxError::Invalid("mutable SAVE bundle is too large".into()))?;
    let mut output = vec![0u8; output_size];

    output[..4].copy_from_slice(b"RMBL");
    put_u16(&mut output, 4, 1);
    put_u16(&mut output, 6, MUTABLE_BUNDLE_HEADER_SIZE as u16);
    put_u16(&mut output, 8, 1); // ROMX_MUTABLE_NAMESPACE_SAVE
    put_u16(&mut output, 0x0a, 0);
    put_u32(&mut output, 0x0c, MUTABLE_BUNDLE_HEADER_SIZE as u32);
    put_u32(
        &mut output,
        0x10,
        u32::try_from(entry_count)
            .map_err(|_| RomxError::Invalid("mutable SAVE bundle entry count overflow".into()))?,
    );
    put_u32(&mut output, 0x14, 0);
    put_u64(&mut output, 0x18, MUTABLE_BUNDLE_HEADER_SIZE);
    put_u64(&mut output, 0x20, path_table_offset);
    put_u64(&mut output, 0x28, data_offset);
    put_u64(&mut output, 0x30, bundle_size);
    put_u32(&mut output, 0x38, 0);
    let header_crc = crc32_u32(&output[..MUTABLE_BUNDLE_HEADER_SIZE as usize]);
    put_u32(&mut output, 0x38, header_crc);

    let mut path_cursor = path_table_offset;
    for (index, input) in inputs.iter().enumerate() {
        let directory_offset = MUTABLE_BUNDLE_HEADER_SIZE
            .checked_add((index as u64) * MUTABLE_BUNDLE_ENTRY_SIZE)
            .ok_or_else(|| RomxError::Invalid("mutable bundle directory overflow".into()))?;
        let directory_offset = usize::try_from(directory_offset)
            .map_err(|_| RomxError::Invalid("mutable bundle directory is too large".into()))?;
        put_u64(&mut output, directory_offset, path_cursor);
        put_u32(
            &mut output,
            directory_offset + 8,
            u32::try_from(input.path.len())
                .map_err(|_| RomxError::Invalid("mutable SAVE path is too long".into()))?,
        );
        put_u64(&mut output, directory_offset + 0x10, input.data_offset);
        put_u64(
            &mut output,
            directory_offset + 0x18,
            input.bytes.len() as u64,
        );
        put_u32(&mut output, directory_offset + 0x20, input.crc32);
        let path_start = usize::try_from(path_cursor)
            .map_err(|_| RomxError::Invalid("mutable bundle path table is too large".into()))?;
        output[path_start..path_start + input.path.len()].copy_from_slice(input.path.as_bytes());
        path_cursor += input.path.len() as u64;
    }
    for input in &inputs {
        let data_start = usize::try_from(input.data_offset)
            .map_err(|_| RomxError::Invalid("mutable bundle data is too large".into()))?;
        output[data_start..data_start + input.bytes.len()].copy_from_slice(&input.bytes);
    }

    Ok(PackedMutableSaveBundle {
        key: bundle.key.clone(),
        crc32: crc32_u32(&output),
        bytes: output,
    })
}

fn build_mutable_entry(
    key: &str,
    data_offset: u64,
    data_capacity: u64,
    data_size: u64,
    data_crc32: u32,
) -> Result<Vec<u8>, RomxError> {
    validate_mutable_save_key(key)?;
    let mut entry = vec![0u8; RIDX_ENTRY_SIZE];
    entry[..4].copy_from_slice(b"MENT");
    put_u16(&mut entry, 4, 1); // ACTIVE
    put_u16(&mut entry, 6, 1); // SAVE namespace
    put_u32(&mut entry, 8, 0);
    put_u32(&mut entry, 0x0c, key.len() as u32);
    put_u64(&mut entry, 0x10, data_offset);
    put_u64(&mut entry, 0x18, data_capacity);
    put_u64(&mut entry, 0x20, data_size);
    put_u64(&mut entry, 0x28, 1); // first generation
    put_u64(&mut entry, 0x30, 0); // modified time is optional
    put_u32(&mut entry, 0x38, data_crc32);
    entry[0x40..0x40 + key.len()].copy_from_slice(key.as_bytes());
    put_u32(&mut entry, 0x3c, 0);
    let crc = crc32_u32(&entry);
    put_u32(&mut entry, 0x3c, crc);
    Ok(entry)
}

fn make_empty_mutable(
    capacity: u64,
    requested_entry_capacity: u32,
    save_bundles: &[MutableSaveBundle],
) -> Result<Vec<u8>, RomxError> {
    if capacity == 0 {
        if !save_bundles.is_empty() {
            return Err(RomxError::Invalid(
                "SAVE bundles require a reserved mutable region".into(),
            ));
        }
        return Ok(Vec::new());
    }
    if !capacity.is_multiple_of(4096) || capacity < MIN_MUTABLE_CAPACITY {
        return Err(RomxError::Invalid(
            "mutable capacity must be a 4096-byte multiple and at least 12288".into(),
        ));
    }
    const HEADER: usize = 4096;
    const ENTRY: usize = RIDX_ENTRY_SIZE;
    let count = if requested_entry_capacity == 0 {
        DEFAULT_MUTABLE_ENTRY_CAPACITY
    } else {
        requested_entry_capacity
    } as usize;
    if count < DEFAULT_MUTABLE_ENTRY_CAPACITY as usize || !count.is_multiple_of(8) {
        return Err(RomxError::Invalid(
            "mutable entry capacity must be a multiple of 8 and at least 8".into(),
        ));
    }
    let directory = ENTRY
        .checked_mul(count)
        .ok_or_else(|| RomxError::Invalid("mutable directory size overflow".into()))?;
    let data_offset = HEADER + directory;
    if capacity < data_offset as u64 + MUTABLE_ALIGNMENT {
        return Err(RomxError::Invalid(
            "mutable capacity does not leave room for mutable data".into(),
        ));
    }
    let mut header = vec![0u8; HEADER];
    header[..4].copy_from_slice(b"RMUT");
    put_u16(&mut header, 4, 1);
    put_u16(&mut header, 6, HEADER as u16);
    put_u32(&mut header, 8, ENTRY as u32);
    put_u32(&mut header, 12, count as u32);
    put_u64(&mut header, 16, HEADER as u64);
    put_u64(&mut header, 24, directory as u64);
    put_u64(&mut header, 32, data_offset as u64);
    put_u64(&mut header, 40, capacity - data_offset as u64);
    put_u32(&mut header, 0x34, 0);
    let checksum = crc32_u32(&header);
    put_u32(&mut header, 0x34, checksum);
    if save_bundles.len() > count {
        return Err(RomxError::Invalid(
            "mutable directory has fewer slots than SAVE bundles".into(),
        ));
    }
    let packed_bundles = save_bundles
        .iter()
        .map(build_mutable_save_bundle)
        .collect::<Result<Vec<_>, _>>()?;
    // Keep the uniqueness check independent of input ordering so GUI-created
    // labels remain deterministic and safe.
    for (index, left) in packed_bundles.iter().enumerate() {
        if packed_bundles[index + 1..]
            .iter()
            .any(|right| left.key.eq_ignore_ascii_case(&right.key))
        {
            return Err(RomxError::Invalid(
                "mutable SAVE keys collide after ASCII case folding".into(),
            ));
        }
    }
    let output_size = usize::try_from(capacity)
        .map_err(|_| RomxError::Invalid("mutable capacity is too large".into()))?;
    let mut output = vec![0u8; output_size];
    output[..HEADER].copy_from_slice(&header);
    let mut data_cursor = data_offset as u64;
    for (slot_index, bundle) in packed_bundles.iter().enumerate() {
        let data_capacity = align_mutable_bundle(
            (bundle.bytes.len() as u64)
                .checked_add(MUTABLE_SAVE_OBJECT_HEADROOM)
                .ok_or_else(|| RomxError::Invalid("mutable SAVE object size overflow".into()))?,
        )?;
        if data_capacity < bundle.bytes.len() as u64
            || data_cursor > capacity
            || data_capacity > capacity - data_cursor
        {
            return Err(RomxError::Invalid(
                "mutable capacity cannot hold the initialized SAVE bundles".into(),
            ));
        }
        let data_start = usize::try_from(data_cursor)
            .map_err(|_| RomxError::Invalid("mutable data offset is too large".into()))?;
        output[data_start..data_start + bundle.bytes.len()].copy_from_slice(&bundle.bytes);
        let entry = build_mutable_entry(
            &bundle.key,
            data_cursor,
            data_capacity,
            bundle.bytes.len() as u64,
            bundle.crc32,
        )?;
        let entry_start = HEADER
            .checked_add(
                ENTRY
                    .checked_mul(slot_index)
                    .ok_or_else(|| RomxError::Invalid("mutable directory overflow".into()))?,
            )
            .ok_or_else(|| RomxError::Invalid("mutable directory overflow".into()))?;
        output[entry_start..entry_start + ENTRY].copy_from_slice(&entry);
        data_cursor += data_capacity;
    }
    Ok(output)
}

fn stream_copy<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    digest: &mut Option<Sha256>,
) -> Result<(u64, u32), RomxError> {
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut size = 0u64;
    let mut crc = 0xffff_ffff;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let chunk = &buffer[..count];
        writer.write_all(chunk)?;
        if let Some(digest) = digest.as_mut() {
            digest.update(chunk);
        }
        for byte in chunk {
            crc = (crc >> 8) ^ CRC32_TABLE[((crc ^ u32::from(*byte)) & 0xff) as usize];
        }
        size = size
            .checked_add(count as u64)
            .ok_or_else(|| RomxError::Invalid("payload size overflow".into()))?;
    }
    if size == 0 {
        return Err(RomxError::Invalid("ROM payload must not be empty".into()));
    }
    Ok((size, crc ^ 0xffff_ffff))
}

fn write_container_stream<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    options: &PackOptions,
    path: &str,
    format: u16,
) -> Result<Footer, RomxError> {
    if let Some(cover) = cover {
        validate_png_bytes(cover)?;
    }
    let mut digest = options.body_sha256.then(Sha256::new);
    let (payload_size, payload_crc) = stream_copy(reader, writer, &mut digest)?;
    let metadata = metadata_bytes(
        metadata,
        cover,
        &format!("{payload_crc:08x}"),
        options.crc32_override.as_deref(),
    )?;
    let metadata_size = metadata.as_ref().map_or(0, |value| value.len() as u64);
    let ridx = make_ridx(
        path,
        if options.entry_format_id == 0 {
            format
        } else {
            options.entry_format_id
        },
        payload_size,
        payload_crc,
        options.include_entry_crc32,
    )?;
    writer.write_all(&ridx)?;
    if let Some(digest) = digest.as_mut() {
        digest.update(&ridx);
    }
    let metadata_offset = payload_size + ridx.len() as u64;
    if let Some(metadata) = metadata.as_deref() {
        writer.write_all(metadata)?;
        if let Some(digest) = digest.as_mut() {
            digest.update(metadata);
        }
    }
    let cover_offset = metadata_offset + metadata_size;
    if let Some(cover) = cover {
        writer.write_all(cover)?;
        if let Some(digest) = digest.as_mut() {
            digest.update(cover);
        }
    }
    let immutable_end = cover_offset + cover.map_or(0, |value| value.len() as u64);
    let mutable = if let Some(existing) = options.mutable_region.as_deref() {
        if options.mutable_capacity == 0 || existing.len() as u64 != options.mutable_capacity {
            return Err(RomxError::Invalid(
                "preserved mutable region does not match mutable_capacity".into(),
            ));
        }
        existing.to_vec()
    } else {
        make_empty_mutable(
            options.mutable_capacity,
            options.mutable_entry_capacity,
            &options.mutable_save_bundles,
        )?
    };
    if !mutable.is_empty() {
        let aligned = (immutable_end + 4095) & !4095;
        if aligned > immutable_end {
            let padding = vec![0u8; (aligned - immutable_end) as usize];
            writer.write_all(&padding)?;
            if let Some(digest) = digest.as_mut() {
                digest.update(&padding);
            }
        }
        writer.write_all(&mutable)?;
    }
    let immutable_sha256 = digest
        .map(|digest| digest.finalize().into())
        .unwrap_or([0; 32]);
    let footer = Footer {
        version: VERSION,
        rom: Region {
            offset: 0,
            size: payload_size,
        },
        metadata: Region {
            offset: metadata_offset,
            size: metadata_size,
        },
        cover: Region {
            offset: if cover.is_some() { cover_offset } else { 0 },
            size: cover.map_or(0, |value| value.len() as u64),
        },
        mutable_capacity: options.mutable_capacity,
        platform_id: options.platform_id,
        launch_format_id: options.launch_format_id,
        immutable_hash_algorithm: if options.body_sha256 {
            HASH_SHA256
        } else {
            HASH_NONE
        },
        immutable_sha256,
        footer_crc32: 0,
        reserved: [0; 44],
        flags: (if metadata_size > 0 { FLAG_METADATA } else { 0 })
            | (if cover.is_some() { FLAG_COVER } else { 0 })
            | (if options.body_sha256 {
                FLAG_BODY_SHA256
            } else {
                0
            })
            | (if options.include_entry_crc32 {
                FLAG_ENTRY_CRC32
            } else {
                0
            }),
        body_sha256: immutable_sha256,
    };
    writer.write_all(&footer.encode())?;
    Ok(footer)
}

fn build_container(
    rom: &[u8],
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    options: &PackOptions,
) -> Result<Vec<u8>, RomxError> {
    let mut output = Cursor::new(Vec::new());
    write_container_stream(
        &mut Cursor::new(rom),
        &mut output,
        metadata,
        cover,
        options,
        "payload.bin",
        if options.entry_format_id == 0 {
            3
        } else {
            options.entry_format_id
        },
    )?;
    Ok(output.into_inner())
}
pub fn pack_bytes(
    rom: &[u8],
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
) -> Result<Vec<u8>, RomxError> {
    pack_bytes_with_writer_options(rom, metadata, cover, &PackOptions::default())
}
pub fn pack_bytes_with_crc32(
    rom: &[u8],
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    override_crc: Option<&str>,
) -> Result<Vec<u8>, RomxError> {
    let options = PackOptions {
        crc32_override: override_crc.map(str::to_owned),
        ..Default::default()
    };
    pack_bytes_with_writer_options(rom, metadata, cover, &options)
}
pub fn pack_bytes_with_writer_options(
    rom: &[u8],
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    options: &PackOptions,
) -> Result<Vec<u8>, RomxError> {
    let cover = cover
        .map(|value| normalize_cover_bytes(value, options.cover_target))
        .transpose()?;
    build_container(rom, metadata, cover.as_deref(), options)
}
pub fn pack_bytes_with_options(
    rom: &[u8],
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    override_crc: Option<&str>,
    target: Option<(u32, u32)>,
) -> Result<Vec<u8>, RomxError> {
    let cover = cover
        .map(|value| normalize_cover_bytes(value, target))
        .transpose()?;
    let options = PackOptions {
        crc32_override: override_crc.map(str::to_owned),
        ..Default::default()
    };
    build_container(rom, metadata, cover.as_deref(), &options)
}
fn write_atomic_stream<F>(path: &Path, replace: bool, write_output: F) -> Result<(), RomxError>
where
    F: FnOnce(&mut fs::File) -> Result<(), RomxError>,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() && !replace {
        return Err(RomxError::Exists(path.to_owned()));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("romx");
    let temporary = parent.join(format!(
        ".{name}.tmp-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        write_output(&mut file)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok::<(), RomxError>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
pub(crate) fn pack_path_with_metadata_options(
    rom: &Path,
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    output: &Path,
    options: &PackOptions,
) -> Result<(), RomxError> {
    let entry = rom
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("payload.bin")
        .replace('\\', "/");
    let format = format_id_from_path(&entry);
    let mut options = options.clone();
    if options.launch_format_id == 1 {
        options.launch_format_id = launch_format_id_for_extension(
            Path::new(&entry)
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default(),
        );
    }
    let cover = cover
        .map(|value| normalize_cover_bytes(value, options.cover_target))
        .transpose()?;
    write_atomic_stream(output, options.replace_existing, |writer| {
        let mut input = fs::File::open(rom)?;
        write_container_stream(
            &mut input,
            writer,
            metadata,
            cover.as_deref(),
            &options,
            &entry,
            format,
        )?;
        Ok(())
    })
}
pub fn pack_to_path(
    rom: &Path,
    metadata: Option<&Path>,
    cover: Option<&Path>,
    output: &Path,
) -> Result<(), RomxError> {
    pack_to_path_with_writer_options(rom, metadata, cover, output, &PackOptions::default())
}
pub fn pack_to_path_with_crc32(
    rom: &Path,
    metadata: Option<&Path>,
    cover: Option<&Path>,
    output: &Path,
    override_crc: Option<&str>,
) -> Result<(), RomxError> {
    let options = PackOptions {
        crc32_override: override_crc.map(str::to_owned),
        ..Default::default()
    };
    pack_to_path_with_writer_options(rom, metadata, cover, output, &options)
}
pub fn pack_to_path_with_writer_options(
    rom: &Path,
    metadata: Option<&Path>,
    cover: Option<&Path>,
    output: &Path,
    options: &PackOptions,
) -> Result<(), RomxError> {
    let metadata = metadata
        .map(|path| parse_json_strict(&fs::read(path)?))
        .transpose()?;
    let cover = cover.map(fs::read).transpose()?;
    pack_path_with_metadata_options(rom, metadata.as_ref(), cover.as_deref(), output, options)
}
pub fn pack_to_path_with_options(
    rom: &Path,
    metadata: Option<&Path>,
    cover: Option<&Path>,
    output: &Path,
    override_crc: Option<&str>,
    target: Option<(u32, u32)>,
) -> Result<(), RomxError> {
    let metadata = metadata
        .map(|path| parse_json_strict(&fs::read(path)?))
        .transpose()?;
    let cover = cover
        .map(|path| normalize_cover_path(path, target))
        .transpose()?;
    let options = PackOptions {
        crc32_override: override_crc.map(str::to_owned),
        ..Default::default()
    };
    pack_path_with_metadata_options(rom, metadata.as_ref(), cover.as_deref(), output, &options)
}

fn all_zero(value: &[u8]) -> bool {
    value.iter().all(|byte| *byte == 0)
}
struct Parsed<'a> {
    footer: Footer,
    entries: Vec<RidxEntry>,
    metadata: Option<&'a [u8]>,
    cover: Option<&'a [u8]>,
    entrypoint: &'a [u8],
    immutable_size: usize,
}
fn parse_container(bytes: &[u8], verify_entries: bool) -> Result<Parsed<'_>, RomxError> {
    if bytes.len() < FOOTER_SIZE {
        return Err(RomxError::Invalid(
            "file is shorter than the ROMX footer".into(),
        ));
    }
    let footer_offset = bytes.len() - FOOTER_SIZE;
    let mut footer = Footer::decode(&bytes[footer_offset..])?;
    let payload_size = footer.rom.size as usize;
    if payload_size == 0
        || payload_size
            .checked_add(RIDX_HEADER_SIZE)
            .is_none_or(|end| end > footer_offset)
    {
        return Err(RomxError::Invalid(
            "payload size cannot locate a RIDX header".into(),
        ));
    }
    let header = &bytes[payload_size..payload_size + RIDX_HEADER_SIZE];
    if &header[..4] != RIDX_MAGIC
        || read_u16(header, 4)? != RIDX_VERSION
        || read_u16(header, 6)? as usize != RIDX_HEADER_SIZE
        || read_u32(header, 12)? as usize != RIDX_ENTRY_SIZE
        || read_u32(header, 16)? != 0
        || !all_zero(&header[0x18..])
    {
        return Err(RomxError::Invalid("invalid RIDX header".into()));
    }
    let count = read_u32(header, 8)? as usize;
    if count == 0 || count > (footer_offset - payload_size - RIDX_HEADER_SIZE) / RIDX_ENTRY_SIZE {
        return Err(RomxError::Invalid("invalid RIDX entry count".into()));
    }
    let index_size = RIDX_HEADER_SIZE + count * RIDX_ENTRY_SIZE;
    if payload_size + index_size > footer_offset {
        return Err(RomxError::Invalid("RIDX exceeds immutable content".into()));
    }
    let mut index = bytes[payload_size..payload_size + index_size].to_vec();
    let index_crc = read_u32(&index, 0x14)?;
    put_u32(&mut index, 0x14, 0);
    if crc32_u32(&index) != index_crc {
        return Err(RomxError::Invalid("RIDX CRC32 mismatch".into()));
    }
    let mut entries = Vec::with_capacity(count);
    let mut paths = Vec::with_capacity(count);
    let mut ranges = Vec::with_capacity(count);
    let mut entrypoint = None;
    for position in 0..count {
        let base = RIDX_HEADER_SIZE + position * RIDX_ENTRY_SIZE;
        let flags = read_u32(&index, base)?;
        let format = read_u16(&index, base + 4)?;
        let path_size = read_u16(&index, base + 6)? as usize;
        let offset = read_u64(&index, base + 8)?;
        let size = read_u64(&index, base + 16)?;
        let value = read_u32(&index, base + 24)?;
        if flags & !(ENTRYPOINT | HAS_CRC32) != 0
            || read_u32(&index, base + 28)? != 0
            || !(1..=RIDX_PATH_CAPACITY).contains(&path_size)
            || !all_zero(&index[base + 0x20 + path_size..base + RIDX_ENTRY_SIZE])
        {
            return Err(RomxError::Invalid("invalid RIDX entry fields".into()));
        }
        let path = String::from_utf8(index[base + 0x20..base + 0x20 + path_size].to_vec())
            .map_err(|_| RomxError::Invalid("RIDX path is not strict UTF-8".into()))?;
        validate_virtual_path(&path)?;
        if paths
            .iter()
            .any(|value: &String| value.to_lowercase() == path.to_lowercase())
        {
            return Err(RomxError::Invalid(
                "RIDX paths collide after case folding".into(),
            ));
        }
        paths.push(path.clone());
        if offset > footer.rom.size || size > footer.rom.size - offset {
            return Err(RomxError::Invalid("RIDX entry exceeds payload".into()));
        }
        if size > 0 {
            ranges.push((offset, offset + size));
        }
        let is_entrypoint = flags & ENTRYPOINT != 0;
        if is_entrypoint {
            if entrypoint.is_some() || offset != 0 || size == 0 || format == 0 {
                return Err(RomxError::Invalid(
                    "entrypoint violates zero-offset or format rules".into(),
                ));
            }
            entrypoint = Some(position);
        }
        entries.push(RidxEntry {
            flags,
            format_id: format,
            path,
            data_offset: offset,
            data_size: size,
            crc32: (flags & HAS_CRC32 != 0).then(|| format!("{value:08x}")),
            entrypoint: is_entrypoint,
        });
    }
    let entrypoint_index = entrypoint
        .ok_or_else(|| RomxError::Invalid("RIDX must contain exactly one entrypoint".into()))?;
    ranges.sort_unstable();
    let mut cursor = 0u64;
    for (start, end) in ranges {
        if start < cursor {
            return Err(RomxError::Invalid("RIDX payload ranges overlap".into()));
        }
        if start > cursor && !all_zero(&bytes[cursor as usize..start as usize]) {
            return Err(RomxError::Invalid(
                "unindexed payload bytes are non-zero".into(),
            ));
        }
        cursor = end;
    }
    if cursor < footer.rom.size && !all_zero(&bytes[cursor as usize..footer.rom.size as usize]) {
        return Err(RomxError::Invalid(
            "trailing unindexed payload bytes are non-zero".into(),
        ));
    }
    if count == 1 && (entries[0].data_offset != 0 || entries[0].data_size != footer.rom.size) {
        return Err(RomxError::Invalid(
            "single-file payload is not exact and contiguous".into(),
        ));
    }
    let metadata_offset = payload_size + index_size;
    if footer.metadata.size > DEFAULT_MAX_METADATA_SIZE {
        return Err(RomxError::Metadata(
            "metadata exceeds the 1 MiB limit".into(),
        ));
    }
    if footer.cover.size > DEFAULT_MAX_COVER_SIZE {
        return Err(RomxError::Cover("cover exceeds the 32 MiB limit".into()));
    }
    let cover_offset = metadata_offset
        .checked_add(footer.metadata.size as usize)
        .ok_or_else(|| RomxError::Invalid("metadata range overflow".into()))?;
    let immutable_end = cover_offset
        .checked_add(footer.cover.size as usize)
        .ok_or_else(|| RomxError::Invalid("cover range overflow".into()))?;
    let immutable_size = if footer.mutable_capacity != 0 {
        if footer.mutable_capacity % 4096 != 0
            || footer.mutable_capacity < MIN_MUTABLE_CAPACITY
            || footer.mutable_capacity as usize > footer_offset
        {
            return Err(RomxError::Invalid("invalid mutable capacity".into()));
        }
        let mutable_offset = footer_offset - footer.mutable_capacity as usize;
        let aligned = (immutable_end + 4095) & !4095;
        if mutable_offset != aligned || !all_zero(&bytes[immutable_end..mutable_offset]) {
            return Err(RomxError::Invalid(
                "invalid immutable alignment padding".into(),
            ));
        }
        if mutable_offset + 4096 > footer_offset
            || &bytes[mutable_offset..mutable_offset + 4] != b"RMUT"
        {
            return Err(RomxError::Invalid("invalid mutable header".into()));
        }
        let mut header = bytes[mutable_offset..mutable_offset + 4096].to_vec();
        let stored = read_u32(&header, 0x34)?;
        put_u32(&mut header, 0x34, 0);
        if crc32_u32(&header) != stored {
            return Err(RomxError::Invalid("mutable header CRC32 mismatch".into()));
        }
        mutable_offset
    } else {
        if immutable_end != footer_offset {
            return Err(RomxError::Invalid("unexpected bytes before footer".into()));
        }
        footer_offset
    };
    footer.metadata.offset = if footer.metadata.size != 0 {
        metadata_offset as u64
    } else {
        0
    };
    footer.cover.offset = if footer.cover.size != 0 {
        cover_offset as u64
    } else {
        0
    };
    if footer.immutable_hash_algorithm == HASH_SHA256
        && payload_sha256(&bytes[..immutable_size]) != footer.immutable_sha256
    {
        return Err(RomxError::BodyHashMismatch);
    }
    if verify_entries {
        for entry in &entries {
            if let Some(expected) = &entry.crc32 {
                let actual = crc32_u32(
                    &bytes[entry.data_offset as usize
                        ..(entry.data_offset + entry.data_size) as usize],
                );
                if format!("{actual:08x}") != *expected {
                    return Err(RomxError::Invalid(format!(
                        "RIDX entry CRC32 mismatch: {}",
                        entry.path
                    )));
                }
            }
        }
    }
    let entry_offset = entries[entrypoint_index].data_offset as usize;
    let entry_size = entries[entrypoint_index].data_size as usize;
    let metadata = (footer.metadata.size != 0)
        .then(|| &bytes[metadata_offset..metadata_offset + footer.metadata.size as usize]);
    let cover = (footer.cover.size != 0)
        .then(|| &bytes[cover_offset..cover_offset + footer.cover.size as usize]);
    Ok(Parsed {
        footer,
        entries,
        metadata,
        cover,
        entrypoint: &bytes[entry_offset..entry_offset + entry_size],
        immutable_size,
    })
}
fn preview(parsed: Parsed<'_>) -> Result<RomxPreview, RomxError> {
    let metadata = parsed.metadata.map(parse_json_strict).transpose()?;
    let cover = parsed
        .cover
        .map(|bytes| {
            validate_png_bytes(bytes)?;
            Ok::<_, RomxError>(bytes.to_vec())
        })
        .transpose()?;
    Ok(RomxPreview {
        footer: parsed.footer,
        metadata,
        cover,
        entries: parsed.entries,
    })
}
pub fn read_bytes(bytes: &[u8]) -> Result<RomxDocument, RomxError> {
    let parsed = parse_container(bytes, false)?;
    let metadata = parsed.metadata.map(parse_json_strict).transpose()?;
    let cover = parsed
        .cover
        .map(|bytes| {
            validate_png_bytes(bytes)?;
            Ok::<_, RomxError>(bytes.to_vec())
        })
        .transpose()?;
    Ok(RomxDocument {
        footer: parsed.footer,
        rom: parsed.entrypoint.to_vec(),
        metadata,
        cover,
        entries: parsed.entries,
    })
}
pub fn read_path(path: &Path) -> Result<RomxDocument, RomxError> {
    read_bytes(&fs::read(path)?)
}
pub fn read_metadata_cover_bytes(bytes: &[u8]) -> Result<RomxPreview, RomxError> {
    preview(parse_container(bytes, false)?)
}

fn read_file_range(file: &mut fs::File, offset: u64, size: usize) -> Result<Vec<u8>, RomxError> {
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0u8; size];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_preview_file(path: &Path) -> Result<RomxPreview, RomxError> {
    let mut file = fs::File::open(path)?;
    let file_size = file.metadata()?.len();
    if file_size < FOOTER_SIZE as u64 {
        return Err(RomxError::Invalid(
            "file is shorter than the ROMX footer".into(),
        ));
    }
    let footer_offset = file_size - FOOTER_SIZE as u64;
    let footer_bytes = read_file_range(&mut file, footer_offset, FOOTER_SIZE)?;
    let mut footer = Footer::decode(&footer_bytes)?;
    let payload_size = footer.rom.size;
    if payload_size == 0 || payload_size + RIDX_HEADER_SIZE as u64 > footer_offset {
        return Err(RomxError::Invalid(
            "payload size cannot locate a RIDX header".into(),
        ));
    }
    let header = read_file_range(&mut file, payload_size, RIDX_HEADER_SIZE)?;
    if &header[..4] != RIDX_MAGIC
        || read_u16(&header, 4)? != RIDX_VERSION
        || read_u16(&header, 6)? as usize != RIDX_HEADER_SIZE
        || read_u32(&header, 12)? as usize != RIDX_ENTRY_SIZE
        || read_u32(&header, 16)? != 0
        || !all_zero(&header[0x18..])
    {
        return Err(RomxError::Invalid("invalid RIDX header".into()));
    }
    let count = read_u32(&header, 8)? as usize;
    if count == 0 {
        return Err(RomxError::Invalid("invalid RIDX entry count".into()));
    }
    let index_size =
        RIDX_HEADER_SIZE
            .checked_add(count.checked_mul(RIDX_ENTRY_SIZE).ok_or_else(|| {
                RomxError::Invalid("RIDX entry count overflows index size".into())
            })?)
            .ok_or_else(|| RomxError::Invalid("RIDX size overflow".into()))?;
    let index_end = payload_size
        .checked_add(index_size as u64)
        .ok_or_else(|| RomxError::Invalid("RIDX range overflow".into()))?;
    if index_end > footer_offset {
        return Err(RomxError::Invalid("RIDX exceeds immutable content".into()));
    }
    let mut index = read_file_range(&mut file, payload_size, index_size)?;
    let index_crc = read_u32(&index, 0x14)?;
    put_u32(&mut index, 0x14, 0);
    if crc32_u32(&index) != index_crc {
        return Err(RomxError::Invalid("RIDX CRC32 mismatch".into()));
    }
    let mut entries = Vec::with_capacity(count);
    let mut paths = Vec::<String>::with_capacity(count);
    let mut entrypoint = None;
    for position in 0..count {
        let base = RIDX_HEADER_SIZE + position * RIDX_ENTRY_SIZE;
        let flags = read_u32(&index, base)?;
        let format = read_u16(&index, base + 4)?;
        let path_size = read_u16(&index, base + 6)? as usize;
        let offset = read_u64(&index, base + 8)?;
        let size = read_u64(&index, base + 16)?;
        let value = read_u32(&index, base + 24)?;
        if flags & !(ENTRYPOINT | HAS_CRC32) != 0
            || read_u32(&index, base + 28)? != 0
            || !(1..=RIDX_PATH_CAPACITY).contains(&path_size)
            || !all_zero(&index[base + 0x20 + path_size..base + RIDX_ENTRY_SIZE])
            || offset > payload_size
            || size > payload_size - offset
        {
            return Err(RomxError::Invalid("invalid RIDX entry fields".into()));
        }
        let path = String::from_utf8(index[base + 0x20..base + 0x20 + path_size].to_vec())
            .map_err(|_| RomxError::Invalid("RIDX path is not strict UTF-8".into()))?;
        validate_virtual_path(&path)?;
        if paths
            .iter()
            .any(|value: &String| value.to_lowercase() == path.to_lowercase())
        {
            return Err(RomxError::Invalid(
                "RIDX paths collide after case folding".into(),
            ));
        }
        paths.push(path.clone());
        let is_entrypoint = flags & ENTRYPOINT != 0;
        if is_entrypoint {
            if entrypoint.is_some() || offset != 0 || size == 0 || format == 0 {
                return Err(RomxError::Invalid(
                    "entrypoint violates zero-offset or format rules".into(),
                ));
            }
            entrypoint = Some(position);
        }
        entries.push(RidxEntry {
            flags,
            format_id: format,
            path,
            data_offset: offset,
            data_size: size,
            crc32: (flags & HAS_CRC32 != 0).then(|| format!("{value:08x}")),
            entrypoint: is_entrypoint,
        });
    }
    let _entrypoint = entrypoint
        .ok_or_else(|| RomxError::Invalid("RIDX must contain exactly one entrypoint".into()))?;
    if count == 1 && (entries[0].data_offset != 0 || entries[0].data_size != payload_size) {
        return Err(RomxError::Invalid(
            "single-file payload is not exact and contiguous".into(),
        ));
    }
    let metadata_size = usize::try_from(footer.metadata.size)
        .map_err(|_| RomxError::Invalid("metadata size is too large".into()))?;
    if footer.metadata.size > DEFAULT_MAX_METADATA_SIZE {
        return Err(RomxError::Metadata(
            "metadata exceeds the 1 MiB limit".into(),
        ));
    }
    let metadata_offset = index_end;
    let cover_offset = metadata_offset
        .checked_add(footer.metadata.size)
        .ok_or_else(|| RomxError::Invalid("metadata range overflow".into()))?;
    let immutable_end = cover_offset
        .checked_add(footer.cover.size)
        .ok_or_else(|| RomxError::Invalid("cover range overflow".into()))?;
    let _immutable_size = if footer.mutable_capacity != 0 {
        if footer.mutable_capacity % 4096 != 0
            || footer.mutable_capacity < MIN_MUTABLE_CAPACITY
            || footer.mutable_capacity > footer_offset
        {
            return Err(RomxError::Invalid("invalid mutable capacity".into()));
        }
        let mutable_offset = footer_offset - footer.mutable_capacity;
        let aligned = (immutable_end + 4095) & !4095;
        if mutable_offset != aligned {
            return Err(RomxError::Invalid(
                "invalid immutable alignment padding".into(),
            ));
        }
        if mutable_offset > immutable_end
            && !all_zero(&read_file_range(
                &mut file,
                immutable_end,
                (mutable_offset - immutable_end) as usize,
            )?)
        {
            return Err(RomxError::Invalid(
                "immutable alignment padding is non-zero".into(),
            ));
        }
        let header = read_file_range(&mut file, mutable_offset, 4096)?;
        let stored = read_u32(&header, 0x34)?;
        let mut checked = header;
        put_u32(&mut checked, 0x34, 0);
        if &checked[..4] != b"RMUT" || crc32_u32(&checked) != stored {
            return Err(RomxError::Invalid("mutable header CRC32 mismatch".into()));
        }
        mutable_offset
    } else {
        if immutable_end != footer_offset {
            return Err(RomxError::Invalid("unexpected bytes before footer".into()));
        }
        footer_offset
    };
    let cover_size = usize::try_from(footer.cover.size)
        .map_err(|_| RomxError::Invalid("cover size is too large".into()))?;
    if footer.cover.size > DEFAULT_MAX_COVER_SIZE {
        return Err(RomxError::Cover("cover exceeds the 32 MiB limit".into()));
    }
    let metadata_bytes = if metadata_size == 0 {
        None
    } else {
        Some(read_file_range(&mut file, metadata_offset, metadata_size)?)
    };
    let cover_bytes = if cover_size == 0 {
        None
    } else {
        Some(read_file_range(&mut file, cover_offset, cover_size)?)
    };
    let metadata = metadata_bytes
        .as_deref()
        .map(parse_json_strict)
        .transpose()?;
    let cover = cover_bytes
        .as_deref()
        .map(|bytes| {
            validate_png_bytes(bytes)?;
            Ok::<_, RomxError>(bytes.to_vec())
        })
        .transpose()?;
    footer.metadata.offset = if footer.metadata.size == 0 {
        0
    } else {
        metadata_offset
    };
    footer.cover.offset = if footer.cover.size == 0 {
        0
    } else {
        cover_offset
    };
    // Preview intentionally does not hash the payload.
    Ok(RomxPreview {
        footer,
        metadata,
        cover,
        entries,
    })
}

pub fn read_metadata_cover_path(path: &Path) -> Result<RomxPreview, RomxError> {
    read_preview_file(path)
}
pub fn validate_bytes(bytes: &[u8]) -> Result<ValidationReport, RomxError> {
    let parsed = parse_container(bytes, false)?;
    let mut report = ValidationReport {
        structure: ValidationStatus::Valid,
        payload_hashes: if parsed.entries.iter().any(|entry| entry.crc32.is_some()) {
            ValidationStatus::Valid
        } else {
            ValidationStatus::Absent
        },
        body_sha256: if parsed.footer.immutable_hash_algorithm == HASH_SHA256 {
            ValidationStatus::Valid
        } else {
            ValidationStatus::Absent
        },
        metadata: ValidationStatus::Absent,
        cover: ValidationStatus::Absent,
        ..Default::default()
    };
    report.computed_payload_crc32 = Some(format!("{:08x}", crc32_u32(parsed.entrypoint)));
    report.computed_payload_sha256 = payload_sha256(parsed.entrypoint);
    if let Some(metadata) = parsed.metadata {
        match parse_json_strict(metadata)
            .and_then(|value| validate_metadata_template(&value).map(|_| value))
        {
            Ok(value) => {
                report.metadata = ValidationStatus::Valid;
                report.metadata_result = Some("valid".into());
                report.metadata_crc32 = if value.get("crc32").is_some() {
                    Crc32Status::ValidLookup
                } else {
                    Crc32Status::Absent
                };
            }
            Err(error) => {
                report.metadata = ValidationStatus::Invalid;
                report.metadata_result = Some(error.to_string());
                report.metadata_crc32 = Crc32Status::Invalid;
            }
        }
    }
    if let Some(cover) = parsed.cover {
        match validate_png_bytes(cover) {
            Ok(info) => {
                report.cover = ValidationStatus::Valid;
                report.cover_info = Some(info);
                report.cover_result = Some("valid".into());
                report.cover_hashes = ValidationStatus::Valid;
                report.computed_cover_sha256 = payload_sha256(cover);
            }
            Err(error) => {
                report.cover = ValidationStatus::Invalid;
                report.cover_result = Some(error.to_string());
            }
        }
    }
    report.computed_body_sha256 = payload_sha256(&bytes[..parsed.immutable_size]);
    Ok(report)
}
pub fn validate_path(path: &Path) -> Result<ValidationReport, RomxError> {
    validate_bytes(&fs::read(path)?)
}

/// Return the complete mutable region, including namespaces that this crate
/// does not interpret. This is intended for lossless frontend edits.
pub fn read_mutable_region(path: &Path) -> Result<Option<Vec<u8>>, RomxError> {
    let bytes = fs::read(path)?;
    let parsed = parse_container(&bytes, false)?;
    if let Some(metadata) = parsed.metadata {
        parse_json_strict(metadata)?;
    }
    if let Some(cover) = parsed.cover {
        validate_png_bytes(cover)?;
    }
    if parsed.footer.mutable_capacity == 0 {
        return Ok(None);
    }
    let capacity = usize::try_from(parsed.footer.mutable_capacity)
        .map_err(|_| RomxError::Invalid("mutable capacity is too large".into()))?;
    if bytes.len() < FOOTER_SIZE + capacity {
        return Err(RomxError::Invalid("mutable region is truncated".into()));
    }
    let offset = bytes.len() - FOOTER_SIZE - capacity;
    Ok(Some(bytes[offset..offset + capacity].to_vec()))
}

/// Read the information a frontend needs for an inspect/details screen.
/// This deliberately keeps the payload bytes out of the returned structure;
/// callers can use read_path only when they actually need to extract or
/// repack the payload.
pub fn inspect_romx_path(path: &Path) -> Result<RomxInspection, RomxError> {
    let bytes = fs::read(path)?;
    let document = read_bytes(&bytes)?;
    let validation = validate_bytes(&bytes)?;
    let mutable =
        save::read_mutable_save_objects_with_capacity(&bytes, document.footer.mutable_capacity)?;
    let identity = identity_from_document(&document)?;
    Ok(RomxInspection {
        identity,
        validation,
        mutable,
        payload_size: document.footer.rom.size,
        entry_count: document.entries.len(),
        has_metadata: document.metadata.is_some(),
        has_cover: document.cover.is_some(),
    })
}
fn atomic_extract(path: &Path, bytes: &[u8]) -> Result<(), RomxError> {
    write_atomic_stream(path, true, |file| {
        file.write_all(bytes)?;
        Ok(())
    })
}
pub fn extract_payload_to_path(romx: &Path, output: &Path, replace: bool) -> Result<(), RomxError> {
    let document = read_path(romx)?;
    if output.exists() && !replace {
        return Err(RomxError::Exists(output.to_owned()));
    }
    atomic_extract(output, &document.rom)
}
pub fn extract_to_dir(path: &Path, output: &Path) -> Result<PathBuf, RomxError> {
    let document = read_path(path)?;
    fs::create_dir_all(output)?;
    let entry = document
        .entries
        .iter()
        .find(|entry| entry.entrypoint)
        .ok_or_else(|| RomxError::Invalid("ROMX entrypoint is missing".into()))?;
    let name = Path::new(&entry.path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("payload.bin");
    let payload = output.join(name);
    atomic_extract(&payload, &document.rom)?;
    if let Some(metadata) = document.metadata {
        atomic_extract(
            &output.join("metadata.json"),
            &serde_json::to_vec_pretty(&metadata)?,
        )?;
    }
    if let Some(cover) = document.cover {
        atomic_extract(&output.join("cover.png"), &cover)?;
    }
    // SAVE objects are part of the user-facing ROMX payload even though they
    // live outside the immutable ROM/RIDX regions.  Export them below a
    // dedicated directory so the result can be fed back to the platform-aware
    // save scanner or copied into a frontend's save root.
    extract_mutable_save_objects(path, &output.join("saves"))?;
    Ok(payload)
}
pub fn required_metadata(
    name: impl Into<String>,
    _platform: impl Into<String>,
    _format: impl Into<String>,
) -> Value {
    serde_json::json!({ "schema_version": SPEC_VERSION, "name": name.into() })
}
pub fn application_version() -> &'static str {
    APP_VERSION
}
