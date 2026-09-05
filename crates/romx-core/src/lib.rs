//! ROMX 0.2.0 core container implementation.
//!
//! The writer and reader follow the active ROMX 0.2.0 wire format: a raw
//! payload, RIDX index, optional strict metadata, optional strict PNG cover,
//! and a fixed 128-byte footer. No ROMX 0.1.x layout is accepted or emitted.

use crc32fast::Hasher as Crc32Hasher;
use image::ImageReader;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use std::borrow::Cow;
use std::fmt;
use std::fs;
use std::io::{self, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod error;
mod extraction;
mod frontend;
mod identity;
mod libretro;
mod lpl;
mod mutable;
mod probe;
mod reader;
mod registry;
mod save;
mod validation;
mod writer;

pub use error::RomxError;
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
pub use mutable::{
    copy_region as copy_mutable_region, write_file as write_mutable_file, MutableBundle,
    MutableFile,
};
pub use probe::{
    extract_embedded_info, infer_payload_format, inspect_payload_profile, EmbeddedCover,
    EmbeddedInfo, PayloadProfile, Probe,
};
pub use reader::{Metadata, PayloadFile, PayloadMapping, Reader, VfsFile};
pub use registry::{
    format_name as registry_format_name, platform_name as registry_platform_name,
    require_known_platform,
};
pub use save::{
    detect_save_bundles, detect_save_bundles_with_flags, extract_mutable_save_object,
    extract_mutable_save_objects, inspect_mutable_path, inspect_mutable_path_with_flags,
    is_supported_save_file, read_mutable_save_objects, save_layout, save_profile,
    MutableRegionInfo, MutableSaveFileData, MutableSaveObject, SaveCandidate, SaveCandidateFile,
    SaveCatalog, SaveGrouping, SaveInventory, SaveLayout, SaveProfile, SaveScope, SaveSlot,
    SaveSourceFormat,
};
pub use writer::Writer;

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
    "zip",
];

pub const RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY: u64 = 512 * 1024;
pub const RECOMMENDED_UNKNOWN_CARTRIDGE_MUTABLE_CAPACITY: u64 = 1024 * 1024;
pub const RECOMMENDED_NDS_MUTABLE_CAPACITY: u64 = 16 * 1024 * 1024;
pub const RECOMMENDED_DISC_MUTABLE_CAPACITY: u64 = 2 * 1024 * 1024;
pub const RECOMMENDED_PS2_MUTABLE_CAPACITY: u64 = 32 * 1024 * 1024;
pub const RECOMMENDED_DIRECTORY_SAVE_MUTABLE_CAPACITY: u64 = 64 * 1024 * 1024;
pub const DEFAULT_MUTABLE_ENTRY_CAPACITY: u32 = 8;
pub const DEFAULT_SAVE_OBJECT_CAPACITY: u64 = 512 * 1024;
const MUTABLE_ALIGNMENT: u64 = 4096;
const MUTABLE_BUNDLE_OVERHEAD_RESERVE: u64 = 4096;

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

/// One immutable payload file in a multi-entry ROMX container.
///
/// The entrypoint must be marked explicitly.  Its bytes are written first at
/// absolute payload offset zero; every other entry keeps the supplied
/// normalized virtual path so descriptor formats such as CUE/GDI/M3U can
/// resolve their sidecars through the ROMX VFS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackEntry {
    pub path: String,
    pub source: PathBuf,
    /// Zero lets the writer infer the registered format from `path`.
    pub format_id: u16,
    pub entrypoint: bool,
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
/// The wire-level bundle/header sizing is owned by libromx. This helper only
/// applies an application-level growth margin and the platform floor; callers
/// that have a `SaveCatalog` should use its measurement API when available.
pub fn recommended_mutable_capacity_for_save_bytes(
    base: u64,
    save_count: usize,
    save_bytes: u64,
) -> u64 {
    let estimated_bytes = DEFAULT_SAVE_OBJECT_CAPACITY.saturating_mul(save_count as u64);
    // Each RMBL object has a header, path table, and alignment padding in
    // addition to the source files. Keep that overhead in the region budget
    // so the per-object growth reservation made by libromx cannot consume the
    // last few blocks of an otherwise sufficient mutable region.
    let bundle_overhead = MUTABLE_BUNDLE_OVERHEAD_RESERVE.saturating_mul(save_count as u64);
    let detected_capacity = save_bytes
        .saturating_add(bundle_overhead)
        .max(estimated_bytes);
    let growth_margin = detected_capacity.saturating_div(4).max(4096);
    let minimum = detected_capacity.saturating_add(growth_margin);
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
pub(crate) static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    /// Existing ROMX source whose complete mutable region should be copied
    /// into the newly-created container. The copy is performed by libromx so
    /// unknown namespaces and inactive slots remain byte-for-byte intact.
    pub mutable_region_source: Option<PathBuf>,
    /// The caller owns this path as a short-lived staging file (normally a
    /// `.tmp` output).  In this mode the writer does not create a second
    /// Rust staging path and libromx writes directly to the temporary path.
    /// Callers must remove the path when a post-write operation fails.
    pub output_is_temporary: bool,
    /// Validate a Rust-created staging output before publishing it.  Temporary
    /// frontend outputs disable this after libromx has completed its own
    /// structural, metadata, cover, and entry-stream checks.
    pub post_write_validation: bool,
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
            mutable_region_source: None,
            output_is_temporary: false,
            post_write_validation: true,
        }
    }
}

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
    let mut checksum = Crc32Hasher::new();
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        checksum.update(&buffer[..count]);
    }
    Ok(format!("{:08x}", checksum.finalize()))
}
fn crc32_u32(value: &[u8]) -> u32 {
    let mut checksum = Crc32Hasher::new();
    checksum.update(value);
    checksum.finalize()
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
    normalize_cover_bytes_cow(value, target).map(Cow::into_owned)
}

