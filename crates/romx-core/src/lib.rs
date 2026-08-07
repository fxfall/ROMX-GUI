//! ROMX 1.0 core implementation.
//!
//! The core owns the binary format: it writes unmodified ROM bytes, embedded
//! metadata and optional PNG bytes, then appends the fixed 128-byte footer.
//! It also validates and extracts ROMX files. GUI code should call this crate
//! instead of duplicating format or hash logic.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use thiserror::Error;

mod lpl;

pub use lpl::{
    export_lpl, import_lpl, plan_lpl_import, ExportLplOptions, ExportLplReport, ImportLplOptions,
    ImportLplPlan, ImportLplReport, PlannedLplItem,
};

pub const FOOTER_SIZE: usize = 128;
pub const VERSION: u32 = 1;
pub const FLAG_METADATA: u32 = 1 << 0;
pub const FLAG_COVER: u32 = 1 << 1;
pub const FLAG_BODY_SHA256: u32 = 1 << 2;
pub const PNG_SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
const MAGIC: &[u8; 4] = b"ROMX";

#[derive(Debug, Error)]
pub enum RomxError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid JSON metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid ROMX: {0}")]
    Invalid(String),
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
    pub rom_sha256: [u8; 32],
    pub flags: u32,
    pub body_sha256: [u8; 32],
}

#[derive(Debug, Clone)]
pub struct RomxDocument {
    pub footer: Footer,
    pub rom: Vec<u8>,
    pub metadata: Option<Value>,
    pub cover: Option<Vec<u8>>,
}

pub(crate) fn sha256(value: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(value);
    digest.finalize().into()
}

pub fn crc32(value: &[u8]) -> String {
    let mut crc = 0xffff_ffffu32;
    for byte in value {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    format!("{:08x}", crc ^ 0xffff_ffff)
}

/// Classify a Game Boy payload using the CGB flag at ROM header offset 0x143.
///
/// 0xC0 means GBC-only and always wins. 0x80 means dual GB/GBC compatibility;
/// in that case the caller must provide the already validated ROMX
/// `payload_format` so the format is not guessed from the header.
pub fn classify_gb_payload(rom: &[u8], payload_format: Option<&str>) -> Result<&'static str, RomxError> {
    if rom.len() <= 0x143 {
        return Err(RomxError::Invalid("GB ROM is too small to contain CGB flag".into()));
    }
    match rom[0x143] {
        0xC0 => Ok("gbc"),
        0x80 => match payload_format {
            Some("gb") => Ok("gb"),
            Some("gbc") => Ok("gbc"),
            _ => Err(RomxError::Invalid("dual GB/GBC ROM requires payload_format gb or gbc".into())),
        },
        _ => match payload_format {
            Some("gb") => Ok("gb"),
            Some("gbc") => Ok("gbc"),
            _ => Err(RomxError::Invalid("GB ROM requires payload_format gb or gbc".into())),
        },
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
        output[0x38..0x58].copy_from_slice(&self.rom_sha256);
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
            return Err(RomxError::Invalid("magic is not ROMX".into()));
        }
        let version = read_u32(bytes, 0x04);
        let footer_size = read_u32(bytes, 0x5c);
        if version != VERSION || footer_size != FOOTER_SIZE as u32 {
            return Err(RomxError::Invalid(
                "unsupported version or footer_size".into(),
            ));
        }
        let flags = read_u32(bytes, 0x58);
        if flags & !(FLAG_METADATA | FLAG_COVER | FLAG_BODY_SHA256) != 0 {
            return Err(RomxError::Invalid("reserved footer flags are set".into()));
        }
        Ok(Self {
            version,
            rom: Region {
                offset: read_u64(bytes, 0x08),
                size: read_u64(bytes, 0x10),
            },
            metadata: Region {
                offset: read_u64(bytes, 0x18),
                size: read_u64(bytes, 0x20),
            },
            cover: Region {
                offset: read_u64(bytes, 0x28),
                size: read_u64(bytes, 0x30),
            },
            rom_sha256: bytes32(&bytes[0x38..0x58]),
            flags,
            body_sha256: bytes32(&bytes[0x60..0x80]),
        })
    }
}

