//! Best-effort inspection of metadata and artwork stored inside ROM payloads.
//!
//! This is deliberately separate from the ROMX container reader.  A payload
//! probe never changes the bytes that are packed; it only supplies defaults
//! for the GUI/CLI when a source ROM contains its own title, serial, or icon.

use crate::{format_id_for_extension, validate_png_bytes, RomxError, DEFAULT_MAX_COVER_SIZE};
use encoding_rs::SHIFT_JIS;
use image::{DynamicImage, ImageBuffer, ImageFormat, Rgb, Rgba};
use serde_json::{Map, Value};
use std::fs;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

const MAX_SFO_SIZE: usize = 1024 * 1024;
const MAX_ISO_DIRECTORY_SIZE: usize = 4 * 1024 * 1024;
const MEDIA_UNIT_SIZE: u64 = 0x200;
const MAX_EXEFS_FILE_SIZE: usize = 16 * 1024 * 1024;
const SMDH_TITLE_SIZE: usize = 0x200;
const SMDH_SIZE: usize = 0x36c0;
const GAMECUBE_FST_HEADER_OFFSET: usize = 0x424;
const GAMECUBE_FST_MAX_SIZE: usize = 32 * 1024 * 1024;
const GAMECUBE_FST_ENTRY_SIZE: usize = 12;
const GAMECUBE_BANNER_IMAGE_OFFSET: usize = 0x20;
const GAMECUBE_BANNER_IMAGE_WIDTH: usize = 96;
const GAMECUBE_BANNER_IMAGE_HEIGHT: usize = 32;
const GAMECUBE_BANNER_METADATA_OFFSET: usize = 0x1820;
const GAMECUBE_BANNER_METADATA_BLOCK_SIZE: usize = 0x140;
const GAMECUBE_BANNER_SIZE: usize = 0x1960;

#[derive(Debug, Clone, Default)]
pub struct EmbeddedInfo {
    pub metadata: Map<String, Value>,
    pub cover: Option<Vec<u8>>,
    /// All usable artwork found in the payload, in display preference order.
    /// `cover` remains as a compatibility shortcut for the first item.
    pub covers: Vec<EmbeddedCover>,
    pub platform: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EmbeddedCover {
    pub name: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct PayloadProfile {
    pub payload_format: String,
    pub platform: String,
    pub metadata: Map<String, Value>,
    pub cover: Option<Vec<u8>>,
    pub covers: Vec<EmbeddedCover>,
}

pub fn infer_payload_format(path: &Path) -> Result<String, RomxError> {
    let format = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if format_id_for_extension(&format) != 0 {
        Ok(format)
    } else {
        Err(RomxError::Invalid(format!(
            "unsupported ROM extension: {}",
            path.extension()
                .and_then(|value| value.to_str())
                .unwrap_or("<none>")
        )))
    }
}

fn platform_for_format(format: &str) -> &'static str {
    match format {
        "gb" => "gb",
        "gbc" => "gbc",
        "gba" => "gba",
        "nes" | "unf" | "unif" => "nes",
        "fds" => "fds",
        "sfc" | "smc" => "snes",
        "nds" => "nds",
        "3ds" | "cci" | "cxi" | "app" => "3ds",
        "z64" | "n64" | "v64" => "n64",
        "md" | "gen" | "smd" => "genesis",
        "32x" => "genesis32x",
        "sms" => "sms",
        "gg" => "gg",
        "pce" => "pce",
        "iso" | "cso" | "chd" | "pbp" => "psp",
        "cdi" => "dreamcast",
        "gcm" => "gamecube",
        "wbfs" | "rvz" | "wia" | "wad" => "wii",
        "zso" => "ps2",
        "elf" | "prx" => "psp",
        _ => "gba",
    }
}

fn read_at(path: &Path, offset: u64, size: usize) -> Result<Vec<u8>, RomxError> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0u8; size];
    let count = file.read(&mut bytes)?;
    bytes.truncate(count);
    Ok(bytes)
}

fn read_exact_at(path: &Path, offset: u64, size: usize) -> Result<Vec<u8>, RomxError> {
    let bytes = read_at(path, offset, size)?;
    if bytes.len() != size {
        return Err(RomxError::Invalid(
            "embedded payload range is truncated".into(),
        ));
    }
    Ok(bytes)
}

fn le_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..offset.checked_add(2)?)
        .map(|value| u16::from_le_bytes([value[0], value[1]]))
}

fn le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset.checked_add(4)?)
        .map(|value| u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn be_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    bytes
        .get(offset..offset.checked_add(2)?)
        .map(|value| u16::from_be_bytes([value[0], value[1]]))
}

