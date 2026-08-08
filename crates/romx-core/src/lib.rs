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
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use thiserror::Error;

mod lpl;

pub use lpl::{
    export_lpl, export_lpl_with_output_handling, import_lpl, import_lpl_with_error_handling,
    import_lpl_with_output_handling, import_lpl_with_progress, plan_lpl_import, ExportLplOptions,
    ExportLplReport, ImportLplOptions, ImportLplPlan, ImportLplReport, PlannedLplItem,
};

pub const FOOTER_SIZE: usize = 128;
pub const VERSION: u32 = 1;
/// Current application release version, shared by Core, CLI, and GUI.
pub const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Return the current application release version.
pub const fn application_version() -> &'static str {
    APP_VERSION
}
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
    #[error("image processing error: {0}")]
    Image(#[from] image::ImageError),
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

/// Lightweight ROMX data for previews and metadata editing.
///
/// Unlike [`RomxDocument`], this type does not load or hash the ROM payload.
/// It reads only the footer, metadata region, and optional cover region.
#[derive(Debug, Clone)]
pub struct RomxPreview {
    pub footer: Footer,
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

/// Normalize an explicitly supplied CRC32 lookup key.
///
/// CRC32 is represented in ROMX metadata as exactly eight hexadecimal
/// characters.  Uppercase input is accepted at the API/CLI boundary and is
/// stored in the canonical lowercase form.
pub fn normalize_crc32(value: &str) -> Result<String, RomxError> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RomxError::Invalid(
            "CRC32 override must be exactly 8 hexadecimal characters".into(),
        ));
    }
    Ok(value.to_ascii_lowercase())
}

/// Convert a supported cover image to PNG.
///
/// With no target size, an existing PNG is returned byte-for-byte unchanged;
/// other supported formats are decoded at their original dimensions and
/// encoded as PNG. When a target is supplied, every format is resized exactly
/// to that width and height before PNG encoding. Animated GIFs use their first
/// frame, matching the single-cover ROMX model.
pub fn normalize_cover_bytes(
    value: &[u8],
    target: Option<(u32, u32)>,
) -> Result<Vec<u8>, RomxError> {
    if value.is_empty() {
        return Err(RomxError::Invalid("cover image must not be empty".into()));
    }
    if let Some((width, height)) = target {
        if width == 0 || height == 0 || width > 8192 || height > 8192 {
            return Err(RomxError::Invalid(
                "cover resolution must be between 1 and 8192 pixels".into(),
            ));
        }
    } else if value.starts_with(PNG_SIGNATURE) {
        return Ok(value.to_vec());
    }
    let image = image::load_from_memory(value)?;
    let image = target
        .map(|(width, height)| {
            image.resize_exact(width, height, image::imageops::FilterType::Lanczos3)
        })
        .unwrap_or(image);
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, image::ImageFormat::Png)?;
    Ok(output.into_inner())
}

pub fn normalize_cover_path(path: &Path, target: Option<(u32, u32)>) -> Result<Vec<u8>, RomxError> {
    normalize_cover_bytes(&fs::read(path)?, target)
}

fn png_dimensions(value: &[u8]) -> Option<(u32, u32)> {
    if value.len() >= 24 && value.starts_with(PNG_SIGNATURE) && &value[12..16] == b"IHDR" {
        Some((
            u32::from_be_bytes(value[16..20].try_into().ok()?),
            u32::from_be_bytes(value[20..24].try_into().ok()?),
        ))
    } else {
        None
    }
}