pub(crate) fn validate_metadata(metadata: &Value) -> Result<(), RomxError> {
    let object = metadata
        .as_object()
        .ok_or_else(|| RomxError::Invalid("metadata top level must be an object".into()))?;
    for key in ["schema_version", "label", "platform", "payload_format"] {
        if !object.contains_key(key) {
            return Err(RomxError::Invalid(format!(
                "metadata missing required field: {key}"
            )));
        }
    }
    if object.get("schema_version") != Some(&Value::String("1.0".into())) {
        return Err(RomxError::Invalid(
            "metadata schema_version must be 1.0".into(),
        ));
    }
    let label = object.get("label").and_then(Value::as_str).unwrap_or("");
    if label.is_empty() {
        return Err(RomxError::Invalid(
            "metadata label must not be empty".into(),
        ));
    }
    let platforms = ["gb", "gbc", "gba", "nes", "snes", "nds", "3ds", "genesis"];
    let formats = [
        "gb", "gbc", "gba", "nes", "fds", "sfc", "smc", "nds", "3ds", "cci", "cia", "md", "gen",
        "smd", "bin",
    ];
    if !platforms.contains(&object.get("platform").and_then(Value::as_str).unwrap_or("")) {
        return Err(RomxError::Invalid("unsupported metadata platform".into()));
    }
    if !formats.contains(
        &object
            .get("payload_format")
            .and_then(Value::as_str)
            .unwrap_or(""),
    ) {
        return Err(RomxError::Invalid(
            "unsupported metadata payload_format".into(),
        ));
    }
    if let Some(cover) = object.get("cover") {
        if cover.get("mime_type").and_then(Value::as_str) != Some("image/png") {
            return Err(RomxError::Invalid(
                "cover.mime_type must be image/png".into(),
            ));
        }
    }
    Ok(())
}

fn slice_region<'a>(body: &'a [u8], name: &str, region: Region) -> Result<&'a [u8], RomxError> {
    let end = region.end()?;
    if end > body.len() as u64 {
        return Err(RomxError::Invalid(format!("{name} region exceeds body")));
    }
    Ok(&body[region.offset as usize..end as usize])
}

fn validate_regions(footer: &Footer, body_len: usize) -> Result<(), RomxError> {
    if footer.rom.size == 0 {
        return Err(RomxError::Invalid("ROM payload must not be empty".into()));
    }
    let regions = [
        ("rom", footer.rom),
        ("metadata", footer.metadata),
        ("cover", footer.cover),
    ];
    let mut nonempty = Vec::new();
    for (name, region) in regions {
        if region.size == 0 {
            continue;
        }
        let end = region.end()?;
        if end > body_len as u64 {
            return Err(RomxError::Invalid(format!("{name} region reaches footer")));
        }
        nonempty.push((name, region.offset, end));
    }
    for (index, first) in nonempty.iter().enumerate() {
        for second in nonempty.iter().skip(index + 1) {
            if first.1 < second.2 && second.1 < first.2 {
                return Err(RomxError::Invalid(format!(
                    "regions overlap: {} and {}",
                    first.0, second.0
                )));
            }
        }
    }
    if (footer.metadata.size > 0) != (footer.flags & FLAG_METADATA != 0)
        || (footer.cover.size > 0) != (footer.flags & FLAG_COVER != 0)
    {
        return Err(RomxError::Invalid(
            "footer flags do not match region sizes".into(),
        ));
    }
    Ok(())
}

pub fn pack_bytes(
    rom: &[u8],
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
) -> Result<Vec<u8>, RomxError> {
    if rom.is_empty() {
        return Err(RomxError::Invalid("ROM payload must not be empty".into()));
    }
    let metadata_bytes = if let Some(value) = metadata {
        validate_metadata(value)?;
        Some(serde_json::to_vec(value)?)
    } else {
        None
    };
    if let Some(cover_bytes) = cover {
        if !cover_bytes.starts_with(PNG_SIGNATURE) {
            return Err(RomxError::Invalid("cover is not a PNG".into()));
        }
    }
    let rom_region = Region {
        offset: 0,
        size: rom.len() as u64,
    };
    let metadata_region = Region {
        offset: metadata_bytes
            .as_ref()
            .map(|_| rom.len() as u64)
            .unwrap_or(0),
        size: metadata_bytes.as_ref().map(|v| v.len() as u64).unwrap_or(0),
    };
    let cover_offset = if cover.is_some() {
        if metadata_region.size > 0 {
            metadata_region.offset + metadata_region.size
        } else {
            rom_region.offset + rom_region.size
        }
    } else {
        0
    };
    let cover_region = Region {
        offset: cover_offset,
        size: cover.map(|v| v.len() as u64).unwrap_or(0),
    };
    let mut body =
        Vec::with_capacity(rom.len() + metadata_region.size as usize + cover_region.size as usize);
    body.extend_from_slice(rom);
    if let Some(bytes) = &metadata_bytes {
        body.extend_from_slice(bytes);
    }
    if let Some(bytes) = cover {
        body.extend_from_slice(bytes);
    }
    let mut flags = FLAG_BODY_SHA256;
    if metadata_region.size > 0 {
        flags |= FLAG_METADATA;
    }
    if cover_region.size > 0 {
        flags |= FLAG_COVER;
    }
    let footer = Footer {
        version: VERSION,
        rom: rom_region,
        metadata: metadata_region,
        cover: cover_region,
        rom_sha256: sha256(rom),
        flags,
        body_sha256: sha256(&body),
    };
    body.extend_from_slice(&footer.encode());
    Ok(body)
}