fn be_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset.checked_add(4)?)
        .map(|value| u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

fn clean_text(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes);
    let text = text
        .chars()
        .map(|character| if character == '\0' { ' ' } else { character })
        .collect::<String>();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.chars().take(512).collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn clean_header_text(bytes: &[u8]) -> Option<String> {
    let text = bytes
        .iter()
        .filter_map(|byte| {
            let character = *byte as char;
            (character.is_ascii_graphic() || character == ' ').then_some(character)
        })
        .collect::<String>();
    let text = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches([' ', '-', '_'])
        .chars()
        .take(512)
        .collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn insert_text(metadata: &mut Map<String, Value>, key: &str, bytes: &[u8]) {
    if let Some(value) = clean_text(bytes).or_else(|| clean_header_text(bytes)) {
        metadata.insert(key.into(), Value::String(value));
    }
}

fn header_info(path: &Path, format: &str, info: &mut EmbeddedInfo) {
    match format {
        "gb" | "gbc" => {
            if let Ok(bytes) = read_at(path, 0x134, 16) {
                insert_text(&mut info.metadata, "name", &bytes);
            }
        }
        "gba" => {
            if let Ok(bytes) = read_at(path, 0xa0, 12) {
                insert_text(&mut info.metadata, "name", &bytes);
            }
            if let Ok(bytes) = read_at(path, 0xac, 4) {
                insert_text(&mut info.metadata, "serial", &bytes);
            }
        }
        "nds" => {
            if let Ok(bytes) = read_at(path, 0, 12) {
                insert_text(&mut info.metadata, "name", &bytes);
            }
            if let Ok(bytes) = read_at(path, 12, 4) {
                insert_text(&mut info.metadata, "serial", &bytes);
            }
        }
        "n64" | "z64" | "v64" => {
            if let Ok(mut bytes) = read_at(path, 0, 0x40) {
                normalize_n64_header(&mut bytes);
                if bytes.starts_with(b"\x80\x37\x12\x40") {
                    insert_text(&mut info.metadata, "name", &bytes[0x20..]);
                }
            }
        }
        "md" | "gen" | "smd" | "32x" => {
            if let Some(bytes) = sega_header(path, format) {
                insert_text(&mut info.metadata, "name", &bytes[0x50..0x80]);
                if !info.metadata.contains_key("name") {
                    insert_text(&mut info.metadata, "name", &bytes[0x20..0x50]);
                }
                insert_text(&mut info.metadata, "serial", &bytes[0x80..0x8e]);
            }
        }
        "sfc" | "smc" => {
            let candidates = [0x7fc0, 0xffc0, 0x40ffc0, 0x81c0, 0x101c0, 0x4101c0];
            let mut best = None;
            let mut best_score = i32::MIN;
            for offset in candidates {
                if let Ok(bytes) = read_at(path, offset, 32) {
                    if bytes.len() < 21 {
                        continue;
                    }
                    let score = bytes[..21]
                        .iter()
                        .map(|byte| {
                            if byte.is_ascii_graphic() || *byte == b' ' {
                                1
                            } else if *byte == 0 || *byte == 0xff {
                                0
                            } else {
                                -2
                            }
                        })
                        .sum::<i32>();
                    if score > best_score {
                        best_score = score;
                        best = Some(bytes);
                    }
                }
            }
            if best_score >= 8 {
                if let Some(bytes) = best {
                    insert_text(&mut info.metadata, "name", &bytes[..21]);
                }
            }
        }
        _ => {}
    }
}

fn clean_utf16_text(bytes: &[u8]) -> Option<String> {
    let mut units = Vec::with_capacity(bytes.len() / 2);
    for chunk in bytes.chunks_exact(2) {
        let unit = u16::from_le_bytes([chunk[0], chunk[1]]);
        if unit == 0 {
            break;
        }
        units.push(unit);
    }
    let text = String::from_utf16_lossy(&units);
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.chars().take(512).collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn morton8(x: usize, y: usize) -> usize {
    let mut result = 0;
    for bit in 0..3 {
        result |= ((x >> bit) & 1) << (bit * 2);
        result |= ((y >> bit) & 1) << (bit * 2 + 1);
    }
    result
}

fn encode_smdh_icon(data: &[u8], offset: usize, width: usize) -> Option<Vec<u8>> {
    if !matches!(width, 24 | 48) {
        return None;
    }
    let tiles_per_row = width / 8;
    let tile_count = tiles_per_row * tiles_per_row;
    let byte_count = tile_count.checked_mul(64)?.checked_mul(2)?;
    let source = data.get(offset..offset.checked_add(byte_count)?)?;
    let mut rgb = vec![0u8; width * width * 3];
    for tile_index in 0..tile_count {
        let tile_x = tile_index % tiles_per_row;
        let tile_y = tile_index / tiles_per_row;
        for local_y in 0..8 {
            for local_x in 0..8 {
                let pixel_index = morton8(local_x, local_y);
                let source_offset = (tile_index * 64 + pixel_index) * 2;
                let color = u16::from_le_bytes([source[source_offset], source[source_offset + 1]]);
                let r = ((color >> 11) & 0x1f) as u8;
                let g = ((color >> 5) & 0x3f) as u8;
                let b = (color & 0x1f) as u8;
                let x = tile_x * 8 + local_x;
                let y = tile_y * 8 + local_y;
                let output = (y * width + x) * 3;
                rgb[output] = (r << 3) | (r >> 2);
                rgb[output + 1] = (g << 2) | (g >> 4);
                rgb[output + 2] = (b << 3) | (b >> 2);
            }
        }
    }
    let image = ImageBuffer::<Rgb<u8>, Vec<u8>>::from_raw(width as u32, width as u32, rgb)?;
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(image)
        .write_to(&mut output, ImageFormat::Png)
        .ok()?;
    let bytes = output.into_inner();
    validate_png_bytes(&bytes).ok()?;
    Some(bytes)
}

fn apply_smdh(info: &mut EmbeddedInfo, smdh: &[u8]) {
    if smdh.len() < SMDH_SIZE || smdh.get(..4) != Some(b"SMDH") {
        return;
    }

    // English is the normal system language.  Prefer Simplified Chinese next
    // so localized titles remain useful on Chinese ROM dumps, then fall back
    // through the remaining SMDH language slots.
    let mut language_order = vec![1usize, 6, 0, 11];
    language_order.extend(2..=10);
    for language in 12..16 {
        language_order.push(language);
    }
    let mut selected = None;
    for language in language_order {
        let base = 8usize.saturating_add(language.saturating_mul(SMDH_TITLE_SIZE));
        let Some(title) = smdh.get(base..base + 0x80).and_then(clean_utf16_text) else {
            continue;
        };
        let description = smdh
            .get(base + 0x80..base + 0x180)
            .and_then(clean_utf16_text);
        let publisher = smdh
            .get(base + 0x180..base + 0x200)
            .and_then(clean_utf16_text);
        selected = Some((title, description, publisher));
        break;
    }
    if let Some((title, description, publisher)) = selected {
        info.metadata.insert("name".into(), Value::String(title));
        if let Some(description) = description {
            info.metadata
                .insert("description".into(), Value::String(description));
        }
        if let Some(publisher) = publisher {
            info.metadata
                .insert("developer".into(), Value::String(publisher));
        }
    }

    if let Some(icon) = encode_smdh_icon(smdh, 0x24c0, 48) {
        add_embedded_cover(info, "3DS icon (48x48)", icon);
    }
    if let Some(icon) = encode_smdh_icon(smdh, 0x2040, 24) {
        add_embedded_cover(info, "3DS icon (24x24)", icon);
    }
}

fn find_3ds_ncch_base(path: &Path) -> Option<u64> {
    let file_size = fs::metadata(path).ok()?.len();
    let magic = read_at(path, 0x100, 4).ok()?;
    if magic == b"NCCH" {
        return Some(0);
    }
    if magic != b"NCSD" {
        return None;
    }
    let partitions = read_exact_at(path, 0x120, 8 * 8).ok()?;
    for index in 0..8 {
        let offset_units = u64::from(le_u32(&partitions, index * 8)?);
        let size_units = u64::from(le_u32(&partitions, index * 8 + 4)?);
        if offset_units == 0 || size_units == 0 {
            continue;
        }
        let offset = offset_units.checked_mul(MEDIA_UNIT_SIZE)?;
        let size = size_units.checked_mul(MEDIA_UNIT_SIZE)?;
        if offset.checked_add(size)? > file_size || offset.checked_add(0x104)? > file_size {
            continue;
        }
        if read_at(path, offset + 0x100, 4).ok()?.as_slice() == b"NCCH" {
            return Some(offset);
        }
    }
    None
}

fn embedded_3ds(path: &Path) -> EmbeddedInfo {
    let mut info = EmbeddedInfo {
        platform: Some("3ds".into()),
        ..Default::default()
    };
    let Some(base) = find_3ds_ncch_base(path) else {
        return info;
    };
    let Ok(header) = read_exact_at(path, base + 0x100, 0x100) else {
        return info;
    };
    if header.get(..4) != Some(b"NCCH") {
        return info;
    }
    if let Ok(product_code) = read_at(path, base + 0x150, 16) {
        insert_text(&mut info.metadata, "serial", &product_code);
    }
    let Some(exefs_offset) = le_u32(&header, 0xa0) else {
        return info;
    };
    let Some(exefs_size) = le_u32(&header, 0xa4) else {
        return info;
    };
    let Some(exefs_start) =
        base.checked_add(u64::from(exefs_offset).saturating_mul(MEDIA_UNIT_SIZE))
    else {
        return info;
    };
    let exefs_bytes = u64::from(exefs_size).saturating_mul(MEDIA_UNIT_SIZE);
    if exefs_bytes < 0x200 || exefs_bytes > MAX_EXEFS_FILE_SIZE as u64 {
        return info;
    }
    let Ok(exefs_header) = read_exact_at(path, exefs_start, 0x200) else {
        return info;
    };
    for index in 0..10 {
        let entry = index * 16;
        let Some(raw_name) = exefs_header.get(entry..entry + 8) else {
            break;
        };
        let name = raw_name
            .split(|byte| *byte == 0)
            .next()
            .map(String::from_utf8_lossy)
            .map(|value| value.to_ascii_lowercase())
            .unwrap_or_default();
        if name != "icon" {
            continue;
        }
        let Some(file_offset) = le_u32(&exefs_header, entry + 8) else {
            continue;
        };
        let Some(file_size) = le_u32(&exefs_header, entry + 12) else {
            continue;
        };
        let data_end = 0x200u64
            .checked_add(u64::from(file_offset))
            .and_then(|offset| offset.checked_add(u64::from(file_size)));
        if data_end.is_none_or(|end| end > exefs_bytes) || file_size as usize > MAX_EXEFS_FILE_SIZE
        {
            continue;
        }
        let data_start = exefs_start
            .checked_add(0x200)
            .and_then(|offset| offset.checked_add(u64::from(file_offset)));
        let Some(data_start) = data_start else {
            continue;
        };
        let Ok(smdh) = read_exact_at(path, data_start, file_size as usize) else {
            continue;
        };
        apply_smdh(&mut info, &smdh);
        break;
    }
    info
}

fn clean_gamecube_text(bytes: &[u8]) -> Option<String> {
    let bytes = bytes.split(|byte| *byte == 0).next().unwrap_or_default();
    if bytes.is_empty() {
        return None;
    }
    // Most banners use ASCII or UTF-8, while Japanese GameCube banners use
    // Shift-JIS. Prefer valid UTF-8 and decode the legacy representation only
    // when the byte sequence requires it.
    let decoded = if let Ok(text) = std::str::from_utf8(bytes) {
        text.to_owned()
    } else {
        SHIFT_JIS.decode(bytes).0.into_owned()
    };
    let text = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.chars().take(512).collect::<String>();
    (!text.is_empty()).then_some(text)
}

fn insert_gamecube_text(metadata: &mut Map<String, Value>, key: &str, bytes: &[u8]) {
    if let Some(value) = clean_gamecube_text(bytes) {
        metadata.insert(key.into(), Value::String(value));
    }
}

fn encode_gamecube_banner_image(data: &[u8]) -> Option<Vec<u8>> {
    let pixel_count = GAMECUBE_BANNER_IMAGE_WIDTH.checked_mul(GAMECUBE_BANNER_IMAGE_HEIGHT)?;
    let source = data.get(
        GAMECUBE_BANNER_IMAGE_OFFSET
            ..GAMECUBE_BANNER_IMAGE_OFFSET.checked_add(pixel_count.checked_mul(2)?)?,
    )?;
    let mut rgba = vec![0u8; pixel_count * 4];
    // GX RGB5A3 stores pixels in 4x4 tiles rather than scanline order.
    // `opening.bnr` uses a 96x32 texture, so each row contains 24 tiles.
    let tiles_per_row = GAMECUBE_BANNER_IMAGE_WIDTH / 4;
    for y in 0..GAMECUBE_BANNER_IMAGE_HEIGHT {
        for x in 0..GAMECUBE_BANNER_IMAGE_WIDTH {
            let tile = (y / 4) * tiles_per_row + x / 4;
            let tile_pixel = (y % 4) * 4 + (x % 4);
            let source_pixel = tile * 16 + tile_pixel;
            let color = be_u16(source, source_pixel * 2)?;
            let output = (y * GAMECUBE_BANNER_IMAGE_WIDTH + x) * 4;
            if color & 0x8000 != 0 {
                let red = ((color >> 10) & 0x1f) as u8;
                let green = ((color >> 5) & 0x1f) as u8;
                let blue = (color & 0x1f) as u8;
                rgba[output] = (red << 3) | (red >> 2);
                rgba[output + 1] = (green << 3) | (green >> 2);
                rgba[output + 2] = (blue << 3) | (blue >> 2);
                rgba[output + 3] = 255;
            } else {
                let alpha = ((color >> 12) & 0x07) as u8;
                rgba[output] = (((color >> 8) & 0x0f) as u8) * 17;
                rgba[output + 1] = (((color >> 4) & 0x0f) as u8) * 17;
                rgba[output + 2] = (color as u8 & 0x0f) * 17;
                rgba[output + 3] = (u16::from(alpha) * 255 / 7) as u8;
            }
        }
    }
    let image = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(
        GAMECUBE_BANNER_IMAGE_WIDTH as u32,
        GAMECUBE_BANNER_IMAGE_HEIGHT as u32,
        rgba,
    )?;
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut output, ImageFormat::Png)
        .ok()?;
    let bytes = output.into_inner();
    validate_png_bytes(&bytes).ok()?;
    Some(bytes)
}

fn gamecube_fst(path: &Path) -> Option<(Vec<u8>, u64)> {
    let disc_size = fs::metadata(path).ok()?.len();
    let header = read_exact_at(path, 0, GAMECUBE_FST_HEADER_OFFSET + 8).ok()?;
    let fst_offset = u64::from(be_u32(&header, GAMECUBE_FST_HEADER_OFFSET)?);
    let fst_size = usize::try_from(be_u32(&header, GAMECUBE_FST_HEADER_OFFSET + 4)?).ok()?;
    if fst_offset == 0
        || !(GAMECUBE_FST_ENTRY_SIZE..=GAMECUBE_FST_MAX_SIZE).contains(&fst_size)
        || fst_offset.checked_add(fst_size as u64)? > disc_size
    {
        return None;
    }
    let fst = read_exact_at(path, fst_offset, fst_size).ok()?;
    let entry_count = usize::try_from(be_u32(&fst, 8)?).ok()?;
    let entries_size = entry_count.checked_mul(GAMECUBE_FST_ENTRY_SIZE)?;
    if entry_count == 0 || entries_size > fst.len() {
        return None;
    }
    Some((fst, disc_size))
}

fn gamecube_banner_file(path: &Path) -> Option<Vec<u8>> {
    let (fst, disc_size) = gamecube_fst(path)?;
    let entry_count = usize::try_from(be_u32(&fst, 8)?).ok()?;
    let entries_size = entry_count.checked_mul(GAMECUBE_FST_ENTRY_SIZE)?;
    let strings = &fst[entries_size..];
    for index in 1..entry_count {
        let entry = index.checked_mul(GAMECUBE_FST_ENTRY_SIZE)?;
        let flags_and_name = be_u32(&fst, entry)?;
        if flags_and_name & 0x8000_0000 != 0 {
            continue;
        }
        let name_offset = usize::try_from(flags_and_name & 0x00ff_ffff).ok()?;
        let name = strings
            .get(name_offset..)?
            .split(|byte| *byte == 0)
            .next()?;
        if !name.eq_ignore_ascii_case(b"opening.bnr") {
            continue;
        }
        let file_offset = u64::from(be_u32(&fst, entry + 4)?);
        let entry_size = u64::from(be_u32(&fst, entry + 8)?);
        let entry_size_usize = usize::try_from(entry_size).ok()?;
        if file_offset.checked_add(entry_size)? > disc_size
            || entry_size_usize > GAMECUBE_FST_MAX_SIZE
        {
            return None;
        }
        return read_exact_at(path, file_offset, entry_size_usize).ok();
    }
    None
}

fn is_gamecube_disc(path: &Path) -> bool {
    let Ok(game_id) = read_at(path, 0, 6) else {
        return false;
    };
    if game_id.len() != 6 || !game_id.iter().all(|byte| byte.is_ascii_alphanumeric()) {
        return false;
    }
    if read_at(path, 0x1c, 4).ok().as_deref() != Some(b"\xc23\x9f\x3d") {
        return false;
    }
    gamecube_fst(path).is_some()
}

fn embedded_gamecube(path: &Path) -> EmbeddedInfo {
    let mut info = EmbeddedInfo {
        platform: Some("gamecube".into()),
        ..Default::default()
    };
    if let Ok(game_id) = read_at(path, 0, 6) {
        insert_gamecube_text(&mut info.metadata, "serial", &game_id);
    }
    if let Ok(title) = read_at(path, 0x20, 64) {
        insert_gamecube_text(&mut info.metadata, "name", &title);
    }
    let Some(banner) = gamecube_banner_file(path) else {
        return info;
    };
    if banner.len() < GAMECUBE_BANNER_SIZE {
        return info;
    }
    if !banner.starts_with(b"BNR1") && !banner.starts_with(b"BNR2") {
        return info;
    }
    if let Some(image) = encode_gamecube_banner_image(&banner) {
        add_embedded_cover(&mut info, "GameCube banner", image);
    }

    let block_count = if banner.starts_with(b"BNR2") {
        ((banner.len().saturating_sub(GAMECUBE_BANNER_METADATA_OFFSET))
            / GAMECUBE_BANNER_METADATA_BLOCK_SIZE)
            .clamp(1, 6)
    } else {
        1
    };
    for block in 0..block_count {
        let Some(base) = GAMECUBE_BANNER_METADATA_OFFSET
            .checked_add(block * GAMECUBE_BANNER_METADATA_BLOCK_SIZE)
        else {
            break;
        };
        let title = banner
            .get(base + 0x40..base + 0x80)
            .and_then(clean_gamecube_text)
            .or_else(|| banner.get(base..base + 0x20).and_then(clean_gamecube_text));
        let developer = banner
            .get(base + 0x80..base + 0xc0)
            .and_then(clean_gamecube_text)
            .or_else(|| {
                banner
                    .get(base + 0x20..base + 0x40)
                    .and_then(clean_gamecube_text)
            });
        let description = banner
            .get(base + 0xc0..base + 0x140)
            .and_then(clean_gamecube_text);
        if title.is_some() || developer.is_some() || description.is_some() {
            if let Some(title) = title {
                info.metadata.insert("name".into(), Value::String(title));
            }
            if let Some(developer) = developer {
                info.metadata
                    .insert("developer".into(), Value::String(developer));
            }
            if let Some(description) = description {
                info.metadata
                    .insert("description".into(), Value::String(description));
            }
            break;
        }
    }
    info
}

fn sega_header(path: &Path, format: &str) -> Option<Vec<u8>> {
    let mut header = read_at(path, 0x100, 0x100).ok()?;
    if format == "smd" && !header.starts_with(b"SEGA") {
        let block = read_at(path, 512, 16_384).ok()?;
        if block.len() < 16_384 {
            return None;
        }
        let mut logical = vec![0u8; 16_384];
        for index in 0..8192 {
            logical[index * 2] = block[8192 + index];
            logical[index * 2 + 1] = block[index];
        }
        header = logical.get(0x100..0x200)?.to_vec();
    }
    header.starts_with(b"SEGA").then_some(header)
}

fn normalize_n64_header(bytes: &mut [u8]) {
    if bytes.starts_with(b"\x37\x80\x40\x12") {
        for pair in bytes.chunks_exact_mut(2) {
            pair.swap(0, 1);
        }
    } else if bytes.starts_with(b"\x40\x12\x37\x80") {
        for word in bytes.chunks_exact_mut(4) {
            word.reverse();
        }
    }
}

fn encode_nds_icon(rgba: Vec<u8>) -> Option<Vec<u8>> {
    let image = ImageBuffer::<Rgba<u8>, Vec<u8>>::from_raw(32, 32, rgba)?;
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(image)
        .write_to(&mut output, ImageFormat::Png)
        .ok()?;
    let bytes = output.into_inner();
    validate_png_bytes(&bytes).ok()?;
    Some(bytes)
}

fn add_embedded_cover(info: &mut EmbeddedInfo, name: &str, bytes: Vec<u8>) {
    if info.covers.iter().any(|cover| cover.name == name) {
        return;
    }
    if info.cover.is_none() {
        info.cover = Some(bytes.clone());
    }
    info.covers.push(EmbeddedCover {
        name: name.to_owned(),
        bytes,
    });
}

fn nds_icon(path: &Path, info: &mut EmbeddedInfo) {
    let offset = read_at(path, 0x68, 4)
        .and_then(|bytes| {
            le_u32(&bytes, 0).ok_or_else(|| RomxError::Invalid("missing NDS banner offset".into()))
        })
        .ok();
    let Some(offset) = offset else { return };
    let Ok(banner) = read_at(path, u64::from(offset), 0x240) else {
        return;
    };
    if banner.len() < 0x240 {
        return;
    }
    let mut rgba = vec![0u8; 32 * 32 * 4];
    for tile_y in 0..4 {
        for tile_x in 0..4 {
            for pixel_y in 0..8 {
                for pixel_x in 0..8 {
                    let tile = tile_y * 4 + tile_x;
                    let pixel = pixel_y * 8 + pixel_x;
                    let packed = banner[0x20 + tile * 32 + pixel / 2];
                    let palette_index = if pixel & 1 != 0 {
                        packed >> 4
                    } else {
                        packed & 0x0f
                    };
                    let color_offset = 0x220 + usize::from(palette_index) * 2;
                    let color =
                        u16::from_le_bytes([banner[color_offset], banner[color_offset + 1]]);
                    let x = tile_x * 8 + pixel_x;
                    let y = tile_y * 8 + pixel_y;
                    let output = (y * 32 + x) * 4;
                    rgba[output] = (((color & 31) * 255) / 31) as u8;
                    rgba[output + 1] = ((((color >> 5) & 31) * 255) / 31) as u8;
                    rgba[output + 2] = ((((color >> 10) & 31) * 255) / 31) as u8;
                    rgba[output + 3] = if palette_index == 0 { 0 } else { 255 };
                }
            }
        }
    }
    if let Some(icon) = encode_nds_icon(rgba) {
        add_embedded_cover(info, "NDS icon", icon);
    }
}

fn clean_sfo_text(bytes: &[u8]) -> Option<String> {
    clean_text(bytes.split(|byte| *byte == 0).next().unwrap_or_default())
}

fn parse_sfo(raw: &[u8]) -> Option<Map<String, Value>> {
    if raw.len() < 20 || &raw[..4] != b"\0PSF" {
        return None;
    }
    let key_offset = le_u32(raw, 8)? as usize;
    let data_offset = le_u32(raw, 12)? as usize;
    let count = le_u32(raw, 16)? as usize;
    if count > 4096 || 20usize.checked_add(count.checked_mul(16)?)? > raw.len() {
        return None;
    }
    let mut result = Map::new();
    for index in 0..count {
        let entry = 20 + index * 16;
        let key_rel = le_u16(raw, entry)? as usize;
        let format = le_u16(raw, entry + 2)?;
        let length = le_u32(raw, entry + 4)? as usize;
        let data_rel = le_u32(raw, entry + 12)? as usize;
        let key_start = key_offset.checked_add(key_rel)?;
        let data_start = data_offset.checked_add(data_rel)?;
        if key_start >= raw.len()
            || data_start > raw.len()
            || length > raw.len().saturating_sub(data_start)
        {
            continue;
        }
        let key_end = raw[key_start..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|position| key_start + position)?;
        let key = String::from_utf8_lossy(&raw[key_start..key_end]);
        if matches!(format, 0x0004 | 0x0204) {
            if let Some(value) = clean_sfo_text(&raw[data_start..data_start + length]) {
                result.insert(key.into_owned(), Value::String(value));
            }
        }
    }
    Some(result)
}

fn apply_sfo(info: &mut EmbeddedInfo, fields: &Map<String, Value>) {
    if let Some(value) = fields.get("TITLE") {
        info.metadata.insert("name".into(), value.clone());
    }
    if let Some(value) = fields.get("DISC_ID").or_else(|| fields.get("TITLE_ID")) {
        if let Some(serial) = value.as_str() {
            info.metadata
                .insert("serial".into(), Value::String(serial.to_ascii_uppercase()));
        }
    }
    if let Some(value) = fields.get("CATEGORY") {
        info.metadata.insert("category".into(), value.clone());
        if value.as_str() == Some("ME") {
            info.platform = Some("playstation".into());
        }
    }
    if let Some(value) = fields.get("REGION") {
        info.metadata.insert("region".into(), value.clone());
    }
}

fn embedded_pbp(path: &Path) -> EmbeddedInfo {
    let mut info = EmbeddedInfo {
        platform: Some("psp".into()),
        ..Default::default()
    };
    let Ok(header) = read_at(path, 0, 40) else {
        return info;
    };
    if header.len() < 40 || &header[..4] != b"\0PBP" {
        return info;
    }
    let Ok(file_size) = fs::metadata(path).map(|value| value.len()) else {
        return info;
    };
    let mut offsets = [0u32; 8];
    for (index, offset) in offsets.iter_mut().enumerate() {
        let Some(value) = le_u32(&header, 8 + index * 4) else {
            return info;
        };
        *offset = value;
    }
    for index in 0..8 {
        let start = u64::from(offsets[index]);
        let end = if index + 1 < 8 {
            u64::from(offsets[index + 1])
        } else {
            file_size
        };
        if start > end || end > file_size {
            return info;
        }
        let size = (end - start).min(DEFAULT_MAX_COVER_SIZE + 1) as usize;
        let Ok(bytes) = read_at(path, start, size) else {
            continue;
        };
        if index == 0 {
            if let Some(fields) = parse_sfo(&bytes) {
                apply_sfo(&mut info, &fields);
            }
        } else if validate_png_bytes(&bytes).is_ok() {
            let name = match index {
                1 => Some("ICON0"),
                3 => Some("PIC0"),
                4 => Some("PIC1"),
                _ => None,
            };
            if let Some(name) = name {
                add_embedded_cover(&mut info, name, bytes);
            }
        }
    }
    info
}

#[derive(Debug, Clone, Copy)]
struct IsoRecord {
    extent: u32,
    size: u32,
    directory: bool,
}

struct IsoReader {
    path: PathBuf,
    sector_size: u64,
    root: IsoRecord,
}

impl IsoReader {
    fn open(path: &Path) -> Option<Self> {
        let pvd = read_exact_at(path, 16 * 2048, 2048).ok()?;
        if pvd.first().copied() != Some(1) || pvd.get(1..6)? != b"CD001" {
            return None;
        }
        let sector_size = u64::from(le_u16(&pvd, 128)?);
        if sector_size == 0 || sector_size > 0x10000 {
            return None;
        }
        Some(Self {
            path: path.to_owned(),
            sector_size,
            root: IsoRecord {
                extent: le_u32(&pvd, 158)?,
                size: le_u32(&pvd, 166)?,
                directory: true,
            },
        })
    }

    fn entries(&self, record: IsoRecord) -> Option<Vec<(String, IsoRecord)>> {
        let size = usize::try_from(record.size).ok()?;
        if size == 0 || size > MAX_ISO_DIRECTORY_SIZE {
            return None;
        }
        let bytes = read_exact_at(
            &self.path,
            u64::from(record.extent) * self.sector_size,
            size,
        )
        .ok()?;
        let mut result = Vec::new();
        let mut offset = 0usize;
        while offset < bytes.len() {
            let length = usize::from(bytes[offset]);
            if length == 0 {
                offset = ((offset / self.sector_size as usize) + 1) * self.sector_size as usize;
                continue;
            }
            if length < 34 || offset + length > bytes.len() {
                break;
            }
            let entry = &bytes[offset..offset + length];
            let name_len = usize::from(*entry.get(32)?);
            if 33 + name_len > entry.len() {
                break;
            }
            let raw_name = &entry[33..33 + name_len];
            let name = if raw_name == [0] {
                ".".into()
            } else if raw_name == [1] {
                "..".into()
            } else {
                String::from_utf8_lossy(raw_name)
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim_end_matches('.')
                    .to_owned()
            };
            result.push((
                name,
                IsoRecord {
                    extent: le_u32(entry, 2)?,
                    size: le_u32(entry, 10)?,
                    directory: entry.get(25).is_some_and(|value| value & 2 != 0),
                },
            ));
            offset += length;
        }
        Some(result)
    }

    fn find(&self, pathname: &str) -> Option<IsoRecord> {
        let mut current = self.root;
        for component in pathname
            .replace('\\', "/")
            .split('/')
            .filter(|value| !value.is_empty() && *value != ".")
        {
            if !current.directory {
                return None;
            }
            let wanted = component.to_ascii_uppercase();
            current = self
                .entries(current)?
                .into_iter()
                .find(|(name, _)| name.to_ascii_uppercase() == wanted)?
                .1;
        }
        Some(current)
    }

    fn read_file(&self, pathname: &str, max_size: usize) -> Option<Vec<u8>> {
        let record = self.find(pathname)?;
        if record.directory || usize::try_from(record.size).ok()? > max_size {
            return None;
        }
        read_exact_at(
            &self.path,
            u64::from(record.extent) * self.sector_size,
            record.size as usize,
        )
        .ok()
    }
}

fn embedded_iso(path: &Path) -> EmbeddedInfo {
    let mut info = EmbeddedInfo {
        platform: Some("psp".into()),
        ..Default::default()
    };
    let Some(reader) = IsoReader::open(path) else {
        return info;
    };
    if let Some(sfo) = reader.read_file("PSP_GAME/PARAM.SFO", MAX_SFO_SIZE) {
        if let Some(fields) = parse_sfo(&sfo) {
            apply_sfo(&mut info, &fields);
        }
    }
    for (name, candidate) in [
        ("ICON0", "PSP_GAME/ICON0.PNG"),
        ("PIC0", "PSP_GAME/PIC0.PNG"),
        ("PIC1", "PSP_GAME/PIC1.PNG"),
    ] {
        if let Some(bytes) = reader.read_file(candidate, DEFAULT_MAX_COVER_SIZE as usize) {
            if validate_png_bytes(&bytes).is_ok() {
                add_embedded_cover(&mut info, name, bytes);
            }
        }
    }
    info
}

pub fn extract_embedded_info(path: &Path, payload_format: &str) -> Result<EmbeddedInfo, RomxError> {
    let mut info = match payload_format {
        "pbp" => embedded_pbp(path),
        "gcm" => embedded_gamecube(path),
        "iso" if is_gamecube_disc(path) => embedded_gamecube(path),
        "iso" => embedded_iso(path),
        "3ds" | "cci" | "cxi" | "app" => embedded_3ds(path),
        _ => EmbeddedInfo::default(),
    };
    header_info(path, payload_format, &mut info);
    if payload_format == "nds" && info.covers.is_empty() {
        nds_icon(path, &mut info);
    }
    Ok(info)
}

pub fn inspect_payload_profile(path: &Path) -> Result<PayloadProfile, RomxError> {
    let mut payload_format = infer_payload_format(path)?;
    if fs::metadata(path)?.len() == 0 {
        return Err(RomxError::Invalid("ROM payload must not be empty".into()));
    }
    if matches!(payload_format.as_str(), "gb" | "gbc") {
        let header = read_at(path, 0, 0x144)?;
        payload_format = crate::classify_gb_payload(&header, Some(&payload_format))?.to_owned();
    }
    let embedded = extract_embedded_info(path, &payload_format)?;
    let platform = embedded
        .platform
        .unwrap_or_else(|| platform_for_format(&payload_format).into());
    let covers = embedded.covers;
    Ok(PayloadProfile {
        payload_format,
        platform,
        metadata: embedded.metadata,
        cover: embedded.cover,
        covers,
    })
}

#[cfg(test)]
mod tests {
    use super::{encode_nds_icon, inspect_payload_profile};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn reads_gba_title_and_serial_from_header() {
        let root = tempdir().unwrap();
        let path = root.path().join("sample.gba");
        let mut bytes = vec![0u8; 0xb3];
        bytes[0xa0..0xa9].copy_from_slice(b"TEST GAME");
        bytes[0xac..0xb0].copy_from_slice(b"ABCD");
        bytes[0xb2] = 0x96;
        fs::write(&path, bytes).unwrap();

        let profile = inspect_payload_profile(&path).unwrap();
        assert_eq!(profile.platform, "gba");
        assert_eq!(profile.metadata["name"], "TEST GAME");
        assert_eq!(profile.metadata["serial"], "ABCD");
    }

    #[test]
    fn extracts_nds_banner_icon_as_png() {
        let root = tempdir().unwrap();
        let path = root.path().join("sample.nds");
        let banner_offset = 0x200usize;
        let mut bytes = vec![0u8; banner_offset + 0x240];
        bytes[0..8].copy_from_slice(b"NDS GAME");
        bytes[12..16].copy_from_slice(b"ABCD");
        bytes[0x68..0x6c].copy_from_slice(&(banner_offset as u32).to_le_bytes());
        for value in &mut bytes[banner_offset + 0x20..banner_offset + 0x220] {
            *value = 0x11;
        }
        bytes[banner_offset + 0x222..banner_offset + 0x224]
            .copy_from_slice(&0x7fff_u16.to_le_bytes());
        fs::write(&path, bytes).unwrap();

        let profile = inspect_payload_profile(&path).unwrap();
        assert_eq!(profile.platform, "nds");
        assert_eq!(profile.metadata["name"], "NDS GAME");
        assert_eq!(profile.metadata["serial"], "ABCD");
        assert!(profile
            .cover
            .as_deref()
            .is_some_and(|cover| cover.starts_with(crate::PNG_SIGNATURE)));
        assert_eq!(profile.covers.len(), 1);
    }

    #[test]
    fn exposes_all_valid_pbp_artwork_choices() {
        let root = tempdir().unwrap();
        let path = root.path().join("sample.pbp");
        let icon = encode_nds_icon(vec![255; 32 * 32 * 4]).unwrap();
        let pic0 = encode_nds_icon(vec![64; 32 * 32 * 4]).unwrap();
        let pic1 = encode_nds_icon(vec![128; 32 * 32 * 4]).unwrap();
        let data_start = 40u32;
        let icon_end = data_start + icon.len() as u32;
        let pic0_end = icon_end + pic0.len() as u32;
        let pic1_end = pic0_end + pic1.len() as u32;
        let offsets = [
            data_start, data_start, icon_end, icon_end, pic0_end, pic1_end, pic1_end, pic1_end,
        ];
        let mut bytes = vec![0u8; 40];
        bytes[..4].copy_from_slice(b"\0PBP");
        for (index, offset) in offsets.into_iter().enumerate() {
            bytes[8 + index * 4..12 + index * 4].copy_from_slice(&offset.to_le_bytes());
        }
        bytes.extend_from_slice(&icon);
        bytes.extend_from_slice(&pic0);
        bytes.extend_from_slice(&pic1);
        fs::write(&path, bytes).unwrap();

        let profile = inspect_payload_profile(&path).unwrap();
        assert_eq!(profile.platform, "psp");
        assert_eq!(
            profile
                .covers
                .iter()
                .map(|cover| cover.name.as_str())
                .collect::<Vec<_>>(),
            ["ICON0", "PIC0", "PIC1"]
        );
        assert_eq!(
            profile.cover.as_deref(),
            Some(profile.covers[0].bytes.as_slice())
        );
    }

    #[test]
    fn reads_3ds_cci_smdh_title_serial_and_icons() {
        let root = tempdir().unwrap();
        let path = root.path().join("sample.cci");
        let partition_base = 0x4000usize;
        let exefs_base = partition_base + 0x15 * 0x200;
        let smdh_size = super::SMDH_SIZE;
        let mut smdh = vec![0u8; smdh_size];
        smdh[..4].copy_from_slice(b"SMDH");
        for (offset, value) in [(8 + 0x200, "Test 3DS"), (8 + 0x200 + 0x180, "Nintendo")] {
            let encoded = value.encode_utf16().collect::<Vec<_>>();
            for (index, unit) in encoded.into_iter().enumerate() {
                smdh[offset + index * 2..offset + index * 2 + 2]
                    .copy_from_slice(&unit.to_le_bytes());
            }
        }
        let mut bytes = vec![0u8; exefs_base + 0x200 + smdh_size];
        bytes[0x100..0x104].copy_from_slice(b"NCSD");
        bytes[0x120..0x124].copy_from_slice(&0x20u32.to_le_bytes());
        bytes[0x124..0x128].copy_from_slice(&0x31u32.to_le_bytes());
        bytes[partition_base + 0x100..partition_base + 0x104].copy_from_slice(b"NCCH");
        bytes[partition_base + 0x150..partition_base + 0x158].copy_from_slice(b"CTR-P-TE");
        bytes[partition_base + 0x1a0..partition_base + 0x1a4]
            .copy_from_slice(&0x15u32.to_le_bytes());
        bytes[partition_base + 0x1a4..partition_base + 0x1a8]
            .copy_from_slice(&0x20u32.to_le_bytes());
        bytes[exefs_base..exefs_base + 4].copy_from_slice(b"icon");
        bytes[exefs_base + 8..exefs_base + 12].copy_from_slice(&0u32.to_le_bytes());
        bytes[exefs_base + 12..exefs_base + 16].copy_from_slice(&(smdh_size as u32).to_le_bytes());
        bytes[exefs_base + 0x200..exefs_base + 0x200 + smdh_size].copy_from_slice(&smdh);
        fs::write(&path, bytes).unwrap();

        let profile = inspect_payload_profile(&path).unwrap();
        assert_eq!(profile.platform, "3ds");
        assert_eq!(profile.metadata["name"], "Test 3DS");
        assert_eq!(profile.metadata["developer"], "Nintendo");
        assert_eq!(profile.metadata["serial"], "CTR-P-TE");
        assert_eq!(profile.covers.len(), 2);
        assert!(profile
            .covers
            .iter()
            .all(|cover| cover.bytes.starts_with(crate::PNG_SIGNATURE)));
    }

    #[test]
    fn reads_gamecube_banner_from_gcm_fst() {
        let root = tempdir().unwrap();
        let path = root.path().join("sample.gcm");
        let fst_offset = 0x1000usize;
        let banner_offset = 0x2000usize;
        let mut banner = vec![0u8; super::GAMECUBE_BANNER_SIZE];
        banner[..4].copy_from_slice(b"BNR1");
        banner[super::GAMECUBE_BANNER_METADATA_OFFSET + 0x40
            ..super::GAMECUBE_BANNER_METADATA_OFFSET + 0x40 + 10]
            .copy_from_slice(b"Test Cube\0");
        banner[super::GAMECUBE_BANNER_METADATA_OFFSET + 0x80
            ..super::GAMECUBE_BANNER_METADATA_OFFSET + 0x80 + 8]
            .copy_from_slice(b"Nintendo");
        // One opaque white RGB5A3 pixel repeated through the banner image.
        let image_end = super::GAMECUBE_BANNER_IMAGE_OFFSET
            + super::GAMECUBE_BANNER_IMAGE_WIDTH * super::GAMECUBE_BANNER_IMAGE_HEIGHT * 2;
        for pixel in banner[super::GAMECUBE_BANNER_IMAGE_OFFSET..image_end].chunks_exact_mut(2) {
            pixel.copy_from_slice(&0xffff_u16.to_be_bytes());
        }
        let fst_size = 2 * super::GAMECUBE_FST_ENTRY_SIZE + b"opening.bnr\0".len();
        let mut bytes = vec![0u8; banner_offset + banner.len()];
        bytes[..6].copy_from_slice(b"GM8E01");
        bytes[0x1c..0x20].copy_from_slice(b"\xc23\x9f\x3d");
        bytes[super::GAMECUBE_FST_HEADER_OFFSET..super::GAMECUBE_FST_HEADER_OFFSET + 4]
            .copy_from_slice(&(fst_offset as u32).to_be_bytes());
        bytes[super::GAMECUBE_FST_HEADER_OFFSET + 4..super::GAMECUBE_FST_HEADER_OFFSET + 8]
            .copy_from_slice(&(fst_size as u32).to_be_bytes());
        bytes[fst_offset..fst_offset + 4].copy_from_slice(&0x8000_0000_u32.to_be_bytes());
        bytes[fst_offset + 8..fst_offset + 12].copy_from_slice(&2_u32.to_be_bytes());
        bytes[fst_offset + 12..fst_offset + 16].copy_from_slice(&0_u32.to_be_bytes());
        bytes[fst_offset + 16..fst_offset + 20]
            .copy_from_slice(&(banner_offset as u32).to_be_bytes());
        bytes[fst_offset + 20..fst_offset + 24]
            .copy_from_slice(&(banner.len() as u32).to_be_bytes());
        bytes[fst_offset + 24..fst_offset + 24 + b"opening.bnr\0".len()]
            .copy_from_slice(b"opening.bnr\0");
        bytes[banner_offset..banner_offset + banner.len()].copy_from_slice(&banner);
        fs::write(&path, bytes).unwrap();

        let profile = inspect_payload_profile(&path).unwrap();
        assert_eq!(profile.platform, "gamecube");
        assert_eq!(profile.metadata["name"], "Test Cube");
        assert_eq!(profile.metadata["developer"], "Nintendo");
        assert_eq!(profile.metadata["serial"], "GM8E01");
        assert_eq!(profile.covers.len(), 1);
        assert!(profile.covers[0].bytes.starts_with(crate::PNG_SIGNATURE));

        let iso_path = root.path().join("sample.iso");
        fs::copy(&path, &iso_path).unwrap();
        let iso_profile = inspect_payload_profile(&iso_path).unwrap();
        assert_eq!(iso_profile.platform, "gamecube");
        assert_eq!(iso_profile.metadata["name"], "Test Cube");
    }
}
