use crate::{
    classify_gb_payload, crc32, normalize_crc32, pack_bytes_with_crc32, read_path,
    required_metadata, sha256, RomxError, PNG_SIGNATURE,
};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

const SUPPORTED_FORMATS: &[&str] = &[
    "gb", "gbc", "gba", "nes", "fds", "sfc", "smc", "nds", "3ds", "cci", "cia", "md", "gen", "smd",
    "bin",
];

#[derive(Debug, Clone)]
pub struct ImportLplOptions {
    pub rom_root: Option<PathBuf>,
    pub cover_root: Option<PathBuf>,
    pub force_rom_dir: Option<PathBuf>,
    pub force_cover_dir: Option<PathBuf>,
    pub cover_set: String,
    pub skip_missing: bool,
    /// Optional database lookup CRC32 to store in every imported metadata
    /// object. Without it, each ROM's original bytes are hashed.
    pub crc32_override: Option<String>,
}

impl Default for ImportLplOptions {
    fn default() -> Self {
        Self {
            rom_root: None,
            cover_root: None,
            force_rom_dir: None,
            force_cover_dir: None,
            cover_set: "Named_Snaps".into(),
            skip_missing: false,
            crc32_override: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedLplItem {
    /// One-based position among object entries in the LPL items array.
    pub index: usize,
    pub source_path: String,
    pub rom_path: PathBuf,
    pub cover_path: Option<PathBuf>,
    pub label: String,
    pub platform: String,
    pub payload_format: String,
    /// LPL item fields that are useful for round-tripping but are not
    /// intrinsic game metadata (for example db_name and core_name).
    pub retroarch: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportLplPlan {
    pub source_lpl: PathBuf,
    pub playlist: String,
    pub total_items: usize,
    pub skipped: usize,
    pub items: Vec<PlannedLplItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportLplReport {
    pub total_items: usize,
    pub imported: usize,
    pub skipped: usize,
    pub output_files: Vec<PathBuf>,
    pub manifest_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ExportLplOptions {
    pub playlist_name: Option<String>,
    pub lpl_path: Option<PathBuf>,
    pub rom_dir: Option<PathBuf>,
    pub cover_dir: Option<PathBuf>,
    pub lpl_rom_prefix: Option<String>,
    pub cover_set: String,
}

impl Default for ExportLplOptions {
    fn default() -> Self {
        Self {
            playlist_name: None,
            lpl_path: None,
            rom_dir: None,
            cover_dir: None,
            lpl_rom_prefix: None,
            cover_set: "Named_Snaps".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportLplReport {
    pub exported: usize,
    pub playlist: String,
    pub lpl_path: PathBuf,
    pub rom_dir: PathBuf,
    pub cover_dir: PathBuf,
}

fn read_lpl_document(path: &Path) -> Result<Value, RomxError> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes).map_err(|error| {
        RomxError::Invalid(format!("invalid LPL file {}: {error}", path.display()))
    })
}

fn read_lpl_items(path: &Path) -> Result<Vec<Map<String, Value>>, RomxError> {
    let document = read_lpl_document(path)?;
    let items = document
        .as_object()
        .and_then(|object| object.get("items"))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            RomxError::Invalid(format!("LPL file has no items array: {}", path.display()))
        })?;
    Ok(items.iter().filter_map(Value::as_object).cloned().collect())
}

fn resolve_lpl_path(lpl_path: &Path, value: &str) -> PathBuf {
    let candidate = PathBuf::from(value);
    if candidate.is_absolute() {
        return candidate;
    }
    let relative_to_lpl = lpl_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(&candidate);
    if relative_to_lpl.is_file() {
        relative_to_lpl
    } else {
        candidate
    }
}

fn lpl_identity(value: Option<&Value>) -> Option<(&'static str, String)> {
    let value = value?.as_str()?.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("DETECT") {
        return None;
    }
    let (token, kind) = value.split_once('|')?;
    if token.is_empty() {
        return None;
    }
    match kind.to_ascii_lowercase().as_str() {
        "crc" => normalize_crc32(token).ok().map(|value| ("crc32", value)),
        "serial" => Some(("serial", token.to_owned())),
        _ => None,
    }
}

fn retroarch_metadata(item: &Map<String, Value>) -> Value {
    let mut extension = Map::new();
    for key in ["db_name", "core_name"] {
        if let Some(Value::String(value)) = item.get(key) {
            if !value.is_empty() && value != "DETECT" {
                extension.insert(key.into(), Value::String(value.clone()));
            }
        }
    }
    if let Some(value) = item.get("crc32").and_then(Value::as_str) {
        if !value.is_empty() {
            extension.insert("source_crc32".into(), Value::String(value.to_owned()));
        }
    }
    let known = [
        "path",
        "label",
        "core_path",
        "core_name",
        "crc32",
        "db_name",
    ];
    let mut extra = Map::new();
    for (key, value) in item {
        if !known.contains(&key.as_str()) && !key.ends_with("_path") {
            extra.insert(key.clone(), value.clone());
        }
    }
    if !extra.is_empty() {
        extension.insert("extra".into(), Value::Object(extra));
    }
    Value::Object(extension)
}

fn cover_from_lpl(
    lpl_path: &Path,
    playlist: &str,
    item: &Map<String, Value>,
    rom_path: &Path,
    label: &str,
    options: &ImportLplOptions,
) -> Option<PathBuf> {
    if let Some(directory) = &options.force_cover_dir {
        return first_file(
            directory.join(format!("{}.png", file_stem(rom_path))),
            directory.join(format!("{label}.png")),
        );
    }
    for key in ["cover_path", "thumbnail_path", "cover", "thumbnail"] {
        if let Some(Value::String(value)) = item.get(key) {
            let candidate = resolve_lpl_path(lpl_path, value);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let directory = if let Some(root) = &options.cover_root {
        root.join(playlist).join(&options.cover_set)
    } else {
        lpl_path
            .parent()
            .and_then(Path::parent)
            .unwrap_or_else(|| Path::new("."))
            .join("thumbnails")
            .join(playlist)
            .join(&options.cover_set)
    };
    first_file(
        directory.join(format!("{}.png", file_stem(rom_path))),
        directory.join(format!("{label}.png")),
    )
}

fn value_label(value: Option<&Value>, fallback: &str) -> String {
    match value {
        Some(Value::String(label)) if !label.is_empty() => label.clone(),
        Some(Value::Null) | None => fallback.to_owned(),
        Some(Value::Bool(false)) => fallback.to_owned(),
        Some(Value::Number(number)) if number.as_i64() == Some(0) => fallback.to_owned(),
        Some(other) => other
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| other.to_string()),
    }
}

fn platform_for(payload_format: &str, playlist_name: &str) -> &'static str {
    if payload_format == "gb" {
        return "gb";
    }
    if payload_format == "gbc" {
        return "gbc";
    }
    let name = playlist_name.to_lowercase();
    for (marker, platform) in [
        ("gbc", "gbc"),
        ("gba", "gba"),
        ("3ds", "3ds"),
        ("nds", "nds"),
        ("snes", "snes"),
        ("genesis", "genesis"),
        ("gb", "gb"),
        ("nes", "nes"),
    ] {
        if name.contains(marker) {
            return platform;
        }
    }
    match payload_format {
        "gb" => "gb",
        "gbc" => "gbc",
        "gba" => "gba",
        "nes" | "fds" => "nes",
        "sfc" | "smc" => "snes",
        "nds" => "nds",
        "3ds" | "cci" | "cia" => "3ds",
        "md" | "gen" | "smd" | "bin" => "genesis",
        _ => "gb",
    }
}

fn virtual_relative_path(value: &str) -> PathBuf {
    PathBuf::from(value.trim_start_matches('/'))
}

fn first_file(primary: PathBuf, fallback: PathBuf) -> Option<PathBuf> {
    if primary.is_file() {
        Some(primary)
    } else if fallback.is_file() {
        Some(fallback)
    } else {
        None
    }
}

fn file_stem(path: &Path) -> String {
    path.file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_owned()
}

pub fn plan_lpl_import(
    lpl_path: &Path,
    options: &ImportLplOptions,
) -> Result<ImportLplPlan, RomxError> {
    let source_items = read_lpl_items(lpl_path)?;
    let playlist = file_stem(lpl_path);
    let total_items = source_items.len();
    let mut skipped = 0;
    let mut items = Vec::with_capacity(total_items);

    for (position, item) in source_items.iter().enumerate() {
        let index = position + 1;
        let Some(source_path) = item
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.is_empty())
        else {
            if options.skip_missing {
                skipped += 1;
                continue;
            }
            return Err(RomxError::Invalid(format!("LPL item {index} has no path")));
        };
        let virtual_path = virtual_relative_path(source_path);
        let rom_path = if let Some(directory) = &options.force_rom_dir {
            directory.join(virtual_path.file_name().unwrap_or_default())
        } else if let Some(root) = &options.rom_root {
            root.join(&virtual_path)
        } else {
            resolve_lpl_path(lpl_path, source_path)
        };
        if !rom_path.is_file() {
            if options.skip_missing {
                skipped += 1;
                continue;
            }
            return Err(RomxError::Invalid(format!(
                "ROM not found for LPL item {index}: {}",
                rom_path.display()
            )));
        }
        let mut payload_format = rom_path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_lowercase();
        if !SUPPORTED_FORMATS.contains(&payload_format.as_str()) {
            return Err(RomxError::Invalid(format!(
                "unsupported ROM extension in LPL item {index}: {}",
                rom_path
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
            )));
        }
        if matches!(payload_format.as_str(), "gb" | "gbc") {
            let rom = fs::read(&rom_path)?;
            payload_format = classify_gb_payload(&rom, Some(&payload_format))?.to_owned();
        }
        let stem = file_stem(&rom_path);
        let label = value_label(item.get("label"), &stem);
        let cover_path = cover_from_lpl(lpl_path, &playlist, item, &rom_path, &label, options);
        items.push(PlannedLplItem {
            index,
            source_path: source_path.to_owned(),
            rom_path,
            cover_path,
            label,
            platform: platform_for(&payload_format, &playlist).to_owned(),
            payload_format,
            retroarch: retroarch_metadata(item),
        });
    }

    Ok(ImportLplPlan {
        source_lpl: lpl_path.to_owned(),
        playlist,
        total_items,
        skipped,
        items,
    })
}

fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    if data.starts_with(PNG_SIGNATURE) && data.len() >= 24 && &data[12..16] == b"IHDR" {
        Some((
            u32::from_be_bytes(data[16..20].try_into().ok()?),
            u32::from_be_bytes(data[20..24].try_into().ok()?),
        ))
    } else {
        None
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

pub fn import_lpl(
    lpl_path: &Path,
    output_dir: &Path,
    options: &ImportLplOptions,
) -> Result<ImportLplReport, RomxError> {
    let plan = plan_lpl_import(lpl_path, options)?;
    fs::create_dir_all(output_dir)?;
    let mut output_files = Vec::with_capacity(plan.items.len());

    for item in &plan.items {
        let rom = fs::read(&item.rom_path)?;
        let mut metadata = required_metadata(&item.label, &item.platform, &item.payload_format);
        if let Some(identity) = lpl_identity(item.retroarch.get("source_crc32")) {
            if identity.0 == "serial" {
                metadata
                    .as_object_mut()
                    .expect("required metadata is an object")
                    .insert("serial".into(), Value::String(identity.1));
            }
        }
        if item
            .retroarch
            .as_object()
            .is_some_and(|object| !object.is_empty())
        {
            metadata
                .as_object_mut()
                .expect("required metadata is an object")
                .insert("x-retroarch".into(), item.retroarch.clone());
        }
        let cover = item.cover_path.as_deref().map(fs::read).transpose()?;
        if let Some(cover_bytes) = &cover {
            let mut description = Map::new();
            description.insert("mime_type".into(), Value::String("image/png".into()));
            if let Some((width, height)) = png_dimensions(cover_bytes) {
                description.insert("width".into(), Value::from(width));
                description.insert("height".into(), Value::from(height));
                description.insert("sha256".into(), Value::String(hex(&sha256(cover_bytes))));
            }
            metadata
                .as_object_mut()
                .expect("required metadata is an object")
                .insert("cover".into(), Value::Object(description));
        }
        let bytes = pack_bytes_with_crc32(
            &rom,
            Some(&metadata),
            cover.as_deref(),
            options.crc32_override.as_deref(),
        )?;
        let output_path = output_dir.join(format!("{:06}.{}x", item.index, item.payload_format));
        fs::write(&output_path, bytes)?;
        output_files.push(output_path);
    }

    let manifest_path = output_dir.join("manifest.json");
    let lpl_document = read_lpl_document(lpl_path)?;
    let settings = lpl_document
        .as_object()
        .map(|object| {
            [
                "version",
                "default_core_path",
                "default_core_name",
                "label_display_mode",
                "right_thumbnail_mode",
                "left_thumbnail_mode",
                "thumbnail_match_mode",
                "sort_mode",
            ]
            .into_iter()
            .filter_map(|key| object.get(key).map(|value| (key.to_owned(), value.clone())))
            .collect::<Map<String, Value>>()
        })
        .unwrap_or_default();
    let manifest = json!({
        "source_lpl": plan.source_lpl.to_string_lossy(),
        "playlist": plan.playlist,
        "items": plan.total_items,
        "imported": output_files.len(),
        "lpl_settings": settings,
    });
    write_json(&manifest_path, &manifest)?;
    Ok(ImportLplReport {
        total_items: plan.total_items,
        imported: output_files.len(),
        skipped: plan.skipped,
        output_files,
        manifest_path,
    })
}

fn collect_romx_files(directory: &Path, output: &mut Vec<PathBuf>) -> Result<(), RomxError> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_romx_files(&path, output)?;
        } else if path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|extension| extension.to_lowercase().ends_with('x'))
        {
            output.push(path);
        }
    }
    Ok(())
}

fn playlist_from_manifest(romx_dir: &Path) -> Option<String> {
    let bytes = fs::read(romx_dir.join("manifest.json")).ok()?;
    let manifest: Value = serde_json::from_slice(&bytes).ok()?;
    manifest
        .get("playlist")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn safe_filename(label: &str) -> String {
    let value = label.replace(['/', '\\', '\0'], "_").trim().to_owned();
    if value.is_empty() {
        "untitled".into()
    } else {
        value
    }
}

fn write_json(path: &Path, value: &Value) -> Result<(), RomxError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    fs::write(path, bytes)?;
    Ok(())
}

pub fn export_lpl(
    romx_dir: &Path,
    output_root: &Path,
    options: &ExportLplOptions,
) -> Result<ExportLplReport, RomxError> {
    let mut files = Vec::new();
    collect_romx_files(romx_dir, &mut files)?;
    files.sort();
    if files.is_empty() {
        return Err(RomxError::Invalid(format!(
            "no ROMX files found in {}",
            romx_dir.display()
        )));
    }
    let playlist = options
        .playlist_name
        .clone()
        .or_else(|| playlist_from_manifest(romx_dir))
        .unwrap_or_else(|| file_stem(romx_dir));
    let rom_dir = options
        .rom_dir
        .clone()
        .unwrap_or_else(|| output_root.join("roms").join(&playlist));
    let cover_dir = options.cover_dir.clone().unwrap_or_else(|| {
        output_root
            .join("thumbnails")
            .join(&playlist)
            .join(&options.cover_set)
    });
    let lpl_path = options.lpl_path.clone().unwrap_or_else(|| {
        output_root
            .join("playlists")
            .join(format!("{playlist}.lpl"))
    });
    fs::create_dir_all(&rom_dir)?;
    fs::create_dir_all(&cover_dir)?;
    let prefix = options
        .lpl_rom_prefix
        .clone()
        .unwrap_or_else(|| format!("/roms/{playlist}"));
    let mut items = Vec::with_capacity(files.len());

    for (position, romx_path) in files.iter().enumerate() {
        let index = position + 1;
        let document = read_path(romx_path)?;
        let payload_format = document
            .metadata
            .as_ref()
            .and_then(|value| value.get("payload_format"))
            .and_then(Value::as_str)
            .unwrap_or("rom");
        let filename = format!("{index:06}.{payload_format}");
        fs::write(rom_dir.join(&filename), &document.rom)?;
        let label = document
            .metadata
            .as_ref()
            .and_then(|value| value.get("label"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                romx_path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
            });
        if let Some(cover) = document.cover {
            fs::write(
                cover_dir.join(format!("{}.png", safe_filename(label))),
                cover,
            )?;
        }
        let item_path = format!("{}/{}", prefix.trim_end_matches('/'), filename);
        let lookup_crc = document
            .metadata
            .as_ref()
            .and_then(|value| value.get("crc32"))
            .and_then(Value::as_str)
            .and_then(|value| normalize_crc32(value).ok())
            .unwrap_or_else(|| crc32(&document.rom));
        let retroarch = document
            .metadata
            .as_ref()
            .and_then(|value| value.get("x-retroarch"))
            .and_then(Value::as_object);
        let core_name = retroarch
            .and_then(|value| value.get("core_name"))
            .and_then(Value::as_str)
            .unwrap_or("DETECT");
        let db_name = retroarch
            .and_then(|value| value.get("db_name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        items.push(json!({
            "path": item_path,
            "label": label,
            "core_path": "DETECT",
            "core_name": core_name,
            "crc32": format!("{}|crc", lookup_crc),
            "db_name": db_name,
        }));
    }
    let lpl = json!({
        "version": "1.5",
        "default_core_path": "DETECT",
        "default_core_name": "DETECT",
        "label_display_mode": 0,
        "right_thumbnail_mode": 0,
        "left_thumbnail_mode": 0,
        "thumbnail_match_mode": 0,
        "sort_mode": 0,
        "items": items,
    });
    write_json(&lpl_path, &lpl)?;
    Ok(ExportLplReport {
        exported: files.len(),
        playlist,
        lpl_path,
        rom_dir,
        cover_dir,
    })
}
