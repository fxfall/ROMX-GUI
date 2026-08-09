//! ROMX 1.0 core implementation.
//!
//! This crate follows the frozen ROMX 1.0 container and metadata contract.
//! The binary reader is deliberately independent from filenames, playlists,
//! image decoders, and emulators.  Image conversion remains available as an
//! adapter for the desktop/CLI layer, while the ROMX writer itself accepts
//! only structurally valid PNG bytes and embeds them byte-for-byte.

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use sha2::{Digest, Sha256};
use std::fmt;
use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

mod lpl;

pub use lpl::{
    export_lpl, export_lpl_with_output_handling, import_lpl, import_lpl_with_error_handling,
    import_lpl_with_output_handling, import_lpl_with_progress, is_official_lpl_item_field,
    is_official_lpl_root_field, plan_lpl_import, ExportLplOptions, ExportLplReport,
    ImportLplOptions, ImportLplPlan, ImportLplReport, PlannedLplItem, LPLX_METADATA_KEY,
    OFFICIAL_LPL_ITEM_FIELDS, OFFICIAL_LPL_ROOT_FIELDS, ROMX_LPLX_METADATA_FIELDS,
};

pub const FOOTER_SIZE: usize = 128;
pub const VERSION: u32 = 1;
pub const SPEC_VERSION: &str = "1.0";
/// Current application release version, shared by Core, CLI, and GUI.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

pub const FLAG_METADATA: u32 = 1 << 0;
pub const FLAG_COVER: u32 = 1 << 1;
pub const FLAG_BODY_SHA256: u32 = 1 << 2;
pub const FLAGS_V1_MASK: u32 = FLAG_METADATA | FLAG_COVER | FLAG_BODY_SHA256;
pub const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
pub const DEFAULT_MAX_METADATA_SIZE: u64 = 1024 * 1024;
pub const DEFAULT_MAX_COVER_SIZE: u64 = 32 * 1024 * 1024;
pub const DEFAULT_MAX_COVER_DIMENSION: u32 = 8192;
const MAGIC: &[u8; 4] = b"ROMX";
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
    #[error("ROMX body SHA-256 mismatch")]
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

impl Region {
    fn end(self) -> Result<u64, RomxError> {
        self.offset
            .checked_add(self.size)
            .ok_or_else(|| RomxError::Invalid("region offset/size overflow".into()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footer {
    pub version: u32,
    pub rom: Region,
    pub metadata: Region,
    pub cover: Region,
    /// Reserved bytes at footer offsets 0x38..0x58.  ROMX 1.0 does not store
    /// a payload SHA-256 in this field.
    pub reserved: [u8; 32],
    pub flags: u32,
    pub body_sha256: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct RomxDocument {
    pub footer: Footer,
    pub rom: Vec<u8>,
    /// Invalid optional metadata is ignored so the payload remains usable.
    pub metadata: Option<Value>,
    /// Invalid optional cover data is ignored so the payload remains usable.
    pub cover: Option<Vec<u8>>,
}

/// Lightweight ROMX data for previews and metadata editing.
#[derive(Debug, Clone)]
pub struct RomxPreview {
    pub footer: Footer,
    pub metadata: Option<Value>,
    pub cover: Option<Vec<u8>>,
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

/// Component-level validation details. Optional metadata and cover failures
/// are represented in this report and do not fail the top-level validation.
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
    /// Disabled by default, as required by the ROMX 1.0 writer contract.
    pub body_sha256: bool,
    pub replace_existing: bool,
    pub crc32_override: Option<String>,
    pub cover_target: Option<(u32, u32)>,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self {
            body_sha256: false,
            replace_existing: true,
            crc32_override: None,
            cover_target: None,
        }
    }
}

pub(crate) fn sha256(value: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(value);
    digest.finalize().into()
}

/// Calculate the RetroArch-compatible CRC32 lookup value.
pub fn crc32(value: &[u8]) -> String {
    format!("{:08x}", crc32_u32(value))
}

const fn build_crc32_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut index = 0;
    while index < table.len() {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}

const CRC32_TABLE: [u32; 256] = build_crc32_table();

fn crc32_u32(value: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in value {
        let index = ((crc ^ u32::from(*byte)) & 0xff) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[index];
    }
    crc ^ 0xffff_ffff
}

pub fn payload_sha256(value: &[u8]) -> [u8; 32] {
    sha256(value)
}

/// Normalize an explicitly supplied CRC32 lookup key.
pub fn normalize_crc32(value: &str) -> Result<String, RomxError> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RomxError::Invalid(
            "CRC32 override must be exactly 8 hexadecimal characters".into(),
        ));
    }
    Ok(value.to_ascii_lowercase())
}

/// Convert a supported cover image to PNG for an application adapter. The
/// strict ROMX writer does not call this function implicitly; it accepts only
/// already validated PNG bytes.
pub fn normalize_cover_bytes(
    value: &[u8],
    target: Option<(u32, u32)>,
) -> Result<Vec<u8>, RomxError> {
    if value.is_empty() {
        return Err(RomxError::Invalid("cover image must not be empty".into()));
    }
    if let Some((width, height)) = target {
        validate_cover_dimensions(width, height)?;
    } else if value.starts_with(PNG_SIGNATURE) {
        return Ok(value.to_vec());
    }
    let decoded = image::load_from_memory(value)?;
    let decoded = target
        .map(|(width, height)| {
            decoded.resize_exact(width, height, image::imageops::FilterType::Lanczos3)
        })
        .unwrap_or(decoded);
    let mut output = Cursor::new(Vec::new());
    decoded.write_to(&mut output, image::ImageFormat::Png)?;
    let output = output.into_inner();
    validate_png_bytes(&output)?;
    Ok(output)
}

pub fn normalize_cover_path(path: &Path, target: Option<(u32, u32)>) -> Result<Vec<u8>, RomxError> {
    normalize_cover_bytes(&fs::read(path)?, target)
}

fn validate_cover_dimensions(width: u32, height: u32) -> Result<(), RomxError> {
    if width == 0
        || height == 0
        || width > DEFAULT_MAX_COVER_DIMENSION
        || height > DEFAULT_MAX_COVER_DIMENSION
    {
        return Err(RomxError::Invalid(
            "cover resolution must be between 1 and 8192 pixels".into(),
        ));
    }
    Ok(())
}

fn png_error(message: impl Into<String>) -> RomxError {
    RomxError::Cover(message.into())
}

fn read_be32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes.try_into().expect("four-byte PNG field"))
}