pub fn pack_to_path(
    rom_path: &Path,
    metadata_path: Option<&Path>,
    cover_path: Option<&Path>,
    output_path: &Path,
) -> Result<(), RomxError> {
    let rom = fs::read(rom_path)?;
    let metadata = if let Some(path) = metadata_path {
        Some(serde_json::from_slice::<Value>(&fs::read(path)?)?)
    } else {
        None
    };
    let cover = if let Some(path) = cover_path {
        Some(fs::read(path)?)
    } else {
        None
    };
    let bytes = pack_bytes(&rom, metadata.as_ref(), cover.as_deref())?;
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(output_path, bytes)?;
    Ok(())
}

pub fn read_bytes(bytes: &[u8]) -> Result<RomxDocument, RomxError> {
    if bytes.len() < FOOTER_SIZE {
        return Err(RomxError::Invalid("file is shorter than footer".into()));
    }
    let body_len = bytes.len() - FOOTER_SIZE;
    let footer = Footer::decode(&bytes[body_len..])?;
    validate_regions(&footer, body_len)?;
    let body = &bytes[..body_len];
    let rom = slice_region(body, "rom", footer.rom)?.to_vec();
    if sha256(&rom) != footer.rom_sha256 {
        return Err(RomxError::Invalid("ROM SHA-256 mismatch".into()));
    }
    if footer.flags & FLAG_BODY_SHA256 != 0 && sha256(body) != footer.body_sha256 {
        return Err(RomxError::Invalid("body SHA-256 mismatch".into()));
    }
    let metadata = if footer.metadata.size > 0 {
        let value: Value =
            serde_json::from_slice(slice_region(body, "metadata", footer.metadata)?)?;
        validate_metadata(&value)?;
        Some(value)
    } else {
        None
    };
    let cover = if footer.cover.size > 0 {
        let value = slice_region(body, "cover", footer.cover)?;
        if !value.starts_with(PNG_SIGNATURE) {
            return Err(RomxError::Invalid("embedded cover is not PNG".into()));
        }
        Some(value.to_vec())
    } else {
        None
    };
    Ok(RomxDocument {
        footer,
        rom,
        metadata,
        cover,
    })
}

pub fn read_path(path: &Path) -> Result<RomxDocument, RomxError> {
    read_bytes(&fs::read(path)?)
}

pub fn extract_to_dir(path: &Path, output_dir: &Path) -> Result<PathBuf, RomxError> {
    let document = read_path(path)?;
    fs::create_dir_all(output_dir)?;
    let format = document
        .metadata
        .as_ref()
        .and_then(|v| v.get("payload_format"))
        .and_then(Value::as_str)
        .unwrap_or("rom");
    let payload_path = output_dir.join(format!("payload.{format}"));
    fs::write(&payload_path, &document.rom)?;
    if let Some(metadata) = document.metadata {
        fs::write(
            output_dir.join("metadata.json"),
            serde_json::to_vec_pretty(&metadata)?,
        )?;
    }
    if let Some(cover) = document.cover {
        fs::write(output_dir.join("cover.png"), cover)?;
    }
    Ok(payload_path)
}

/// Helper for GUI forms: create the required metadata object with an optional
/// cover description. Additional form fields can be inserted by the caller.
pub fn required_metadata(
    label: impl Into<String>,
    platform: impl Into<String>,
    payload_format: impl Into<String>,
) -> Value {
    let mut object = Map::new();
    object.insert("schema_version".into(), Value::String("1.0".into()));
    object.insert("label".into(), Value::String(label.into()));
    object.insert("platform".into(), Value::String(platform.into()));
    object.insert(
        "payload_format".into(),
        Value::String(payload_format.into()),
    );
    Value::Object(object)
}
