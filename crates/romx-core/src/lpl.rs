use crate::{
    classify_gb_payload, crc32, format_id_for_extension, launch_format_id_for_extension,
    normalize_cover_path, normalize_crc32, pack_path_with_metadata_options, platform_id_for_name,
    read_path, required_metadata, PackOptions, RomxError, SPEC_VERSION,
};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};

const SUPPORTED_FORMATS: &[&str] = &[
    "gb", "gbc", "gba", "nes", "fds", "sfc", "smc", "nds", "3ds", "cci", "cia", "md", "gen", "smd",
    "bin",
];
const COVER_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp"];

/// Fields understood by RetroArch's JSON playlist reader/writer at the
/// playlist root. `items` is included so callers can use this list when
/// filtering a complete document; generated documents always write it last.
pub const OFFICIAL_LPL_ROOT_FIELDS: &[&str] = &[
    "version",
    "default_core_path",
    "default_core_name",
    "base_content_directory",
    "label_display_mode",
    "right_thumbnail_mode",
    "left_thumbnail_mode",
    "thumbnail_match_mode",
    "sort_mode",
    "scan_content_dir",
    "scan_file_exts",
    "scan_dat_file_path",
    "scan_database_name",
    "scan_search_recursively",
    "scan_search_archives",
    "scan_filter_dat_content",
    "scan_omit_db_ref",
    "scan_overwrite_playlist",
    "scan_db_usage",
    "items",
];

/// Fields understood by RetroArch's JSON playlist item parser/writer. The
/// runtime and last-played fields are emitted by RetroArch's history writer,
/// while subsystem fields are emitted for multi-content entries.
pub const OFFICIAL_LPL_ITEM_FIELDS: &[&str] = &[
    "path",
    "label",
    "core_path",
    "core_name",
    "crc32",
    "db_name",
    "entry_slot",
    "subsystem_ident",
    "subsystem_name",
    "subsystem_roms",
    "runtime_hours",
    "runtime_minutes",
    "runtime_seconds",
    "last_played_year",
    "last_played_month",
    "last_played_day",
    "last_played_hour",
    "last_played_minute",
    "last_played_second",
];

/// ROMX metadata keys that an extended `.lplx` item may carry. These are
/// copied into the ROMX metadata object during import; playlist-only keys
/// remain in the import plan/manifest and are never embedded in ROMX.
pub const ROMX_LPLX_METADATA_FIELDS: &[&str] = &[
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

/// Reserved item key containing ROMX-only metadata in an extended playlist.
/// Official RetroArch item fields stay at the item root unchanged.
pub const LPLX_METADATA_KEY: &str = "metadata";

pub fn is_official_lpl_root_field(key: &str) -> bool {
    OFFICIAL_LPL_ROOT_FIELDS.contains(&key)
}

pub fn is_official_lpl_item_field(key: &str) -> bool {
    OFFICIAL_LPL_ITEM_FIELDS.contains(&key)
}

fn is_lplx(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("lplx"))
}

#[derive(Debug, Clone)]
pub struct ImportLplOptions {
    pub rom_root: Option<PathBuf>,
    pub cover_root: Option<PathBuf>,
    pub force_rom_dir: Option<PathBuf>,
    pub force_cover_dir: Option<PathBuf>,
    pub cover_set: String,
    pub skip_missing: bool,
    /// Write generated ROMX and manifest files with a `.tmp` suffix so a
    /// caller can atomically commit each output after validation.
    pub temporary_output: bool,
    /// Optional database lookup CRC32 to store in every imported metadata
    /// object. Without it, each ROM's original bytes are hashed.
    pub crc32_override: Option<String>,
    /// Optional exact output resolution for imported covers.
    pub cover_target: Option<(u32, u32)>,
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
            temporary_output: false,
            crc32_override: None,
            cover_target: None,
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
    /// Formal ROMX metadata supplied by an extended `.lplx` item. For a
    /// normal `.lpl` this contains only the required generated fields.
    pub romx_metadata: Value,
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
    /// Legacy compatibility option. Cover files are still written to
    /// `cover_dir`, but no cover-path key is emitted because RetroArch's
    /// official playlist item format does not define one.
    pub lpl_cover_prefix: Option<String>,
    pub cover_set: String,
    /// Write every generated file with a `.tmp` suffix and invoke the output
    /// callback before processing the next file.
    pub temporary_output: bool,
}