fn valid_chunk_type(bytes: &[u8]) -> bool {
    bytes.len() == 4
        && bytes.iter().all(|byte| byte.is_ascii_alphabetic())
        // Reserved bit: the third type character must be uppercase.
        && bytes[2].is_ascii_uppercase()
}

fn validate_ihdr(data: &[u8]) -> Option<CoverInfo> {
    if data.len() != 13 {
        return None;
    }
    let width = read_be32(&data[0..4]);
    let height = read_be32(&data[4..8]);
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
    let valid_depth = match color {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        2 => matches!(depth, 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8),
        4 | 6 => matches!(depth, 8 | 16),
        _ => false,
    };
    valid_depth.then_some(CoverInfo { width, height })
}

/// Validate the structural PNG profile used by ROMX 1.0. Pixels are not
/// decoded; chunk boundaries, CRCs, ordering, IHDR fields and limits are.
pub fn validate_png_bytes(value: &[u8]) -> Result<CoverInfo, RomxError> {
    if value.len() > DEFAULT_MAX_COVER_SIZE as usize {
        return Err(png_error("cover exceeds the 32 MiB size limit"));
    }
    if value.len() < PNG_SIGNATURE.len() || &value[..8] != PNG_SIGNATURE {
        return Err(png_error("cover has an invalid PNG signature"));
    }
    let mut position = 8usize;
    let mut first = true;
    let mut saw_idat = false;
    let mut ended_idat = false;
    let mut saw_iend = false;
    let mut saw_plte = false;
    let mut color_type = 0u8;
    let mut cover_info = None;

    while position < value.len() {
        if value.len() - position < 12 {
            return Err(png_error("PNG chunk is truncated"));
        }
        let length = read_be32(&value[position..position + 4]) as usize;
        let chunk_type = &value[position + 4..position + 8];
        if !valid_chunk_type(chunk_type) || length > value.len() - position - 12 {
            return Err(png_error("PNG chunk is malformed or out of range"));
        }
        let data_start = position + 8;
        let data_end = data_start + length;
        let crc_start = data_end;
        let mut crc_input = Vec::with_capacity(4 + length);
        crc_input.extend_from_slice(chunk_type);
        crc_input.extend_from_slice(&value[data_start..data_end]);
        let expected_crc = crc32_u32(&crc_input);
        let stored_crc = read_be32(&value[crc_start..crc_start + 4]);
        if expected_crc != stored_crc {
            return Err(png_error("PNG chunk CRC mismatch"));
        }

        if first && (length != 13 || chunk_type != b"IHDR") {
            return Err(png_error("PNG IHDR must be the first chunk"));
        }
        if !first
            // PNG criticality is encoded by the first chunk-type byte.
            // The third byte is the reserved bit and must remain uppercase
            // for every conforming PNG chunk, including ancillary chunks such
            // as iTXt/tEXt metadata.
            && chunk_type[0].is_ascii_uppercase()
            && chunk_type != b"PLTE"
            && chunk_type != b"IDAT"
            && chunk_type != b"IEND"
        {
            return Err(png_error("PNG contains an unknown critical chunk"));
        }

        if first {
            let info = validate_ihdr(&value[data_start..data_end])
                .ok_or_else(|| png_error("PNG IHDR fields are invalid"))?;
            color_type = value[data_start + 9];
            cover_info = Some(info);
            first = false;
        } else if chunk_type == b"IHDR" {
            return Err(png_error("PNG contains multiple IHDR chunks"));
        } else if chunk_type == b"PLTE" {
            if saw_plte
                || saw_idat
                || color_type == 0
                || color_type == 4
                || length == 0
                || !length.is_multiple_of(3)
                || length > 768
            {
                return Err(png_error("PNG PLTE chunk is invalid"));
            }
            saw_plte = true;
        } else if chunk_type == b"IDAT" {
            if ended_idat {
                return Err(png_error("PNG IDAT chunks are not consecutive"));
            }
            saw_idat = true;
        } else if chunk_type == b"IEND" {
            if length != 0 || !saw_idat || (color_type == 3 && !saw_plte) {
                return Err(png_error("PNG IEND or required chunks are invalid"));
            }
            saw_iend = true;
        } else if saw_idat {
            ended_idat = true;
        }

        position = crc_start + 4;
        if saw_iend {
            break;
        }
    }

    if first || !saw_iend || position != value.len() {
        return Err(png_error("PNG is missing IEND or has trailing bytes"));
    }
    cover_info.ok_or_else(|| png_error("PNG has no IHDR"))
}