fn normalize_cover_bytes_cow(
    value: &[u8],
    target: Option<(u32, u32)>,
) -> Result<Cow<'_, [u8]>, RomxError> {
    if value.is_empty() {
        return Err(RomxError::Cover("cover must not be empty".into()));
    }
    if target.is_none() && value.starts_with(PNG_SIGNATURE) {
        validate_png_bytes(value)?;
        return Ok(Cow::Borrowed(value));
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
    Ok(Cow::Owned(output.into_inner()))
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

/// Classify a Game Boy payload without loading the complete ROM into memory.
/// Only the header byte used by [`classify_gb_payload`] is read from disk.
pub fn classify_gb_payload_path(
    path: &Path,
    payload_format: Option<&str>,
) -> Result<&'static str, RomxError> {
    let mut header = [0u8; 0x144];
    let mut read = 0usize;
    let mut file = fs::File::open(path)?;
    while read < header.len() {
        let count = file.read(&mut header[read..])?;
        if count == 0 {
            break;
        }
        read += count;
    }
    classify_gb_payload(&header[..read], payload_format)
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
        "zip" => 0x91,
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
        0x91 => "zip",
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
        "mega_drive_32x" | "32x" => 0x13,
        "sega_cd" => 0x14,
        "saturn" => 0x15,
        "dreamcast" => 0x16,
        "pce" | "pc_engine" => 0x20,
        "pc_engine_cd" => 0x21,
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
        0x13 => "mega_drive_32x",
        0x14 => "sega_cd",
        0x15 => "saturn",
        0x16 => "dreamcast",
        0x20 => "pce",
        0x21 => "pc_engine_cd",
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

/// Execute a path-oriented libromx operation for an in-memory compatibility
/// API without retaining a second payload-sized allocation.  The byte APIs
/// predate the streaming reader/writer handles; staging their input in a
/// private temporary file keeps the actual container implementation in
/// libromx while retaining the existing public Rust API.
fn with_temporary_file<T>(
    suffix: &str,
    bytes: &[u8],
    operation: impl FnOnce(&Path) -> Result<T, RomxError>,
) -> Result<T, RomxError> {
    let mut attempts = 0u32;
    let path = loop {
        let serial = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = std::env::temp_dir().join(format!(
            "romx-core-{}-{serial}.{suffix}",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.write_all(bytes)?;
                break candidate;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempts < 8 => {
                attempts += 1;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let result = operation(&path);
    let cleanup = fs::remove_file(&path);
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(value), Err(_)) => Ok(value),
        (Err(error), _) => Err(error),
    }
}

/// Reserve a unique path for a libromx writer.  The placeholder is removed
/// before invoking C so `replace_existing = false` retains its normal
/// semantics instead of observing our reservation file as a collision.
fn with_temporary_output<T>(
    suffix: &str,
    operation: impl FnOnce(&Path) -> Result<T, RomxError>,
) -> Result<T, RomxError> {
    let mut attempts = 0u32;
    let path = loop {
        let serial = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = std::env::temp_dir().join(format!(
            "romx-core-{}-{serial}.{suffix}",
            std::process::id()
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => {
                fs::remove_file(&candidate)?;
                break candidate;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists && attempts < 8 => {
                attempts += 1;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let result = operation(&path);
    let _ = fs::remove_file(&path);
    result
}

fn pack_bytes_through_writer(
    rom: &[u8],
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    options: &PackOptions,
) -> Result<Vec<u8>, RomxError> {
    with_temporary_file("gba", rom, |payload| {
        with_temporary_output("romx", |output| {
            let entry = PackEntry {
                path: "payload.gba".into(),
                source: payload.to_owned(),
                format_id: if options.entry_format_id == 0 {
                    libromx_sys::ROMX_FORMAT_GBA
                } else {
                    options.entry_format_id
                },
                entrypoint: true,
            };
            writer::Writer::write_path_entries(&[entry], metadata, cover, output, options)?;
            fs::read(output).map_err(RomxError::from)
        })
    })
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
    pack_bytes_through_writer(rom, metadata, cover.as_deref(), options)
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
    pack_bytes_through_writer(rom, metadata, cover.as_deref(), &options)
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
        match fs::rename(&temporary, path) {
            Ok(()) => {}
            #[cfg(windows)]
            Err(error) if replace && path.exists() => {
                // std::fs::rename cannot replace an existing file on
                // Windows. Remove only the requested destination after the
                // staged file is durable, then complete the move.
                fs::remove_file(path)?;
                fs::rename(&temporary, path).map_err(|rename_error| {
                    io::Error::new(
                        rename_error.kind(),
                        format!("{rename_error}; original rename error: {error}"),
                    )
                })?;
            }
            Err(error) => return Err(error.into()),
        }
        Ok::<(), RomxError>(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
/// Pack one payload through libromx's streaming writer.
///
/// Mutable data must be supplied with [`PackOptions::mutable_region_source`]
/// or [`PackOptions::mutable_save_bundles`]. The old in-memory
/// `mutable_region` field is retained solely so downstream callers get an
/// explicit migration error instead of silently losing their data.
pub(crate) fn pack_path_with_metadata_options(
    rom: &Path,
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    output: &Path,
    options: &PackOptions,
) -> Result<(), RomxError> {
    if options.mutable_region.is_some() {
        return Err(RomxError::Invalid(
            "in-memory mutable_region is no longer supported; use mutable_region_source".into(),
        ));
    }
    let name = rom
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("payload.bin")
        .replace('\\', "/");
    let entry = PackEntry {
        path: name,
        source: rom.to_owned(),
        format_id: options.entry_format_id,
        entrypoint: true,
    };
    writer::Writer::write_path_entries(&[entry], metadata, cover, output, options).map(|_| ())
}

/// Pack a descriptor and sidecars through libromx's streaming writer.
pub fn pack_entries_to_path_with_writer_options(
    entries: &[PackEntry],
    entrypoint: Option<&str>,
    metadata: Option<&Path>,
    cover: Option<&Path>,
    output: &Path,
    options: &PackOptions,
) -> Result<(), RomxError> {
    if options.mutable_region.is_some() {
        return Err(RomxError::Invalid(
            "in-memory mutable_region is no longer supported; use mutable_region_source".into(),
        ));
    }
    let metadata = metadata
        .map(|path| parse_json_strict(&fs::read(path)?))
        .transpose()?;
    let cover = cover.map(fs::read).transpose()?;
    if let Some(requested) = entrypoint {
        if !entries
            .iter()
            .any(|entry| entry.entrypoint && entry.path == requested)
        {
            return Err(RomxError::Invalid(format!(
                "requested entrypoint is not marked in entries: {requested}"
            )));
        }
    }
    writer::Writer::write_path_entries(
        entries,
        metadata.as_ref(),
        cover.as_deref(),
        output,
        options,
    )
    .map(|_| ())
}

/// Convenience wrapper for multi-entry files using default writer options.
pub fn pack_entries_to_path(
    entries: &[PackEntry],
    entrypoint: &str,
    metadata: Option<&Path>,
    cover: Option<&Path>,
    output: &Path,
) -> Result<(), RomxError> {
    pack_entries_to_path_with_writer_options(
        entries,
        Some(entrypoint),
        metadata,
        cover,
        output,
        &PackOptions::default(),
    )
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

/// Compatibility wrapper for callers that already hold a complete ROMX
/// buffer. The actual parsing remains in libromx's reader; the temporary file
/// only adapts the old byte-oriented API to the streaming path API.
pub fn read_bytes(bytes: &[u8]) -> Result<RomxDocument, RomxError> {
    with_temporary_file("romx", bytes, reader::read_document_path)
}

pub fn read_path(path: &Path) -> Result<RomxDocument, RomxError> {
    reader::read_document_path(path)
}

pub fn read_metadata_cover_bytes(bytes: &[u8]) -> Result<RomxPreview, RomxError> {
    with_temporary_file("romx", bytes, reader::read_preview_path)
}

pub fn read_metadata_cover_path(path: &Path) -> Result<RomxPreview, RomxError> {
    reader::read_preview_path(path)
}
pub fn validate_bytes(bytes: &[u8]) -> Result<ValidationReport, RomxError> {
    with_temporary_file("romx", bytes, validate_path)
}
pub fn validate_path(path: &Path) -> Result<ValidationReport, RomxError> {
    validation::validate_path(path)
}

/// Return the complete mutable region, including namespaces that this crate
/// does not interpret. This is intended for lossless frontend edits.
pub fn read_mutable_region(path: &Path) -> Result<Option<Vec<u8>>, RomxError> {
    let reader = Reader::open(path)?;
    let info = reader.info()?;
    if info.mutable_region.size == 0 {
        return Ok(None);
    }
    reader
        .read_region(libromx_sys::ROMX_REGION_MUTABLE, info.mutable_region.size)
        .map(Some)
}

/// Read the information a frontend needs for an inspect/details screen.
/// This deliberately keeps the payload bytes out of the returned structure;
/// callers can use read_path only when they actually need to extract or
/// repack the payload.
pub fn inspect_romx_path(path: &Path) -> Result<RomxInspection, RomxError> {
    let reader = Reader::open(path)?;
    let info = reader.info()?;
    let validation = validation::validate_path(path)?;
    let mutable = save::read_mutable_save_objects(path)?;
    let identity = identity_from_path(path)?;
    let entry_count = reader.entries()?.len();
    Ok(RomxInspection {
        identity,
        validation,
        mutable,
        payload_size: info.payload.size,
        entry_count,
        has_metadata: info.metadata.size != 0,
        has_cover: info.cover.size != 0,
    })
}
fn atomic_extract(path: &Path, bytes: &[u8]) -> Result<(), RomxError> {
    write_atomic_stream(path, true, |file| {
        file.write_all(bytes)?;
        Ok(())
    })
}

/// Extract one indexed ROMX region without materializing the complete
/// container. The returned checksum is useful to callers that need a
/// RetroArch identity when the metadata does not contain one.
pub(crate) fn extract_entry_to_path(
    romx: &Path,
    entry: &RidxEntry,
    output: &Path,
    replace: bool,
) -> Result<String, RomxError> {
    let reader = Reader::open(romx)?;
    let entries = reader.entries()?;
    let index = entries
        .iter()
        .position(|candidate| {
            candidate.data_offset == entry.data_offset
                && candidate.data_size == entry.data_size
                && error::c_field(&candidate.path) == entry.path
        })
        .ok_or_else(|| RomxError::Invalid("ROMX entry is not present in the reader".into()))?;
    let expected_size = entries[index].data_size;
    let mut checksum = Crc32Hasher::new();
    write_atomic_stream(output, replace, |writer| {
        let mut digesting = DigestWriter {
            output: writer,
            checksum: &mut checksum,
        };
        let copied = reader.read_entry_to(entries[index].index, expected_size, &mut digesting)?;
        if copied != expected_size {
            return Err(RomxError::Invalid(
                "ROMX entry size changed while extracting".into(),
            ));
        }
        Ok(())
    })?;
    Ok(format!("{:08x}", checksum.finalize()))
}

struct DigestWriter<'a, W> {
    output: &'a mut W,
    checksum: &'a mut Crc32Hasher,
}

impl<W: Write> Write for DigestWriter<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.output.write_all(bytes)?;
        self.checksum.update(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.output.flush()
    }
}

pub fn extract_payload_to_path(romx: &Path, output: &Path, replace: bool) -> Result<(), RomxError> {
    extraction::extract_payload(romx, output, replace)
}
/// Extract a ROMX container through libromx without decoding the payload into
/// a Rust buffer.  Metadata and cover remain bounded by the C reader limits;
/// SAVE objects are delegated to the platform-aware save adapter.
pub fn extract_to_dir(path: &Path, output: &Path) -> Result<PathBuf, RomxError> {
    let reader = Reader::open(path)?;
    let entries = reader.entries()?;
    let entry = entries
        .iter()
        .find(|entry| entry.flags & libromx_sys::ROMX_RIDX_ENTRYPOINT != 0)
        .ok_or_else(|| RomxError::Invalid("ROMX entrypoint is missing".into()))?;
    fs::create_dir_all(output)?;
    let name = error::c_field(&entry.path);
    let name = Path::new(&name)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("payload.bin");
    let payload = output.join(name);
    extraction::extract_payload(path, &payload, true)?;
    if let Some(metadata) = reader.metadata_json()? {
        atomic_extract(&output.join("metadata.json"), &metadata)?;
    }
    if let Some(cover) = reader.cover_bytes()? {
        atomic_extract(&output.join("cover.png"), &cover)?;
    }
    save::extract_mutable_save_objects_from_path(
        path,
        &output.join("saves"),
        false,
        || false,
        |save_path| Ok(Some(save_path.to_owned())),
    )?;
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