fn hex_digest(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Classify a Game Boy payload using the CGB flag at ROM header offset 0x143.
///
/// 0xC0 means GBC-only and always wins. 0x80 means dual GB/GBC compatibility;
/// in that case the caller must provide the already validated ROMX
/// `payload_format` so the format is not guessed from the header.
pub fn classify_gb_payload(
    rom: &[u8],
    payload_format: Option<&str>,
) -> Result<&'static str, RomxError> {
    if rom.len() <= 0x143 {
        return match payload_format {
            Some("gb") => Ok("gb"),
            Some("gbc") => Ok("gbc"),
            _ => Err(RomxError::Invalid(
                "GB ROM is too small to classify without payload_format gb or gbc".into(),
            )),
        };
    }
    match rom[0x143] {
        0xC0 => Ok("gbc"),
        0x80 => match payload_format {
            Some("gb") => Ok("gb"),
            Some("gbc") => Ok("gbc"),
            _ => Err(RomxError::Invalid(
                "dual GB/GBC ROM requires payload_format gb or gbc".into(),
            )),
        },
        _ => match payload_format {
            Some("gb") => Ok("gb"),
            Some("gbc") => Ok("gbc"),
            _ => Err(RomxError::Invalid(
                "GB ROM requires payload_format gb or gbc".into(),
            )),
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

fn validate_regions(footer: &Footer, body_len: u64) -> Result<(), RomxError> {
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
        if end > body_len {
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
    pack_bytes_with_options(rom, metadata, cover, None, None)
}

/// Pack a ROMX container, optionally overriding the metadata CRC32 lookup key.
///
/// By default the key is always regenerated from the unchanged ROM bytes.  An
/// explicit override is useful when matching a database that intentionally
/// publishes a different CRC32 identity.  The footer SHA-256 is never
/// overridden and always describes the actual ROM payload.
pub fn pack_bytes_with_crc32(
    rom: &[u8],
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    crc32_override: Option<&str>,
) -> Result<Vec<u8>, RomxError> {
    pack_bytes_with_options(rom, metadata, cover, crc32_override, None)
}

/// Pack a ROMX container with optional CRC32 and cover normalization options.
pub fn pack_bytes_with_options(
    rom: &[u8],
    metadata: Option<&Value>,
    cover: Option<&[u8]>,
    crc32_override: Option<&str>,
    cover_target: Option<(u32, u32)>,
) -> Result<Vec<u8>, RomxError> {
    if rom.is_empty() {
        return Err(RomxError::Invalid("ROM payload must not be empty".into()));
    }
    let mut metadata_value = if let Some(value) = metadata {
        let mut value = value.clone();
        validate_metadata(&value)?;
        let lookup_crc = crc32_override
            .map(normalize_crc32)
            .transpose()?
            .unwrap_or_else(|| crc32(rom));
        value
            .as_object_mut()
            .expect("validated metadata is an object")
            .insert("crc32".into(), Value::String(lookup_crc));
        Some(value)
    } else if crc32_override.is_some() {
        return Err(RomxError::Invalid(
            "custom CRC32 requires a metadata object".into(),
        ));
    } else {
        None
    };
    let cover_bytes = cover
        .map(|value| normalize_cover_bytes(value, cover_target))
        .transpose()?;
    if let Some(cover_bytes) = cover_bytes.as_deref() {
        if !cover_bytes.starts_with(PNG_SIGNATURE) {
            return Err(RomxError::Invalid("cover is not a PNG".into()));
        }
    }
    if let (Some(metadata_value), Some(cover_bytes)) =
        (metadata_value.as_mut(), cover_bytes.as_deref())
    {
        let object = metadata_value
            .as_object_mut()
            .expect("validated metadata is an object");
        let mut cover = Map::new();
        cover.insert("mime_type".into(), Value::String("image/png".into()));
        cover.insert(
            "sha256".into(),
            Value::String(hex_digest(&sha256(cover_bytes))),
        );
        if let Some((width, height)) = png_dimensions(cover_bytes) {
            cover.insert("width".into(), Value::from(width));
            cover.insert("height".into(), Value::from(height));
        }
        object.insert("cover".into(), Value::Object(cover));
    }
    let metadata_bytes = metadata_value
        .as_ref()
        .map(serde_json::to_vec)
        .transpose()?;
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
    let cover_offset = if cover_bytes.is_some() {
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
        size: cover_bytes.as_ref().map(|v| v.len() as u64).unwrap_or(0),
    };
    let mut body =
        Vec::with_capacity(rom.len() + metadata_region.size as usize + cover_region.size as usize);
    body.extend_from_slice(rom);
    if let Some(bytes) = &metadata_bytes {
        body.extend_from_slice(bytes);
    }
    if let Some(bytes) = cover_bytes.as_deref() {
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
    pack_to_path_with_options(rom_path, metadata_path, cover_path, output_path, None, None)
}

pub fn pack_to_path_with_crc32(
    rom_path: &Path,
    metadata_path: Option<&Path>,
    cover_path: Option<&Path>,
    output_path: &Path,
    crc32_override: Option<&str>,
) -> Result<(), RomxError> {
    pack_to_path_with_options(
        rom_path,
        metadata_path,
        cover_path,
        output_path,
        crc32_override,
        None,
    )
}

pub fn pack_to_path_with_options(
    rom_path: &Path,
    metadata_path: Option<&Path>,
    cover_path: Option<&Path>,
    output_path: &Path,
    crc32_override: Option<&str>,
    cover_target: Option<(u32, u32)>,
) -> Result<(), RomxError> {
    let rom = fs::read(rom_path)?;
    let metadata = if let Some(path) = metadata_path {
        Some(serde_json::from_slice::<Value>(&fs::read(path)?)?)
    } else {
        None
    };
    let cover = if let Some(path) = cover_path {
        Some(normalize_cover_path(path, cover_target)?)
    } else {
        None
    };
    let bytes = pack_bytes_with_options(
        &rom,
        metadata.as_ref(),
        cover.as_deref(),
        crc32_override,
        None,
    )?;
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
    validate_regions(&footer, body_len as u64)?;
    let body = &bytes[..body_len];
    let rom = slice_region(body, "rom", footer.rom)?.to_vec();
    if sha256(&rom) != footer.rom_sha256 {
        return Err(RomxError::Invalid("ROM SHA-256 mismatch".into()));
    }
    if footer.flags & FLAG_BODY_SHA256 != 0 && sha256(body) != footer.body_sha256 {
        return Err(RomxError::Invalid("body SHA-256 mismatch".into()));
    }
    let metadata = if footer.metadata.size == 0 {
        None
    } else {
        Some(slice_region(body, "metadata", footer.metadata)?.to_vec())
    };
    let cover = if footer.cover.size == 0 {
        None
    } else {
        Some(slice_region(body, "cover", footer.cover)?.to_vec())
    };
    let preview = parse_preview_regions(footer, metadata, cover)?;
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

fn parse_preview_regions(
    footer: Footer,
    metadata_bytes: Option<Vec<u8>>,
    cover_bytes: Option<Vec<u8>>,
) -> Result<RomxPreview, RomxError> {
    let metadata = if let Some(bytes) = metadata_bytes {
        let value: Value = serde_json::from_slice(&bytes)?;
        validate_metadata(&value)?;
        Some(value)
    } else {
        None
    };
    let cover = if let Some(bytes) = cover_bytes {
        if !bytes.starts_with(PNG_SIGNATURE) {
            return Err(RomxError::Invalid("embedded cover is not PNG".into()));
        }
        Some(bytes)
    } else {
        None
    };
    Ok(RomxPreview {
        footer,
        metadata,
        cover,
    })
}

fn read_file_region(
    file: &mut fs::File,
    body_len: u64,
    name: &str,
    region: Region,
) -> Result<Option<Vec<u8>>, RomxError> {
    if region.size == 0 {
        return Ok(None);
    }
    let end = region.end()?;
    if end > body_len {
        return Err(RomxError::Invalid(format!("{name} region reaches footer")));
    }
    let size = usize::try_from(region.size)
        .map_err(|_| RomxError::Invalid(format!("{name} region is too large")))?;
    file.seek(SeekFrom::Start(region.offset))?;
    let mut bytes = vec![0u8; size];
    file.read_exact(&mut bytes)?;
    Ok(Some(bytes))
}

/// Read only the footer, metadata, and optional cover from an in-memory ROMX.
///
/// This intentionally skips the ROM and body SHA-256 checks because it does
/// not read the ROM region. Use [`read_bytes`] when full validation is needed.
pub fn read_metadata_cover_bytes(bytes: &[u8]) -> Result<RomxPreview, RomxError> {
    if bytes.len() < FOOTER_SIZE {
        return Err(RomxError::Invalid("file is shorter than footer".into()));
    }
    let body_len = bytes.len() - FOOTER_SIZE;
    let footer = Footer::decode(&bytes[body_len..])?;
    validate_regions(&footer, body_len as u64)?;
    let body = &bytes[..body_len];
    let metadata = if footer.metadata.size == 0 {
        None
    } else {
        Some(slice_region(body, "metadata", footer.metadata)?.to_vec())
    };
    let cover = if footer.cover.size == 0 {
        None
    } else {
        Some(slice_region(body, "cover", footer.cover)?.to_vec())
    };
    parse_preview_regions(footer, metadata, cover)
}

/// Read only the footer, metadata, and optional cover from a ROMX path.
///
/// Unlike [`read_path`], this does not load the ROM payload or verify either
/// payload hash, so it is suitable for metadata and cover previews of large
/// ROMX files.
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
    let metadata = read_file_region(&mut file, body_len, "metadata", footer.metadata)?;
    let cover = read_file_region(&mut file, body_len, "cover", footer.cover)?;
    parse_preview_regions(footer, metadata, cover)
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