/// Classify a Game Boy payload using the CGB flag at ROM header offset 0x143.
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
        0xC0 => Ok("gbc"),
        0x80 => explicit(),
        _ => explicit(),
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn bytes32(bytes: &[u8]) -> [u8; 32] {
    bytes.try_into().expect("SHA-256 fields are 32 bytes")
}

impl Footer {
    pub fn encode(&self) -> [u8; FOOTER_SIZE] {
        let mut output = [0u8; FOOTER_SIZE];
        output[0..4].copy_from_slice(MAGIC);
        write_u32(&mut output, 0x04, self.version);
        write_u64(&mut output, 0x08, self.rom.offset);
        write_u64(&mut output, 0x10, self.rom.size);
        write_u64(&mut output, 0x18, self.metadata.offset);
        write_u64(&mut output, 0x20, self.metadata.size);
        write_u64(&mut output, 0x28, self.cover.offset);
        write_u64(&mut output, 0x30, self.cover.size);
        output[0x38..0x58].copy_from_slice(&self.reserved);
        write_u32(&mut output, 0x58, self.flags);
        write_u32(&mut output, 0x5c, FOOTER_SIZE as u32);
        output[0x60..0x80].copy_from_slice(&self.body_sha256);
        output
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, RomxError> {
        if bytes.len() != FOOTER_SIZE {
            return Err(RomxError::Invalid(
                "footer must be exactly 128 bytes".into(),
            ));
        }
        if &bytes[0..4] != MAGIC {
            return Err(RomxError::Invalid("footer magic is not ROMX".into()));
        }
        let version = read_u32(bytes, 0x04);
        let footer_size = read_u32(bytes, 0x5c);
        if version != VERSION || footer_size != FOOTER_SIZE as u32 {
            return Err(RomxError::Invalid(
                "unsupported version or footer_size".into(),
            ));
        }
        let flags = read_u32(bytes, 0x58);
        if flags & !FLAGS_V1_MASK != 0 {
            return Err(RomxError::Invalid("reserved footer flags are set".into()));
        }
        let body_sha256 = bytes32(&bytes[0x60..0x80]);
        if flags & FLAG_BODY_SHA256 == 0 && body_sha256 != [0; 32] {
            return Err(RomxError::Invalid(
                "body_sha256 must be zero when body hashing is disabled".into(),
            ));
        }
        let metadata_size = read_u64(bytes, 0x20);
        let cover_size = read_u64(bytes, 0x30);
        if (flags & FLAG_METADATA != 0) != (metadata_size != 0)
            || (flags & FLAG_COVER != 0) != (cover_size != 0)
        {
            return Err(RomxError::Invalid(
                "footer flags do not match optional region sizes".into(),
            ));
        }
        Ok(Self {
            version,
            rom: Region {
                offset: read_u64(bytes, 0x08),
                size: read_u64(bytes, 0x10),
            },
            metadata: Region {
                offset: if metadata_size == 0 {
                    0
                } else {
                    read_u64(bytes, 0x18)
                },
                size: metadata_size,
            },
            cover: Region {
                offset: if cover_size == 0 {
                    0
                } else {
                    read_u64(bytes, 0x28)
                },
                size: cover_size,
            },
            reserved: bytes32(&bytes[0x38..0x58]),
            flags,
            body_sha256,
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

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON value without duplicate object keys")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue(Value::Null))
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue(Value::Bool(value)))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue(Value::Number(value.into())))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue(Value::Number(value.into())))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Number::from_f64(value)
                    .map(|number| StrictValue(Value::Number(number)))
                    .ok_or_else(|| E::custom("JSON number is not finite"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue(Value::String(value.to_owned())))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                Ok(StrictValue(Value::String(value)))
            }

            fn visit_seq<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = access.next_element::<StrictValue>()? {
                    values.push(value.0);
                }
                Ok(StrictValue(Value::Array(values)))
            }

            fn visit_map<A>(self, mut access: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values = Map::new();
                while let Some(key) = access.next_key::<String>()? {
                    if values.contains_key(&key) {
                        return Err(de::Error::custom(format!(
                            "duplicate JSON object key: {key}"
                        )));
                    }
                    values.insert(key, access.next_value::<StrictValue>()?.0);
                }
                Ok(StrictValue(Value::Object(values)))
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
    std::str::from_utf8(bytes)
        .map_err(|_| RomxError::Metadata("metadata contains invalid UTF-8".into()))?;
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = StrictValue::deserialize(&mut deserializer)
        .map_err(|error| RomxError::Metadata(format!("metadata contains invalid JSON: {error}")))?
        .0;
    deserializer
        .end()
        .map_err(|error| RomxError::Metadata(format!("metadata contains invalid JSON: {error}")))?;
    Ok(value)
}

fn value_string<'a>(object: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    object.get(key).and_then(Value::as_str)
}

fn string_len(value: &str) -> usize {
    value.chars().count()
}

