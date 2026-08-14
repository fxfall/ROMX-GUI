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
use std::io::{self, Cursor, Read, Write};
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
                    || length % 3 != 0
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

fn make_empty_mutable(capacity: u64) -> Result<Vec<u8>, RomxError> {
    if capacity == 0 {
        return Ok(Vec::new());
    }
    if capacity % 4096 != 0 || capacity < MIN_MUTABLE_CAPACITY {
        return Err(RomxError::Invalid(
            "mutable capacity must be a 4096-byte multiple and at least 12288".into(),
        ));
    }
    const HEADER: usize = 4096;
    const ENTRY: usize = 512;
    const COUNT: usize = 8;
    let directory = ENTRY * COUNT;
    let data_offset = HEADER + directory;
    let mut header = vec![0u8; HEADER];
    header[..4].copy_from_slice(b"RMUT");
    put_u16(&mut header, 4, 1);
    put_u16(&mut header, 6, HEADER as u16);
    put_u32(&mut header, 8, ENTRY as u32);
    put_u32(&mut header, 12, COUNT as u32);
    put_u64(&mut header, 16, HEADER as u64);
    put_u64(&mut header, 24, directory as u64);
    put_u64(&mut header, 32, data_offset as u64);
    put_u64(&mut header, 40, capacity - data_offset as u64);
    put_u32(&mut header, 0x34, 0);
    let checksum = crc32_u32(&header);
    put_u32(&mut header, 0x34, checksum);
    let mut output = vec![0u8; capacity as usize];
    output[..HEADER].copy_from_slice(&header);
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
    let mutable = make_empty_mutable(options.mutable_capacity)?;
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
            options,
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
pub fn read_metadata_cover_path(path: &Path) -> Result<RomxPreview, RomxError> {
    read_metadata_cover_bytes(&fs::read(path)?)
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