impl Default for ExportLplOptions {
    fn default() -> Self {
        Self {
            playlist_name: None,
            lpl_path: None,
            rom_dir: None,
            cover_dir: None,
            lpl_rom_prefix: None,
            lpl_cover_prefix: None,
            cover_set: "Named_Snaps".into(),
            temporary_output: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportLplReport {
    pub total_items: usize,
    pub exported: usize,
    pub skipped: usize,
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
    if relative_to_lpl.exists() {
        relative_to_lpl
    } else {
        candidate
    }
}

fn resolve_lpl_rom_path(lpl_path: &Path, value: &str) -> PathBuf {
    if value.starts_with("/roms/") {
        // <content-root>/retroarch/playlists/<name>.lpl -> <content-root>/roms/...
        if let Some(content_root) = lpl_path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::parent)
        {
            let candidate = content_root.join(virtual_relative_path(value));
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    resolve_lpl_path(lpl_path, value)
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
    for key in OFFICIAL_LPL_ITEM_FIELDS {
        if matches!(*key, "path" | "label") {
            continue;
        }
        if let Some(value) = item.get(*key) {
            extension.insert((*key).to_owned(), value.clone());
        }
    }
    Value::Object(extension)
}

fn supported_platform(value: Option<&Value>) -> Option<&str> {
    let value = value?.as_str()?;
    matches!(
        value,
        "gb" | "gbc" | "gba" | "nes" | "snes" | "nds" | "3ds" | "genesis"
    )
    .then_some(value)
}

fn supported_payload_format(value: Option<&Value>) -> Option<&str> {
    let value = value?.as_str()?;
    SUPPORTED_FORMATS.contains(&value).then_some(value)
}

fn lplx_metadata_value<'a>(item: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    item.get(LPLX_METADATA_KEY)
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get(key))
        .or_else(|| {
            // Read old un-namespaced LPLX files for compatibility, but never
            // generate that shape. Official LPL keys remain authoritative at
            // the item root.
            (!is_official_lpl_item_field(key))
                .then(|| item.get(key))
                .flatten()
        })
}

fn lplx_romx_metadata(
    lpl_path: &Path,
    item: &Map<String, Value>,
    label: &str,
    platform: &str,
    payload_format: &str,
) -> Value {
    let mut metadata = required_metadata(label, platform, payload_format);
    if !is_lplx(lpl_path) {
        return metadata;
    }
    let object = metadata
        .as_object_mut()
        .expect("required metadata is an object");
    for key in ROMX_LPLX_METADATA_FIELDS {
        match *key {
            // crc32 is an official LPL identity field. It may be stored in
            // temporary LPLX metadata with an identity suffix, but final
            // ROMX metadata always receives the CRC calculated from the ROM.
            "schema_version" | "name" | "platform" | "payload_format" | "crc32" => continue,
            _ => {
                if let Some(value) = lplx_metadata_value(item, key) {
                    object.insert((*key).to_owned(), value.clone());
                }
            }
        }
    }
    if let Some(name) = lplx_metadata_value(item, "name")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        object.insert("name".into(), Value::String(name.to_owned()));
    }
    // `label` and `crc32` are official LPL fields and stay at the item root,
    // but their corresponding ROMX values still participate in conversion.
    if !object.contains_key("crc32") {
        if let Some(("crc32", value)) = lpl_identity(item.get("crc32")) {
            object.insert("crc32".into(), Value::String(value));
        }
    }
    if !object.contains_key("serial") {
        if let Some(("serial", value)) = lpl_identity(item.get("crc32")) {
            object.insert("serial".into(), Value::String(value));
        }
    }
    object.insert("schema_version".into(), Value::String(SPEC_VERSION.into()));
    metadata
}

fn official_lpl_settings(document: &Value) -> Map<String, Value> {
    document
        .as_object()
        .map(|object| {
            OFFICIAL_LPL_ROOT_FIELDS
                .iter()
                .filter(|key| **key != "items")
                .filter_map(|key| {
                    object
                        .get(*key)
                        .map(|value| ((*key).to_owned(), value.clone()))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn manifest_lplx_item(
    item: &PlannedLplItem,
    expected_filename: &str,
    output_filename: Option<&str>,
) -> Value {
    let mut manifest_item = Map::new();
    manifest_item.insert(
        "romx_filename".into(),
        Value::String(expected_filename.to_owned()),
    );
    if let Some(output_filename) = output_filename {
        manifest_item.insert(
            "output_filename".into(),
            Value::String(output_filename.to_owned()),
        );
    }
    manifest_item.insert(
        "source_path".into(),
        Value::String(item.source_path.clone()),
    );
    manifest_item.insert("label".into(), Value::String(item.label.clone()));
    if let Some(retroarch) = item.retroarch.as_object() {
        for (key, value) in retroarch {
            if is_official_lpl_item_field(key) && key != "path" && key != "label" {
                manifest_item.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(manifest_item)
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
        return first_cover_file(directory, &[file_stem(rom_path), label.to_owned()]);
    }
    for key in ["cover_path", "thumbnail_path", "cover", "thumbnail"] {
        if let Some(Value::String(value)) = item.get(key) {
            let candidate = resolve_lpl_path(lpl_path, value);
            if candidate.is_file() {
                return Some(candidate);
            }
            if candidate.is_dir() {
                if let Some(cover) =
                    first_cover_file(&candidate, &[file_stem(rom_path), label.to_owned()])
                {
                    return Some(cover);
                }
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
    first_cover_file(&directory, &[file_stem(rom_path), label.to_owned()])
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

fn platform_from_database_name(database: &str) -> Option<&'static str> {
    let name = database.to_ascii_lowercase();
    [
        ("game boy advance", "gba"),
        ("game boy color", "gbc"),
        ("game boy", "gb"),
        ("nintendo ds", "nds"),
        ("nintendo 3ds", "3ds"),
        ("super nintendo", "snes"),
        ("entertainment system", "nes"),
        ("mega drive", "genesis"),
        ("genesis", "genesis"),
    ]
    .into_iter()
    .find_map(|(marker, platform)| name.contains(marker).then_some(platform))
}

fn lpl_platform(item: &Map<String, Value>, payload_format: &str, playlist: &str) -> String {
    supported_platform(lplx_metadata_value(item, "platform"))
        .or_else(|| {
            item.get("db_name")
                .and_then(Value::as_str)
                .and_then(platform_from_database_name)
        })
        .unwrap_or_else(|| platform_for(payload_format, playlist))
        .to_owned()
}

fn database_name(platform: &str) -> &'static str {
    match platform {
        "gb" => "Nintendo - Game Boy.lpl",
        "gbc" => "Nintendo - Game Boy Color.lpl",
        "gba" => "Nintendo - Game Boy Advance.lpl",
        "nes" => "Nintendo - Nintendo Entertainment System.lpl",
        "snes" => "Nintendo - Super Nintendo Entertainment System.lpl",
        "nds" => "Nintendo - Nintendo DS.lpl",
        "3ds" => "Nintendo - Nintendo 3DS.lpl",
        "genesis" => "Sega - Mega Drive - Genesis.lpl",
        _ => "",
    }
}

fn virtual_relative_path(value: &str) -> PathBuf {
    PathBuf::from(value.trim_start_matches('/'))
}

fn first_cover_file(directory: &Path, stems: &[String]) -> Option<PathBuf> {
    for stem in stems {
        for extension in COVER_EXTENSIONS {
            let candidate = directory.join(format!("{stem}.{extension}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
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
            resolve_lpl_rom_path(lpl_path, source_path)
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
            if options.skip_missing {
                skipped += 1;
                continue;
            }
            return Err(RomxError::Invalid(format!(
                "unsupported ROM extension in LPL item {index}: {}",
                rom_path
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default()
            )));
        }
        if matches!(payload_format.as_str(), "gb" | "gbc") {
            let rom = match fs::read(&rom_path) {
                Ok(rom) => rom,
                Err(error) if options.skip_missing => {
                    skipped += 1;
                    let _ = error;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            payload_format = match classify_gb_payload(&rom, Some(&payload_format)) {
                Ok(format) => format.to_owned(),
                Err(error) if options.skip_missing => {
                    skipped += 1;
                    let _ = error;
                    continue;
                }
                Err(error) => return Err(error),
            };
        }
        if is_lplx(lpl_path) {
            if let Some(declared_format) =
                supported_payload_format(lplx_metadata_value(item, "payload_format"))
            {
                payload_format = declared_format.to_owned();
            }
        }
        let stem = file_stem(&rom_path);
        let label = value_label(item.get("label"), &stem);
        let cover_path = cover_from_lpl(lpl_path, &playlist, item, &rom_path, &label, options);
        let platform = lpl_platform(item, &payload_format, &playlist);
        let romx_metadata = lplx_romx_metadata(
            lpl_path,
            item,
            lplx_metadata_value(item, "name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(&label),
            &platform,
            &payload_format,
        );
        items.push(PlannedLplItem {
            index,
            source_path: source_path.to_owned(),
            rom_path,
            cover_path,
            label,
            platform,
            payload_format,
            romx_metadata,
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

pub fn import_lpl(
    lpl_path: &Path,
    output_dir: &Path,
    options: &ImportLplOptions,
) -> Result<ImportLplReport, RomxError> {
    import_lpl_with_progress(
        lpl_path,
        output_dir,
        options,
        false,
        |_current, _total, _imported, _skipped| {},
        || false,
    )
}

/// Import an LPL while reporting item progress and allowing the caller to
/// cancel. When `continue_on_error` is true, an item that cannot be packed is
/// counted as skipped and the remaining items continue to be processed.
pub fn import_lpl_with_progress<F, C>(
    lpl_path: &Path,
    output_dir: &Path,
    options: &ImportLplOptions,
    continue_on_error: bool,
    progress: F,
    is_cancelled: C,
) -> Result<ImportLplReport, RomxError>
where
    F: FnMut(usize, usize, usize, usize),
    C: FnMut() -> bool,
{
    import_lpl_with_error_handling(
        lpl_path,
        output_dir,
        options,
        progress,
        is_cancelled,
        |_index, _error| continue_on_error,
    )
}

/// Import an LPL with progress, cancellation, and per-item error handling.
/// The error callback returns `true` to skip the failed item and continue, or
/// `false` to stop immediately. This allows a GUI to ask the user what to do
/// when an item fails without coupling the core to any UI toolkit.
pub fn import_lpl_with_error_handling<F, C, E>(
    lpl_path: &Path,
    output_dir: &Path,
    options: &ImportLplOptions,
    progress: F,
    is_cancelled: C,
    on_error: E,
) -> Result<ImportLplReport, RomxError>
where
    F: FnMut(usize, usize, usize, usize),
    C: FnMut() -> bool,
    E: FnMut(usize, &RomxError) -> bool,
{
    import_lpl_with_output_handling(
        lpl_path,
        output_dir,
        options,
        progress,
        is_cancelled,
        on_error,
        |path| Ok(Some(path.to_owned())),
    )
}

/// Import an LPL and invoke `on_output` immediately after each ROMX (or
/// temporary ROMX) has been completely written. The callback runs on the
/// importing thread, so callers can atomically commit, rename, or remove the
/// just-created file before the next item is processed. Return `Some(path)`
/// for a committed output path or `None` when the item was intentionally
/// skipped (for example, after an output collision prompt).
pub fn import_lpl_with_output_handling<F, C, E, O>(
    lpl_path: &Path,
    output_dir: &Path,
    options: &ImportLplOptions,
    mut progress: F,
    mut is_cancelled: C,
    mut on_error: E,
    mut on_output: O,
) -> Result<ImportLplReport, RomxError>
where
    F: FnMut(usize, usize, usize, usize),
    C: FnMut() -> bool,
    E: FnMut(usize, &RomxError) -> bool,
    O: FnMut(&Path) -> Result<Option<PathBuf>, RomxError>,
{
    let plan = plan_lpl_import(lpl_path, options)?;
    fs::create_dir_all(output_dir)?;
    let mut output_files = Vec::with_capacity(plan.items.len());
    let mut manifest_items = Vec::with_capacity(plan.items.len());
    let mut skipped = plan.skipped;
    progress(0, plan.total_items, 0, skipped);

    for item in &plan.items {
        if is_cancelled() {
            return Err(RomxError::Cancelled);
        }
        let result = (|| -> Result<Option<PathBuf>, RomxError> {
            let mut metadata = item.romx_metadata.clone();
            if let Some(identity) = lpl_identity(
                item.retroarch
                    .get("crc32")
                    .or_else(|| item.retroarch.get("source_crc32")),
            ) {
                if identity.0 == "serial" {
                    metadata
                        .as_object_mut()
                        .expect("required metadata is an object")
                        .insert("serial".into(), Value::String(identity.1));
                }
            }
            let cover = item
                .cover_path
                .as_deref()
                .map(|path| normalize_cover_path(path, options.cover_target))
                .transpose()?;
            let expected_filename = planned_romx_filename(item);
            let filename = if options.temporary_output {
                format!("{expected_filename}.tmp")
            } else {
                expected_filename.clone()
            };
            let output_path = output_dir.join(filename);
            let pack_options = PackOptions {
                crc32_override: options.crc32_override.clone(),
                platform_id: platform_id_for_name(&item.platform),
                entry_format_id: format_id_for_extension(&item.payload_format),
                launch_format_id: launch_format_id_for_extension(&item.payload_format),
                ..Default::default()
            };
            pack_path_with_metadata_options(
                &item.rom_path,
                Some(&metadata),
                cover.as_deref(),
                &output_path,
                &pack_options,
            )?;
            let committed = on_output(&output_path)?;
            if let Some(committed_path) = committed.as_deref() {
                manifest_items.push(manifest_lplx_item(
                    item,
                    &expected_filename,
                    committed_path.file_name().and_then(|value| value.to_str()),
                ));
            }
            Ok(committed)
        })();
        match result {
            Ok(Some(output_path)) => {
                output_files.push(output_path);
                progress(item.index, plan.total_items, output_files.len(), skipped);
            }
            Ok(None) => progress(item.index, plan.total_items, output_files.len(), skipped),
            Err(error) if on_error(item.index, &error) => {
                skipped += 1;
                progress(item.index, plan.total_items, output_files.len(), skipped);
                continue;
            }
            Err(error) => return Err(error),
        }
    }

    let manifest_path = output_dir.join(if options.temporary_output {
        "manifest.json.tmp"
    } else {
        "manifest.json"
    });
    let lpl_document = read_lpl_document(lpl_path)?;
    let settings = official_lpl_settings(&lpl_document);
    let manifest = json!({
        "source_lpl": plan.source_lpl.to_string_lossy(),
        "playlist": plan.playlist,
        "items": plan.total_items,
        "imported": output_files.len(),
        "lpl_settings": settings,
        "lplx_items": manifest_items,
    });
    write_json(&manifest_path, &manifest)?;
    Ok(ImportLplReport {
        total_items: plan.total_items,
        imported: output_files.len(),
        skipped,
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
            .is_some_and(|extension| extension.eq_ignore_ascii_case("romx"))
        {
            output.push(path);
        }
    }
    Ok(())
}

fn numeric_path_cmp(left: &Path, right: &Path) -> std::cmp::Ordering {
    let name = |path: &Path| {
        path.file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned())
    };
    let left_name = name(left);
    let right_name = name(right);
    let number = |value: &str| {
        value
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse::<u64>()
            .ok()
    };
    match (number(&left_name), number(&right_name)) {
        (Some(left_number), Some(right_number)) => left_number
            .cmp(&right_number)
            .then_with(|| left_name.to_lowercase().cmp(&right_name.to_lowercase())),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left_name.to_lowercase().cmp(&right_name.to_lowercase()),
    }
}

fn read_manifest(romx_dir: &Path) -> Option<Value> {
    let bytes = fs::read(romx_dir.join("manifest.json")).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn playlist_from_manifest(romx_dir: &Path) -> Option<String> {
    read_manifest(romx_dir)?
        .get("playlist")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn manifest_lpl_settings(romx_dir: &Path) -> Map<String, Value> {
    read_manifest(romx_dir)
        .and_then(|manifest| manifest.get("lpl_settings").cloned())
        .map(|settings| official_lpl_settings(&settings))
        .unwrap_or_default()
}

fn manifest_lpl_item(romx_dir: &Path, romx_path: &Path) -> Option<Map<String, Value>> {
    let filename = romx_path.file_name()?.to_str()?;
    let manifest = read_manifest(romx_dir)?;
    manifest
        .get("lplx_items")
        .and_then(Value::as_array)
        .and_then(|items| {
            items.iter().find_map(|item| {
                let object = item.as_object()?;
                let matches = ["romx_filename", "output_filename"]
                    .into_iter()
                    .filter_map(|key| object.get(key).and_then(Value::as_str))
                    .any(|value| value == filename);
                matches.then(|| object.clone())
            })
        })
}

fn safe_filename(label: &str) -> String {
    let value = label.replace(['/', '\\', '\0'], "_").trim().to_owned();
    if value.is_empty() {
        "untitled".into()
    } else {
        value
    }
}

fn planned_romx_filename(item: &PlannedLplItem) -> String {
    let stem = item
        .rom_path
        .file_stem()
        .and_then(|value| value.to_str())
        .map(safe_filename)
        .unwrap_or_else(|| "untitled".into());
    format!("{stem}.romx")
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

fn export_sources(source: &Path) -> Result<Vec<PathBuf>, RomxError> {
    let mut files = Vec::new();
    if source.is_file() {
        files.push(source.to_owned());
    } else if source.is_dir() {
        collect_romx_files(source, &mut files)?;
    } else {
        return Err(RomxError::Invalid(format!(
            "ROMX source does not exist: {}",
            source.display()
        )));
    }
    files.sort_by(|left, right| numeric_path_cmp(left, right));
    if files.is_empty() {
        return Err(RomxError::Invalid(format!(
            "no ROMX files found in {}",
            source.display()
        )));
    }
    Ok(files)
}

fn temporary_output_path(path: &Path, temporary: bool) -> PathBuf {
    if !temporary {
        return path.to_owned();
    }
    let filename = path
        .file_name()
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| "output".into());
    path.with_file_name(format!("{filename}.tmp"))
}

fn joined_lpl_path(prefix: &str, filename: &str) -> String {
    format!("{}/{}", prefix.trim_end_matches(['/', '\\']), filename)
}

pub fn export_lpl(
    source: &Path,
    output_root: &Path,
    options: &ExportLplOptions,
) -> Result<ExportLplReport, RomxError> {
    export_lpl_with_output_handling(
        source,
        output_root,
        options,
        |_current, _total, _exported, _skipped| {},
        || false,
        |_index, _error| false,
        |path| Ok(Some(path.to_owned())),
    )
}

/// Export one ROMX file or all ROMX files below a directory. Each generated
/// ROM, cover, and LPL file is passed to `on_output` immediately after it is
/// completely written. The callback can atomically commit a temporary file
/// and return its final path, or return `None` to skip the current item.
pub fn export_lpl_with_output_handling<F, C, E, O>(
    source: &Path,
    output_root: &Path,
    options: &ExportLplOptions,
    mut progress: F,
    mut is_cancelled: C,
    mut on_error: E,
    mut on_output: O,
) -> Result<ExportLplReport, RomxError>
where
    F: FnMut(usize, usize, usize, usize),
    C: FnMut() -> bool,
    E: FnMut(usize, &RomxError) -> bool,
    O: FnMut(&Path) -> Result<Option<PathBuf>, RomxError>,
{
    let files = export_sources(source)?;
    let total_items = files.len();
    let playlist = options
        .playlist_name
        .clone()
        .or_else(|| {
            if source.is_dir() {
                playlist_from_manifest(source)
            } else {
                None
            }
        })
        .unwrap_or_else(|| file_stem(source));
    let manifest_settings = if source.is_dir() {
        manifest_lpl_settings(source)
    } else {
        Map::new()
    };
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
    let mut skipped = 0usize;
    progress(0, total_items, 0, skipped);

    for (position, romx_path) in files.iter().enumerate() {
        let index = position + 1;
        if is_cancelled() {
            return Err(RomxError::Cancelled);
        }
        let result = (|| -> Result<Option<Value>, RomxError> {
            let document = read_path(romx_path)?;
            let manifest_item = if source.is_dir() {
                manifest_lpl_item(source, romx_path)
            } else {
                None
            };
            let entry_format_id = document
                .entries
                .iter()
                .find(|entry| entry.entrypoint)
                .map(|entry| entry.format_id)
                .unwrap_or(0);
            let payload_format = crate::format_extension(entry_format_id).unwrap_or("bin");
            let source_stem = romx_path
                .file_stem()
                .and_then(|value| value.to_str())
                .map(safe_filename)
                .unwrap_or_else(|| "untitled".into());
            let rom_target = rom_dir.join(format!("{source_stem}.{payload_format}"));
            let staged_rom = temporary_output_path(&rom_target, options.temporary_output);
            fs::write(&staged_rom, &document.rom)?;
            let Some(committed_rom) = on_output(&staged_rom)? else {
                return Ok(None);
            };
            let rom_filename = committed_rom
                .file_name()
                .and_then(|value| value.to_str())
                .ok_or_else(|| RomxError::Invalid("exported ROM filename is invalid".into()))?;
            let label = document
                .metadata
                .as_ref()
                .and_then(|value| value.get("name"))
                .and_then(Value::as_str)
                .unwrap_or(&source_stem);
            let _committed_cover = if let Some(cover) = document.cover.as_deref() {
                // Keep the thumbnail basename identical to the exported ROM
                // basename. Do not use the metadata label here: labels are
                // often localized and RetroArch thumbnail lookup is filename
                // based.
                let cover_stem = committed_rom
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .map(safe_filename)
                    .unwrap_or_else(|| source_stem.clone());
                let cover_target = cover_dir.join(format!("{cover_stem}.png"));
                let staged_cover = temporary_output_path(&cover_target, options.temporary_output);
                fs::write(&staged_cover, cover)?;
                on_output(&staged_cover)?
            } else {
                None
            };
            let item_path = joined_lpl_path(&prefix, rom_filename);
            let lookup_crc = document
                .metadata
                .as_ref()
                .and_then(|value| value.get("crc32"))
                .and_then(Value::as_str)
                .and_then(|value| normalize_crc32(value).ok())
                .unwrap_or_else(|| crc32(&document.rom));
            let platform =
                crate::platform_name_from_id(document.footer.platform_id).unwrap_or_default();
            let mut item = Map::new();
            item.insert("path".into(), Value::String(item_path));
            let label = manifest_item
                .as_ref()
                .and_then(|value| value.get("label"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or(label);
            item.insert("label".into(), Value::String(label.to_owned()));
            let core_path = manifest_item
                .as_ref()
                .and_then(|value| value.get("core_path"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("DETECT");
            let core_name = manifest_item
                .as_ref()
                .and_then(|value| value.get("core_name"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .unwrap_or("DETECT");
            item.insert("core_path".into(), Value::String(core_path.into()));
            item.insert("core_name".into(), Value::String(core_name.into()));
            item.insert("crc32".into(), Value::String(format!("{}|crc", lookup_crc)));
            let db_name = manifest_item
                .as_ref()
                .and_then(|value| value.get("db_name"))
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| database_name(platform).to_owned());
            item.insert("db_name".into(), Value::String(db_name));
            if let Some(manifest_item) = manifest_item.as_ref() {
                for key in OFFICIAL_LPL_ITEM_FIELDS {
                    if matches!(
                        *key,
                        "path" | "label" | "core_path" | "core_name" | "crc32" | "db_name"
                    ) {
                        continue;
                    }
                    if let Some(value) = manifest_item.get(*key) {
                        item.insert((*key).to_owned(), value.clone());
                    }
                }
            }
            Ok(Some(Value::Object(item)))
        })();
        match result {
            Ok(Some(item)) => items.push(item),
            Ok(None) => skipped += 1,
            Err(error) if on_error(index, &error) => skipped += 1,
            Err(error) => return Err(error),
        }
        progress(index, total_items, items.len(), skipped);
    }
    let exported = items.len();
    let mut lpl = manifest_settings;
    lpl.entry("version")
        .or_insert_with(|| Value::String("1.5".into()));
    lpl.entry("default_core_path")
        .or_insert_with(|| Value::String("DETECT".into()));
    lpl.entry("default_core_name")
        .or_insert_with(|| Value::String("DETECT".into()));
    lpl.entry("label_display_mode")
        .or_insert_with(|| Value::from(0));
    lpl.entry("right_thumbnail_mode")
        .or_insert_with(|| Value::from(0));
    lpl.entry("left_thumbnail_mode")
        .or_insert_with(|| Value::from(0));
    lpl.entry("thumbnail_match_mode")
        .or_insert_with(|| Value::from(0));
    lpl.entry("sort_mode").or_insert_with(|| Value::from(0));
    lpl.insert("items".into(), Value::Array(items));
    let lpl = Value::Object(lpl);
    let staged_lpl = temporary_output_path(&lpl_path, options.temporary_output);
    write_json(&staged_lpl, &lpl)?;
    let committed_lpl = on_output(&staged_lpl)?.unwrap_or(lpl_path);
    Ok(ExportLplReport {
        total_items,
        exported,
        skipped,
        playlist,
        lpl_path: committed_lpl,
        rom_dir,
        cover_dir,
    })
}