fn valid_hex_lower(value: &str) -> bool {
    value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_string_array(value: &Value, max_items: usize, max_len: usize) -> bool {
    let Some(values) = value.as_array() else {
        return false;
    };
    if values.len() > max_items {
        return false;
    }
    let mut strings = Vec::with_capacity(values.len());
    for value in values {
        let Some(value) = value.as_str() else {
            return false;
        };
        if string_len(value) > max_len || strings.contains(&value) {
            return false;
        }
        strings.push(value);
    }
    true
}

fn validate_release_date(value: &str) -> bool {
    matches!(value.len(), 4 | 7 | 10)
        && value.bytes().enumerate().all(|(index, byte)| {
            if index == 4 || index == 7 {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        })
}

fn validate_cover_descriptor(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    let Some(mime) = value_string(object, "mime_type") else {
        return false;
    };
    if mime != "image/png" {
        return false;
    }
    for (key, value) in object {
        match key.as_str() {
            "mime_type" => {}
            "width" | "height" => {
                if value.as_u64().is_none_or(|dimension| {
                    dimension == 0 || dimension > DEFAULT_MAX_COVER_DIMENSION as u64
                }) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn validate_metadata_object(
    object: &Map<String, Value>,
    require_crc32: bool,
) -> Result<(), RomxError> {
    const PLATFORMS: &[&str] = &["gb", "gbc", "gba", "nes", "snes", "nds", "3ds", "genesis"];
    const FORMATS: &[&str] = &[
        "gb", "gbc", "gba", "nes", "fds", "sfc", "smc", "nds", "3ds", "cci", "cia", "md", "gen",
        "smd", "bin",
    ];
    const DUMP_STATUS: &[&str] = &[
        "unknown",
        "good",
        "bad",
        "overdump",
        "hack",
        "translation",
        "homebrew",
    ];
    const ALLOWED: &[&str] = &[
        "schema_version",
        "name",
        "platform",
        "payload_format",
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

    for key in object.keys() {
        if !ALLOWED.contains(&key.as_str()) {
            return Err(RomxError::Metadata(format!(
                "unknown metadata field: {key}"
            )));
        }
    }
    if value_string(object, "schema_version") != Some(SPEC_VERSION) {
        return Err(RomxError::Metadata("schema_version must be 1.0".into()));
    }
    let name = value_string(object, "name")
        .ok_or_else(|| RomxError::Metadata("metadata missing required field: name".into()))?;
    if name.is_empty() || string_len(name) > 512 {
        return Err(RomxError::Metadata(
            "name must contain 1..512 characters".into(),
        ));
    }
    if !PLATFORMS.contains(&value_string(object, "platform").unwrap_or_default()) {
        return Err(RomxError::Metadata("unsupported metadata platform".into()));
    }
    if !FORMATS.contains(&value_string(object, "payload_format").unwrap_or_default()) {
        return Err(RomxError::Metadata(
            "unsupported metadata payload_format".into(),
        ));
    }
    if require_crc32 && !valid_hex_lower(value_string(object, "crc32").unwrap_or_default()) {
        return Err(RomxError::Metadata(
            "crc32 must be exactly eight lowercase hexadecimal characters".into(),
        ));
    }
    for key in ["crc32", "origin_crc32"] {
        if let Some(value) = value_string(object, key) {
            if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(RomxError::Metadata(format!(
                    "{key} must be exactly eight hexadecimal characters"
                )));
            }
            if require_crc32 && value != value.to_ascii_lowercase() {
                return Err(RomxError::Metadata(format!("{key} must be lowercase")));
            }
        }
    }
    for (key, max_len) in [
        ("serial", 128),
        ("origin", 128),
        ("category", 128),
        ("developer", 256),
        ("publisher", 256),
        ("franchise", 256),
        ("language", 256),
        ("enhancement_hw", 256),
        ("media", 64),
        ("description", 32768),
    ] {
        if let Some(value) = object.get(key) {
            let value = value.as_str().ok_or_else(|| {
                RomxError::Metadata(format!("metadata field {key} must be a string"))
            })?;
            if string_len(value) > max_len {
                return Err(RomxError::Metadata(format!(
                    "metadata field {key} is too long"
                )));
            }
        }
    }
    if let Some(value) = object.get("release_date") {
        let value = value
            .as_str()
            .ok_or_else(|| RomxError::Metadata("release_date must be a string".into()))?;
        if !validate_release_date(value) {
            return Err(RomxError::Metadata(
                "release_date has an invalid format".into(),
            ));
        }
    }
    if let Some(value) = object.get("genre") {
        if !validate_string_array(value, 32, 64) {
            return Err(RomxError::Metadata(
                "genre must be a unique string array".into(),
            ));
        }
    }
    if let Some(value) = object.get("region") {
        if !validate_string_array(value, 32, 32) {
            return Err(RomxError::Metadata(
                "region must be a unique string array".into(),
            ));
        }
    }
    if let Some(value) = object.get("users") {
        if value
            .as_u64()
            .is_none_or(|users| !(1..=255).contains(&users))
        {
            return Err(RomxError::Metadata(
                "users must be an integer from 1 to 255".into(),
            ));
        }
    }
    for key in ["coop", "rumble", "analog"] {
        if let Some(value) = object.get(key) {
            if !value.is_boolean() {
                return Err(RomxError::Metadata(format!("{key} must be boolean")));
            }
        }
    }
    if let Some(value) = object.get("dump_status") {
        if !DUMP_STATUS.contains(&value.as_str().unwrap_or_default()) {
            return Err(RomxError::Metadata("dump_status is not supported".into()));
        }
    }
    if let Some(value) = object.get("cover") {
        if !validate_cover_descriptor(value) {
            return Err(RomxError::Metadata("cover descriptor is invalid".into()));
        }
    }
    Ok(())
}

/// Validate a final ROMX metadata object. The writer accepts a template with
/// no `crc32`; readers and this public validator require the final form.
pub fn validate_metadata(metadata: &Value) -> Result<(), RomxError> {
    let object = metadata
        .as_object()
        .ok_or_else(|| RomxError::Metadata("metadata top level must be an object".into()))?;
    validate_metadata_object(object, true)
}

fn validate_metadata_template(metadata: &Value) -> Result<(), RomxError> {
    let object = metadata
        .as_object()
        .ok_or_else(|| RomxError::Metadata("metadata top level must be an object".into()))?;
    validate_metadata_object(object, false)
}

fn canonical_metadata_with_crc(
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    computed_crc32: &str,
    crc32_override: Option<&str>,
) -> Result<Option<Vec<u8>>, RomxError> {
    let Some(metadata) = metadata else {
        if crc32_override.is_some() {
            return Err(RomxError::Invalid(
                "custom CRC32 requires a metadata object".into(),
            ));
        }
        return Ok(None);
    };
    validate_metadata_template(metadata)?;
    let mut value = metadata.clone();
    let object = value
        .as_object_mut()
        .expect("metadata template was validated as an object");
    let lookup = crc32_override
        .map(normalize_crc32)
        .transpose()?
        .unwrap_or_else(|| computed_crc32.to_owned());
    object.insert("crc32".into(), Value::String(lookup));
    if object.contains_key("origin_crc32") {
        object.insert(
            "origin_crc32".into(),
            Value::String(computed_crc32.to_owned()),
        );
    }
    if let Some(cover) = cover {
        let info = validate_png_bytes(cover)?;
        let mut descriptor = Map::new();
        descriptor.insert("mime_type".into(), Value::String("image/png".into()));
        descriptor.insert("width".into(), Value::from(info.width));
        descriptor.insert("height".into(), Value::from(info.height));
        object.insert("cover".into(), Value::Object(descriptor));
    }
    let bytes = serde_json::to_vec(&value)?;
    if bytes.len() as u64 > DEFAULT_MAX_METADATA_SIZE {
        return Err(RomxError::Metadata(
            "metadata exceeds the 1 MiB limit".into(),
        ));
    }
    validate_metadata(&value)?;
    Ok(Some(bytes))
}

fn validate_regions(footer: &Footer, body_len: u64) -> Result<(), RomxError> {
    if footer.rom.size == 0 {
        return Err(RomxError::Invalid("ROM payload must not be empty".into()));
    }
    let regions = [
        ("rom", footer.rom),
        ("metadata", footer.metadata),
        ("cover", footer.cover),
    ];
    let mut present = Vec::new();
    for (name, region) in regions {
        if region.size == 0 {
            continue;
        }
        let end = region.end()?;
        if end > body_len {
            return Err(RomxError::Invalid(format!(
                "{name} region exceeds container body"
            )));
        }
        present.push((name, region.offset, end));
    }
    present.sort_by_key(|region| region.1);
    for pair in present.windows(2) {
        if pair[0].2 > pair[1].1 {
            return Err(RomxError::Invalid(format!(
                "regions overlap: {} and {}",
                pair[0].0, pair[1].0
            )));
        }
    }
    let mut cursor = 0u64;
    for (_, start, end) in present {
        if start != cursor {
            return Err(RomxError::Invalid(
                "footer body contains uncovered bytes".into(),
            ));
        }
        cursor = end;
    }
    if cursor != body_len {
        return Err(RomxError::Invalid(
            "footer body contains uncovered bytes".into(),
        ));
    }
    Ok(())
}

fn open_container(bytes: &[u8]) -> Result<(Footer, &[u8]), RomxError> {
    if bytes.len() < FOOTER_SIZE {
        return Err(RomxError::Invalid("file is shorter than footer".into()));
    }
    let body_len = bytes.len() - FOOTER_SIZE;
    let footer = Footer::decode(&bytes[body_len..])?;
    validate_regions(&footer, body_len as u64)?;
    Ok((footer, &bytes[..body_len]))
}

fn slice_region<'a>(body: &'a [u8], name: &str, region: Region) -> Result<&'a [u8], RomxError> {
    let end = region.end()?;
    if end > body.len() as u64 || region.offset > usize::MAX as u64 {
        return Err(RomxError::Invalid(format!(
            "{name} region exceeds container body"
        )));
    }
    Ok(&body[region.offset as usize..end as usize])
}

fn parse_optional_metadata(bytes: &[u8]) -> Result<Value, RomxError> {
    if bytes.len() as u64 > DEFAULT_MAX_METADATA_SIZE {
        return Err(RomxError::Metadata(
            "metadata exceeds the 1 MiB size limit".into(),
        ));
    }
    let value = parse_json_strict(bytes)?;
    validate_metadata(&value)?;
    Ok(value)
}

fn parse_optional_cover(bytes: &[u8]) -> Result<Vec<u8>, RomxError> {
    validate_png_bytes(bytes)?;
    Ok(bytes.to_vec())
}

/// Validate all components. Invalid optional metadata/cover is retained in
/// the report while the function remains successful; body hash failures are
/// container integrity failures and return an error.
pub fn validate_bytes(bytes: &[u8]) -> Result<ValidationReport, RomxError> {
    let (footer, body) = open_container(bytes)?;
    let rom = slice_region(body, "rom", footer.rom)?;
    let mut report = ValidationReport {
        structure: ValidationStatus::Valid,
        payload_hashes: ValidationStatus::Valid,
        computed_payload_crc32: Some(crc32(rom)),
        computed_payload_sha256: sha256(rom),
        ..Default::default()
    };
    let mut body_hash_invalid = false;
    if footer.flags & FLAG_BODY_SHA256 != 0 {
        report.computed_body_sha256 = sha256(body);
        if report.computed_body_sha256 == footer.body_sha256 {
            report.body_sha256 = ValidationStatus::Valid;
        } else {
            report.body_sha256 = ValidationStatus::Invalid;
            body_hash_invalid = true;
        }
    } else {
        report.body_sha256 = ValidationStatus::Absent;
    }
    if footer.metadata.size == 0 {
        report.metadata = ValidationStatus::Absent;
        report.metadata_crc32 = Crc32Status::Absent;
    } else {
        match parse_optional_metadata(slice_region(body, "metadata", footer.metadata)?) {
            Ok(metadata) => {
                report.metadata = ValidationStatus::Valid;
                report.metadata_crc32 = match value_string(
                    metadata.as_object().expect("validated metadata object"),
                    "crc32",
                ) {
                    Some(value) if valid_hex_lower(value) => Crc32Status::ValidLookup,
                    _ => Crc32Status::Invalid,
                };
            }
            Err(error) => {
                report.metadata = ValidationStatus::Invalid;
                report.metadata_crc32 = Crc32Status::Invalid;
                report.metadata_result = Some(error.to_string());
            }
        }
    }
    if footer.cover.size == 0 {
        report.cover = ValidationStatus::Absent;
        report.cover_hashes = ValidationStatus::Absent;
    } else {
        match validate_png_bytes(slice_region(body, "cover", footer.cover)?) {
            Ok(info) => {
                report.cover = ValidationStatus::Valid;
                report.cover_hashes = ValidationStatus::Valid;
                report.cover_info = Some(info);
                report.computed_cover_sha256 = sha256(slice_region(body, "cover", footer.cover)?);
            }
            Err(error) => {
                report.cover = ValidationStatus::Invalid;
                report.cover_hashes = ValidationStatus::NotChecked;
                report.cover_result = Some(error.to_string());
            }
        }
    }
    if body_hash_invalid {
        Err(RomxError::BodyHashMismatch)
    } else {
        Ok(report)
    }
}

pub fn validate_path(path: &Path) -> Result<ValidationReport, RomxError> {
    validate_bytes(&fs::read(path)?)
}

fn parse_preview_regions(footer: Footer, body: &[u8]) -> Result<RomxPreview, RomxError> {
    let metadata = if footer.metadata.size == 0 {
        None
    } else {
        parse_optional_metadata(slice_region(body, "metadata", footer.metadata)?).ok()
    };
    let cover = if footer.cover.size == 0 {
        None
    } else {
        parse_optional_cover(slice_region(body, "cover", footer.cover)?).ok()
    };
    Ok(RomxPreview {
        footer,
        metadata,
        cover,
    })
}

/// Read and integrity-check a ROMX. Optional invalid regions are salvaged as
/// absent; the raw payload and valid footer remain available.
pub fn read_bytes(bytes: &[u8]) -> Result<RomxDocument, RomxError> {
    let (footer, body) = open_container(bytes)?;
    if footer.flags & FLAG_BODY_SHA256 != 0 && sha256(body) != footer.body_sha256 {
        return Err(RomxError::BodyHashMismatch);
    }
    let rom = slice_region(body, "rom", footer.rom)?.to_vec();
    let preview = parse_preview_regions(footer, body)?;
    Ok(RomxDocument {
        footer: preview.footer,
        rom,
        metadata: preview.metadata,
        cover: preview.cover,
    })
}

pub fn read_path(path: &Path) -> Result<RomxDocument, RomxError> {
    read_bytes(&fs::read(path)?)
}

/// Read only footer/metadata/cover. This deliberately skips payload and body
/// hash validation and is suitable for large-file previews.
pub fn read_metadata_cover_bytes(bytes: &[u8]) -> Result<RomxPreview, RomxError> {
    let (footer, body) = open_container(bytes)?;
    parse_preview_regions(footer, body)
}

pub fn read_metadata_cover_path(path: &Path) -> Result<RomxPreview, RomxError> {
    let mut file = fs::File::open(path)?;
    let file_len = file.metadata()?.len();
    if file_len < FOOTER_SIZE as u64 {
        return Err(RomxError::Invalid("file is shorter than footer".into()));
    }
    let body_len = file_len - FOOTER_SIZE as u64;
    file.seek(SeekFrom::Start(body_len))?;
    let mut footer_bytes = [0u8; FOOTER_SIZE];
    file.read_exact(&mut footer_bytes)?;
    let footer = Footer::decode(&footer_bytes)?;
    validate_regions(&footer, body_len)?;

    let read_region =
        |file: &mut fs::File, region: Region, name: &str| -> Result<Option<Vec<u8>>, RomxError> {
            if region.size == 0 {
                return Ok(None);
            }
            let size = usize::try_from(region.size)
                .map_err(|_| RomxError::Invalid(format!("{name} region is too large")))?;
            file.seek(SeekFrom::Start(region.offset))?;
            let mut bytes = vec![0u8; size];
            file.read_exact(&mut bytes)?;
            Ok(Some(bytes))
        };
    let metadata_bytes = read_region(&mut file, footer.metadata, "metadata")?;
    let cover_bytes = read_region(&mut file, footer.cover, "cover")?;
    let metadata = metadata_bytes
        .as_deref()
        .and_then(|bytes| parse_optional_metadata(bytes).ok());
    let cover = cover_bytes
        .as_deref()
        .and_then(|bytes| parse_optional_cover(bytes).ok());
    Ok(RomxPreview {
        footer,
        metadata,
        cover,
    })
}

fn write_body_chunk<W: Write>(
    writer: &mut W,
    body_sha256: &mut Option<Sha256>,
    bytes: &[u8],
) -> Result<(), RomxError> {
    writer.write_all(bytes)?;
    if let Some(digest) = body_sha256 {
        digest.update(bytes);
    }
    Ok(())
}

struct Crc32Hasher {
    value: u32,
}

impl Crc32Hasher {
    fn new() -> Self {
        Self { value: 0xffff_ffff }
    }

    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            let index = ((self.value ^ u32::from(*byte)) & 0xff) as usize;
            self.value = (self.value >> 8) ^ CRC32_TABLE[index];
        }
    }

    fn finish(self) -> u32 {
        self.value ^ 0xffff_ffff
    }
}

fn write_container_stream<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    options: &PackOptions,
) -> Result<Footer, RomxError> {
    if metadata.is_none() && options.crc32_override.is_some() {
        return Err(RomxError::Invalid(
            "custom CRC32 requires a metadata object".into(),
        ));
    }
    if let Some(metadata) = metadata {
        validate_metadata_template(metadata)?;
    }
    if let Some(cover) = cover {
        validate_png_bytes(cover)?;
    }
    let mut body_sha256 = options.body_sha256.then(Sha256::new);
    let mut rom_crc32 = Crc32Hasher::new();
    let mut rom_size = 0u64;
    let mut buffer = [0u8; 1024 * 1024];
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        write_body_chunk(writer, &mut body_sha256, &buffer[..count])?;
        rom_crc32.update(&buffer[..count]);
        rom_size = rom_size
            .checked_add(count as u64)
            .ok_or_else(|| RomxError::Invalid("ROM payload size overflow".into()))?;
    }
    if rom_size == 0 {
        return Err(RomxError::Invalid("ROM payload must not be empty".into()));
    }
    let computed_crc32 = format!("{:08x}", rom_crc32.finish());
    let metadata_bytes = canonical_metadata_with_crc(
        metadata,
        cover,
        &computed_crc32,
        options.crc32_override.as_deref(),
    )?;
    let metadata_size = metadata_bytes
        .as_ref()
        .map_or(0, |bytes| bytes.len() as u64);
    let metadata_region = Region {
        offset: if metadata_bytes.is_some() {
            rom_size
        } else {
            0
        },
        size: metadata_size,
    };
    let cover_offset = rom_size
        .checked_add(metadata_size)
        .ok_or_else(|| RomxError::Invalid("ROMX body size overflow".into()))?;
    let cover_region = Region {
        offset: if cover.is_some() { cover_offset } else { 0 },
        size: cover.map_or(0, |bytes| bytes.len() as u64),
    };
    if let Some(metadata) = &metadata_bytes {
        write_body_chunk(writer, &mut body_sha256, metadata)?;
    }
    if let Some(cover) = cover {
        write_body_chunk(writer, &mut body_sha256, cover)?;
    }
    let mut flags = 0;
    if metadata_region.size != 0 {
        flags |= FLAG_METADATA;
    }
    if cover_region.size != 0 {
        flags |= FLAG_COVER;
    }
    let body_sha256 = if options.body_sha256 {
        flags |= FLAG_BODY_SHA256;
        body_sha256
            .expect("body hash state exists when body hashing is enabled")
            .finalize()
            .into()
    } else {
        [0; 32]
    };
    let footer = Footer {
        version: VERSION,
        rom: Region {
            offset: 0,
            size: rom_size,
        },
        metadata: metadata_region,
        cover: cover_region,
        reserved: [0; 32],
        flags,
        body_sha256,
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
    write_container_stream(&mut Cursor::new(rom), &mut output, metadata, cover, options)?;
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
    crc32_override: Option<&str>,
) -> Result<Vec<u8>, RomxError> {
    let options = PackOptions {
        crc32_override: crc32_override.map(str::to_owned),
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
    build_container(rom, metadata, cover, options)
}

/// Compatibility convenience API: normalize an application-supplied cover,
/// then call the strict ROMX writer.
pub fn pack_bytes_with_options(
    rom: &[u8],
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    crc32_override: Option<&str>,
    cover_target: Option<(u32, u32)>,
) -> Result<Vec<u8>, RomxError> {
    let cover = cover
        .map(|value| normalize_cover_bytes(value, cover_target))
        .transpose()?;
    let options = PackOptions {
        crc32_override: crc32_override.map(str::to_owned),
        ..Default::default()
    };
    build_container(rom, metadata, cover.as_deref(), &options)
}

fn write_atomic_stream<F>(
    path: &Path,
    replace_existing: bool,
    write_output: F,
) -> Result<(), RomxError>
where
    F: FnOnce(&mut fs::File) -> Result<(), RomxError>,
{
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() && !replace_existing {
        return Err(RomxError::Exists(path.to_owned()));
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("romx");
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(".{name}.tmp-{}-{counter}", std::process::id()));
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        if let Err(error) = write_output(&mut file) {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
        if let Err(error) = file.sync_all() {
            let _ = fs::remove_file(&temporary);
            return Err(error.into());
        }
    }
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        if path.exists() && !replace_existing {
            return Err(RomxError::Exists(path.to_owned()));
        }
        return Err(error.into());
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8], replace_existing: bool) -> Result<(), RomxError> {
    write_atomic_stream(path, replace_existing, |file| {
        file.write_all(bytes)?;
        Ok(())
    })
}

pub(crate) fn pack_path_with_metadata_options(
    rom_path: &Path,
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    output_path: &Path,
    options: &PackOptions,
) -> Result<(), RomxError> {
    write_atomic_stream(output_path, options.replace_existing, |output| {
        let mut rom = fs::File::open(rom_path)?;
        write_container_stream(&mut rom, output, metadata, cover, options)?;
        Ok(())
    })
}

pub fn pack_to_path(
    rom_path: &Path,
    metadata_path: Option<&Path>,
    cover_path: Option<&Path>,
    output_path: &Path,
) -> Result<(), RomxError> {
    pack_to_path_with_writer_options(
        rom_path,
        metadata_path,
        cover_path,
        output_path,
        &PackOptions::default(),
    )
}

pub fn pack_to_path_with_crc32(
    rom_path: &Path,
    metadata_path: Option<&Path>,
    cover_path: Option<&Path>,
    output_path: &Path,
    crc32_override: Option<&str>,
) -> Result<(), RomxError> {
    let options = PackOptions {
        crc32_override: crc32_override.map(str::to_owned),
        ..Default::default()
    };
    pack_to_path_with_writer_options(rom_path, metadata_path, cover_path, output_path, &options)
}

pub fn pack_to_path_with_writer_options(
    rom_path: &Path,
    metadata_path: Option<&Path>,
    cover_path: Option<&Path>,
    output_path: &Path,
    options: &PackOptions,
) -> Result<(), RomxError> {
    let metadata = metadata_path
        .map(|path| parse_json_strict(&fs::read(path)?))
        .transpose()?;
    let cover = cover_path.map(fs::read).transpose()?;
    pack_path_with_metadata_options(
        rom_path,
        metadata.as_ref(),
        cover.as_deref(),
        output_path,
        options,
    )
}

/// Compatibility path API: image conversion is performed before strict
/// embedding, which keeps the existing desktop and CLI cover-size workflow.
pub fn pack_to_path_with_options(
    rom_path: &Path,
    metadata_path: Option<&Path>,
    cover_path: Option<&Path>,
    output_path: &Path,
    crc32_override: Option<&str>,
    cover_target: Option<(u32, u32)>,
) -> Result<(), RomxError> {
    let metadata = metadata_path
        .map(|path| parse_json_strict(&fs::read(path)?))
        .transpose()?;
    let cover = cover_path
        .map(|path| normalize_cover_path(path, cover_target))
        .transpose()?;
    let options = PackOptions {
        crc32_override: crc32_override.map(str::to_owned),
        ..Default::default()
    };
    pack_path_with_metadata_options(
        rom_path,
        metadata.as_ref(),
        cover.as_deref(),
        output_path,
        &options,
    )
}

fn atomic_extract(path: &Path, bytes: &[u8]) -> Result<(), RomxError> {
    write_atomic(path, bytes, true)
}

pub fn extract_payload_to_path(
    romx_path: &Path,
    output_path: &Path,
    replace_existing: bool,
) -> Result<(), RomxError> {
    let document = read_path(romx_path)?;
    if output_path.exists() && !replace_existing {
        return Err(RomxError::Exists(output_path.to_owned()));
    }
    write_atomic(output_path, &document.rom, replace_existing)
}

pub fn extract_to_dir(path: &Path, output_dir: &Path) -> Result<PathBuf, RomxError> {
    let document = read_path(path)?;
    fs::create_dir_all(output_dir)?;
    let format = document
        .metadata
        .as_ref()
        .and_then(|value| value.get("payload_format"))
        .and_then(Value::as_str)
        .unwrap_or("rom");
    let payload_path = output_dir.join(format!("payload.{format}"));
    atomic_extract(&payload_path, &document.rom)?;
    if let Some(metadata) = document.metadata {
        let metadata_bytes = serde_json::to_vec_pretty(&metadata)?;
        atomic_extract(&output_dir.join("metadata.json"), &metadata_bytes)?;
    }
    if let Some(cover) = document.cover {
        atomic_extract(&output_dir.join("cover.png"), &cover)?;
    }
    Ok(payload_path)
}

/// Helper for GUI forms and LPL adapters. `crc32` is generated by the writer.
pub fn required_metadata(
    name: impl Into<String>,
    platform: impl Into<String>,
    payload_format: impl Into<String>,
) -> Value {
    let mut object = Map::new();
    object.insert("schema_version".into(), Value::String(SPEC_VERSION.into()));
    object.insert("name".into(), Value::String(name.into()));
    object.insert("platform".into(), Value::String(platform.into()));
    object.insert(
        "payload_format".into(),
        Value::String(payload_format.into()),
    );
    Value::Object(object)
}

pub fn application_version() -> &'static str {
    APP_VERSION
}
