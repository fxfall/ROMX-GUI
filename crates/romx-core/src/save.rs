//! Platform-aware SAVE discovery and ROMX mutable SAVE inspection.
//!
//! A frontend must not infer save boundaries from directory depth alone.  The
//! platform profile decides whether one object is a single file, a directory
//! tree, or a directory marked by a platform-specific file such as PSP's
//! `PARAM.SFO`.

use crate::{
    crc32_u32, MutableSaveBundle, MutableSaveFile, RomxError, FOOTER_SIZE, RIDX_ENTRY_SIZE,
};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const SAVE_FILE_EXTENSIONS: &[&str] = &[
    "sav", "save", "srm", "dsv", "eep", "eeprom", "ram", "sra", "fla", "flash", "rtc", "mcr",
    "gci", "dat",
];
const PSP_MARKER: &str = "PARAM.SFO";
const MUTABLE_HEADER_SIZE: usize = 4096;
const MUTABLE_HEADER_ENTRY_SIZE_OFFSET: usize = 8;
const MUTABLE_HEADER_ENTRY_COUNT_OFFSET: usize = 12;
const MUTABLE_HEADER_DATA_OFFSET: usize = 32;
const MUTABLE_HEADER_DATA_SIZE: usize = 40;
const MUTABLE_HEADER_CRC_OFFSET: usize = 0x34;
const MUTABLE_ENTRY_SIZE: usize = RIDX_ENTRY_SIZE;
const MUTABLE_ENTRY_ACTIVE: u16 = 1;
const MUTABLE_NAMESPACE_SAVE: u16 = 1;
const MUTABLE_KEY_CAPACITY: usize = 448;
const MUTABLE_BUNDLE_HEADER_SIZE: usize = 64;
const MUTABLE_BUNDLE_ENTRY_SIZE: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveGrouping {
    /// Every recognized file is an independent save object.
    SingleFile,
    /// Each directory directly under the selected root is one save object.
    DirectoryPerSave,
    /// A marker file identifies a complete directory tree as one object.
    MarkerDirectory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveProfile {
    pub platform: String,
    pub payload_format: String,
    pub grouping: SaveGrouping,
    pub marker: Option<String>,
}

/// Return the save boundary policy for a platform/ROM format pair.
pub fn save_profile(platform: &str, payload_format: &str) -> SaveProfile {
    let platform = platform.trim().to_ascii_lowercase();
    let payload_format = payload_format
        .trim()
        .trim_start_matches('.')
        .to_ascii_lowercase();
    let grouping = if platform == "psp" || payload_format == "pbp" {
        SaveGrouping::MarkerDirectory
    } else if matches!(platform.as_str(), "3ds" | "gamecube" | "wii" | "ps2") {
        SaveGrouping::DirectoryPerSave
    } else {
        SaveGrouping::SingleFile
    };
    let marker = matches!(grouping, SaveGrouping::MarkerDirectory).then(|| PSP_MARKER.to_owned());
    SaveProfile {
        platform,
        payload_format,
        grouping,
        marker,
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SaveInventory {
    pub count: usize,
    pub bytes: u64,
    pub files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableSaveFileData {
    pub path: String,
    pub bytes: Vec<u8>,
    pub crc32: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableSaveObject {
    pub slot: usize,
    pub key: String,
    pub files: Vec<MutableSaveFileData>,
    pub data_size: u64,
    pub data_capacity: u64,
    pub generation: u64,
    pub modified_at: u64,
    pub crc32: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutableRegionInfo {
    pub offset: u64,
    pub capacity: u64,
    pub entry_capacity: usize,
    pub data_capacity: u64,
    pub free_slots: usize,
    pub free_bytes: u64,
    pub objects: Vec<MutableSaveObject>,
}

pub fn is_supported_save_file(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            SAVE_FILE_EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

fn sorted_directory_entries(path: &Path) -> Result<Vec<fs::DirEntry>, RomxError> {
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by(|left, right| {
        left.file_name()
            .to_string_lossy()
            .as_bytes()
            .cmp(right.file_name().to_string_lossy().as_bytes())
    });
    Ok(entries)
}

fn relative_path(root: &Path, path: &Path) -> Result<String, RomxError> {
    let relative = path.strip_prefix(root).map_err(|_| {
        RomxError::Invalid(format!("SAVE path is outside its root: {}", path.display()))
    })?;
    let value = relative.to_string_lossy().replace('\\', "/");
    if value.is_empty() {
        return Err(RomxError::Invalid("SAVE path is empty".into()));
    }
    Ok(value)
}

fn collect_regular_files(
    root: &Path,
    directory: &Path,
    output: &mut Vec<MutableSaveFile>,
) -> Result<(), RomxError> {
    for entry in sorted_directory_entries(directory)? {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_regular_files(root, &path, output)?;
        } else if metadata.is_file() {
            output.push(MutableSaveFile {
                path: relative_path(root, &path)?,
                source: path,
            });
        }
    }
    Ok(())
}

fn contains_marker(path: &Path, marker: &str) -> Result<bool, RomxError> {
    Ok(sorted_directory_entries(path)?.into_iter().any(|entry| {
        entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(marker)
    }))
}

fn bundle_key(path: &Path) -> String {
    path.file_stem()
        .or_else(|| path.file_name())
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("save")
        .to_owned()
}

fn validate_object_key(key: &str) -> Result<(), RomxError> {
    if key.is_empty()
        || key.len() > MUTABLE_KEY_CAPACITY
        || key == "."
        || key == ".."
        || key.contains('/')
        || key.contains('\\')
        || key.as_bytes().contains(&0)
    {
        return Err(RomxError::Invalid(format!(
            "invalid mutable SAVE key: {key}"
        )));
    }
    Ok(())
}

fn unique_key(base: String, used: &mut HashSet<String>) -> String {
    let fallback = if base.trim().is_empty() {
        "save"
    } else {
        &base
    };
    let mut candidate = fallback.to_owned();
    let mut suffix = 2usize;
    while !used.insert(candidate.to_ascii_lowercase()) {
        candidate = format!("{fallback} ({suffix})");
        suffix = suffix.saturating_add(1);
    }
    candidate
}

fn push_file_bundle(
    root: &Path,
    path: &Path,
    output: &mut Vec<MutableSaveBundle>,
    used: &mut HashSet<String>,
) -> Result<(), RomxError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || !is_supported_save_file(path) {
        return Ok(());
    }
    output.push(MutableSaveBundle {
        key: unique_key(bundle_key(path), used),
        files: vec![MutableSaveFile {
            path: relative_path(root, path)?,
            source: path.to_owned(),
        }],
    });
    Ok(())
}

fn push_directory_bundle(
    root: &Path,
    directory: &Path,
    output: &mut Vec<MutableSaveBundle>,
    used: &mut HashSet<String>,
) -> Result<(), RomxError> {
    let mut files = Vec::new();
    collect_regular_files(root, directory, &mut files)?;
    if !files.is_empty() {
        output.push(MutableSaveBundle {
            key: unique_key(bundle_key(directory), used),
            files,
        });
    }
    Ok(())
}

fn collect_marker_bundles(
    root: &Path,
    directory: &Path,
    marker: &str,
    output: &mut Vec<MutableSaveBundle>,
    used: &mut HashSet<String>,
) -> Result<(), RomxError> {
    if contains_marker(directory, marker)? {
        // The marker directory is the logical object root.  Keeping paths
        // relative to it lets a selected single PSP savedata directory and a
        // directory containing many savedata directories round-trip through
        // the same API without duplicating the object name on extraction.
        return push_directory_bundle(directory, directory, output, used);
    }
    for entry in sorted_directory_entries(directory)? {
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_marker_bundles(root, &path, marker, output, used)?;
        } else {
            push_file_bundle(root, &path, output, used)?;
        }
    }
    Ok(())
}

/// Discover local save objects using the platform-aware profile.
pub fn detect_save_bundles(
    path: &Path,
    platform: &str,
    payload_format: &str,
) -> Result<Vec<MutableSaveBundle>, RomxError> {
    let metadata = fs::symlink_metadata(path)?;
    let profile = save_profile(platform, payload_format);
    let mut output = Vec::new();
    let mut used = HashSet::new();
    if metadata.is_file() {
        let root = path.parent().unwrap_or(path);
        push_file_bundle(root, path, &mut output, &mut used)?;
    } else if metadata.is_dir() {
        match profile.grouping {
            SaveGrouping::SingleFile => {
                fn walk(
                    root: &Path,
                    path: &Path,
                    output: &mut Vec<MutableSaveBundle>,
                    used: &mut HashSet<String>,
                ) -> Result<(), RomxError> {
                    for entry in sorted_directory_entries(path)? {
                        let child = entry.path();
                        let metadata = fs::symlink_metadata(&child)?;
                        if metadata.file_type().is_symlink() {
                            continue;
                        }
                        if metadata.is_dir() {
                            walk(root, &child, output, used)?;
                        } else {
                            push_file_bundle(root, &child, output, used)?;
                        }
                    }
                    Ok(())
                }
                walk(path, path, &mut output, &mut used)?;
            }
            SaveGrouping::MarkerDirectory => {
                collect_marker_bundles(
                    path,
                    path,
                    profile.marker.as_deref().unwrap_or(PSP_MARKER),
                    &mut output,
                    &mut used,
                )?;
            }
            SaveGrouping::DirectoryPerSave => {
                for entry in sorted_directory_entries(path)? {
                    let child = entry.path();
                    let metadata = fs::symlink_metadata(&child)?;
                    if metadata.file_type().is_symlink() {
                        continue;
                    }
                    if metadata.is_dir() {
                        push_directory_bundle(&child, &child, &mut output, &mut used)?;
                    } else {
                        push_file_bundle(path, &child, &mut output, &mut used)?;
                    }
                }
            }
        }
    } else {
        return Err(RomxError::Invalid(format!(
            "SAVE path is not a file or directory: {}",
            path.display()
        )));
    }
    output.sort_by(|left, right| left.key.as_bytes().cmp(right.key.as_bytes()));
    Ok(output)
}

pub fn inspect_mutable_path(
    path: &Path,
    platform: &str,
    payload_format: &str,
) -> Result<SaveInventory, RomxError> {
    let bundles = detect_save_bundles(path, platform, payload_format)?;
    let mut inventory = SaveInventory {
        count: bundles.len(),
        ..Default::default()
    };
    for bundle in bundles {
        inventory.files = inventory.files.saturating_add(bundle.files.len());
        for file in bundle.files {
            inventory.bytes = inventory
                .bytes
                .saturating_add(fs::metadata(file.source)?.len());
        }
    }
    Ok(inventory)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, RomxError> {
    bytes
        .get(offset..offset + 2)
        .and_then(|value| value.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| RomxError::Invalid("truncated mutable SAVE field".into()))
}
fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, RomxError> {
    bytes
        .get(offset..offset + 4)
        .and_then(|value| value.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| RomxError::Invalid("truncated mutable SAVE field".into()))
}
fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, RomxError> {
    bytes
        .get(offset..offset + 8)
        .and_then(|value| value.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or_else(|| RomxError::Invalid("truncated mutable SAVE field".into()))
}
fn all_zero(bytes: &[u8]) -> bool {
    bytes.iter().all(|value| *value == 0)
}

fn portable_bundle_path(path: &str) -> Result<(), RomxError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.as_bytes().contains(&0)
        || path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(RomxError::Invalid(format!(
            "invalid mutable SAVE path: {path}"
        )));
    }
    Ok(())
}

fn parse_bundle(bytes: &[u8]) -> Result<Vec<MutableSaveFileData>, RomxError> {
    if bytes.len() < MUTABLE_BUNDLE_HEADER_SIZE
        || &bytes[..4] != b"RMBL"
        || read_u16(bytes, 4)? != 1
        || read_u16(bytes, 6)? as usize != MUTABLE_BUNDLE_HEADER_SIZE
        || read_u16(bytes, 8)? != 1
    {
        return Err(RomxError::Invalid(
            "invalid mutable SAVE bundle header".into(),
        ));
    }
    let entry_count = read_u32(bytes, 0x10)? as usize;
    let directory_offset = read_u64(bytes, 0x18)? as usize;
    let path_table_offset = read_u64(bytes, 0x20)? as usize;
    let data_offset = read_u64(bytes, 0x28)? as usize;
    let bundle_size = read_u64(bytes, 0x30)? as usize;
    if directory_offset != MUTABLE_BUNDLE_HEADER_SIZE
        || data_offset > bundle_size
        || bundle_size > bytes.len()
        || entry_count > (bytes.len().saturating_sub(directory_offset)) / MUTABLE_BUNDLE_ENTRY_SIZE
        || path_table_offset < directory_offset + entry_count * MUTABLE_BUNDLE_ENTRY_SIZE
        || path_table_offset > data_offset
    {
        return Err(RomxError::Invalid(
            "invalid mutable SAVE bundle ranges".into(),
        ));
    }
    let stored_crc = read_u32(bytes, 0x38)?;
    let mut header = bytes[..MUTABLE_BUNDLE_HEADER_SIZE].to_vec();
    header[0x38..0x3c].fill(0);
    if crc32_u32(&header) != stored_crc {
        return Err(RomxError::Invalid(
            "mutable SAVE bundle header CRC32 mismatch".into(),
        ));
    }
    let mut output = Vec::with_capacity(entry_count);
    for index in 0..entry_count {
        let base = directory_offset + index * MUTABLE_BUNDLE_ENTRY_SIZE;
        let path_offset = read_u64(bytes, base)? as usize;
        let path_size = read_u32(bytes, base + 8)? as usize;
        let file_offset = read_u64(bytes, base + 0x10)? as usize;
        let file_size = read_u64(bytes, base + 0x18)? as usize;
        let expected_crc = read_u32(bytes, base + 0x20)?;
        if path_offset < path_table_offset
            || path_size > data_offset.saturating_sub(path_offset)
            || file_offset < data_offset
            || file_size > bundle_size.saturating_sub(file_offset)
        {
            return Err(RomxError::Invalid(
                "mutable SAVE bundle entry is out of range".into(),
            ));
        }
        let path = String::from_utf8(bytes[path_offset..path_offset + path_size].to_vec())
            .map_err(|_| RomxError::Invalid("mutable SAVE path is not UTF-8".into()))?;
        portable_bundle_path(&path)?;
        let file_bytes = bytes[file_offset..file_offset + file_size].to_vec();
        if crc32_u32(&file_bytes) != expected_crc {
            return Err(RomxError::Invalid(format!(
                "mutable SAVE file CRC32 mismatch: {path}"
            )));
        }
        output.push(MutableSaveFileData {
            path,
            crc32: format!("{expected_crc:08x}"),
            bytes: file_bytes,
        });
    }
    Ok(output)
}

/// Read the SAVE namespace from an existing ROMX file without reading its ROM payload separately.
pub fn read_mutable_save_objects(path: &Path) -> Result<MutableRegionInfo, RomxError> {
    read_mutable_save_objects_from_bytes(&fs::read(path)?)
}

pub(crate) fn read_mutable_save_objects_from_bytes(
    bytes: &[u8],
) -> Result<MutableRegionInfo, RomxError> {
    let parsed = crate::parse_container(bytes, false)?;
    if let Some(metadata) = parsed.metadata {
        crate::parse_json_strict(metadata)?;
    }
    if let Some(cover) = parsed.cover {
        crate::validate_png_bytes(cover)?;
    }
    read_mutable_save_objects_with_capacity(bytes, parsed.footer.mutable_capacity)
}

pub(crate) fn read_mutable_save_objects_with_capacity(
    bytes: &[u8],
    capacity: u64,
) -> Result<MutableRegionInfo, RomxError> {
    if capacity == 0 {
        return Ok(MutableRegionInfo {
            offset: bytes.len().saturating_sub(FOOTER_SIZE) as u64,
            capacity: 0,
            entry_capacity: 0,
            data_capacity: 0,
            free_slots: 0,
            free_bytes: 0,
            objects: Vec::new(),
        });
    }
    let capacity_usize = usize::try_from(capacity)
        .map_err(|_| RomxError::Invalid("mutable capacity is too large".into()))?;
    if bytes.len() < FOOTER_SIZE + capacity_usize + MUTABLE_HEADER_SIZE {
        return Err(RomxError::Invalid("mutable region is truncated".into()));
    }
    let offset = bytes.len() - FOOTER_SIZE - capacity_usize;
    let header = &bytes[offset..offset + MUTABLE_HEADER_SIZE];
    if &header[..4] != b"RMUT"
        || read_u16(header, 4)? != 1
        || read_u16(header, 6)? as usize != MUTABLE_HEADER_SIZE
        || read_u32(header, MUTABLE_HEADER_ENTRY_SIZE_OFFSET)? as usize != MUTABLE_ENTRY_SIZE
    {
        return Err(RomxError::Invalid(
            "invalid mutable SAVE directory header".into(),
        ));
    }
    let entry_capacity = read_u32(header, MUTABLE_HEADER_ENTRY_COUNT_OFFSET)? as usize;
    let data_offset = read_u64(header, MUTABLE_HEADER_DATA_OFFSET)? as usize;
    let data_capacity = read_u64(header, MUTABLE_HEADER_DATA_SIZE)?;
    if entry_capacity == 0
        || entry_capacity > (capacity_usize - MUTABLE_HEADER_SIZE) / MUTABLE_ENTRY_SIZE
        || data_offset < MUTABLE_HEADER_SIZE + entry_capacity * MUTABLE_ENTRY_SIZE
        || data_capacity as usize > capacity_usize.saturating_sub(data_offset)
    {
        return Err(RomxError::Invalid(
            "invalid mutable SAVE directory ranges".into(),
        ));
    }
    let stored_crc = read_u32(header, MUTABLE_HEADER_CRC_OFFSET)?;
    let mut checked_header = header.to_vec();
    checked_header[MUTABLE_HEADER_CRC_OFFSET..MUTABLE_HEADER_CRC_OFFSET + 4].fill(0);
    if crc32_u32(&checked_header) != stored_crc {
        return Err(RomxError::Invalid(
            "mutable SAVE directory header CRC32 mismatch".into(),
        ));
    }
    let mut objects = Vec::new();
    let mut used_ranges = Vec::<(usize, usize)>::new();
    for slot in 0..entry_capacity {
        let base = offset + MUTABLE_HEADER_SIZE + slot * MUTABLE_ENTRY_SIZE;
        let entry = &bytes[base..base + MUTABLE_ENTRY_SIZE];
        if all_zero(entry) {
            continue;
        }
        let is_ment = &entry[..4] == b"MENT";
        let active = is_ment && read_u16(entry, 4)? == MUTABLE_ENTRY_ACTIVE;
        let namespace = if is_ment { read_u16(entry, 6)? } else { 0 };
        // This API only owns the SAVE namespace. Unknown namespaces such as
        // CHEAT/STATS remain opaque, but their allocated ranges still count
        // against free capacity.
        if !active || namespace != MUTABLE_NAMESPACE_SAVE {
            if active {
                let data_start = read_u64(entry, 0x10)? as usize;
                let data_capacity_entry = read_u64(entry, 0x18)?;
                if data_start >= data_offset
                    && data_start <= data_offset + data_capacity as usize
                    && data_capacity_entry as usize
                        <= data_offset + data_capacity as usize - data_start
                {
                    used_ranges.push((data_start, data_start + data_capacity_entry as usize));
                }
            }
            continue;
        }
        let key_size = read_u32(entry, 0x0c)? as usize;
        if key_size == 0 || key_size > MUTABLE_KEY_CAPACITY {
            return Err(RomxError::Invalid(format!(
                "invalid mutable SAVE key length in slot {slot}"
            )));
        }
        let key = String::from_utf8(entry[0x40..0x40 + key_size].to_vec())
            .map_err(|_| RomxError::Invalid("mutable SAVE key is not UTF-8".into()))?;
        validate_object_key(&key)?;
        // Mutable directory offsets are relative to the start of RMUT, not
        // absolute file offsets.
        let data_start = read_u64(entry, 0x10)? as usize;
        let data_capacity_entry = read_u64(entry, 0x18)?;
        let data_size = read_u64(entry, 0x20)?;
        if data_start < data_offset
            || data_start > data_offset + data_capacity as usize
            || data_capacity_entry as usize > data_offset + data_capacity as usize - data_start
            || data_size > data_capacity_entry
        {
            return Err(RomxError::Invalid(format!(
                "mutable SAVE object range is invalid: {key}"
            )));
        }
        let range = (data_start, data_start + data_capacity_entry as usize);
        if used_ranges
            .iter()
            .any(|(start, end)| range.0 < *end && *start < range.1)
        {
            return Err(RomxError::Invalid(format!(
                "mutable SAVE objects overlap: {key}"
            )));
        }
        used_ranges.push(range);
        let absolute_data_start = offset
            .checked_add(data_start)
            .ok_or_else(|| RomxError::Invalid("mutable SAVE data offset overflows".into()))?;
        let bundle_end = absolute_data_start
            .checked_add(data_size as usize)
            .ok_or_else(|| RomxError::Invalid("mutable SAVE data size overflows".into()))?;
        if bundle_end > offset + capacity_usize {
            return Err(RomxError::Invalid(format!(
                "mutable SAVE object exceeds the mutable region: {key}"
            )));
        }
        let bundle_bytes = &bytes[absolute_data_start..bundle_end];
        let files = parse_bundle(bundle_bytes)?;
        let object_crc = read_u32(entry, 0x38)?;
        if crc32_u32(bundle_bytes) != object_crc {
            return Err(RomxError::Invalid(format!(
                "mutable SAVE object CRC32 mismatch: {key}"
            )));
        }
        let stored_entry_crc = read_u32(entry, 0x3c)?;
        let mut checked_entry = entry.to_vec();
        checked_entry[0x3c..0x40].fill(0);
        if crc32_u32(&checked_entry) != stored_entry_crc {
            return Err(RomxError::Invalid(format!(
                "mutable SAVE directory entry CRC32 mismatch: {key}"
            )));
        }
        objects.push(MutableSaveObject {
            slot,
            key,
            files,
            data_size,
            data_capacity: data_capacity_entry,
            generation: read_u64(entry, 0x28)?,
            modified_at: read_u64(entry, 0x30)?,
            crc32: format!("{object_crc:08x}"),
        });
    }
    let used_capacity = used_ranges
        .iter()
        .map(|(start, end)| end.saturating_sub(*start) as u64)
        .sum::<u64>();
    Ok(MutableRegionInfo {
        offset: offset as u64,
        capacity,
        entry_capacity,
        data_capacity,
        free_slots: entry_capacity.saturating_sub(objects.len()),
        free_bytes: data_capacity.saturating_sub(used_capacity),
        objects,
    })
}

/// Extract one logical SAVE object to a directory, preserving its internal tree.
pub fn extract_mutable_save_object(
    romx: &Path,
    key: &str,
    output: &Path,
) -> Result<PathBuf, RomxError> {
    let info = read_mutable_save_objects(romx)?;
    let object = info
        .objects
        .into_iter()
        .find(|object| object.key == key)
        .ok_or_else(|| RomxError::Invalid(format!("mutable SAVE object not found: {key}")))?;
    let root = output.join(&object.key);
    for file in object.files {
        let destination = root.join(file.path);
        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }
        crate::write_atomic_stream(&destination, true, |writer| {
            use std::io::Write;
            writer.write_all(&file.bytes)?;
            Ok(())
        })?;
    }
    Ok(root)
}

/// Extract every logical SAVE object below `output`.
///
/// The returned paths are the object roots.  A single-file object is written
/// as `<key>.<ext>` so a save directory can be rescanned without losing
/// object boundaries.  Multi-file objects are written as `<key>/...` and keep
/// their internal relative paths, which is the layout expected by PSP
/// savedata and directory-based platforms.
pub fn extract_mutable_save_objects(romx: &Path, output: &Path) -> Result<Vec<PathBuf>, RomxError> {
    let info = read_mutable_save_objects(romx)?;
    let mut roots = Vec::with_capacity(info.objects.len());
    for object in info.objects {
        let root = output.join(&object.key);
        let single_file = object.files.len() == 1
            && !object.files[0].path.contains('/')
            && !object.files[0].path.eq_ignore_ascii_case(PSP_MARKER);
        let single_destination = if single_file {
            let extension = Path::new(&object.files[0].path)
                .extension()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .map(|value| format!(".{value}"))
                .unwrap_or_default();
            Some(output.join(format!("{}{extension}", object.key)))
        } else {
            None
        };
        for file in object.files {
            let destination = single_destination
                .clone()
                .unwrap_or_else(|| root.join(&file.path));
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            crate::write_atomic_stream(&destination, true, |writer| {
                use std::io::Write;
                writer.write_all(&file.bytes)?;
                Ok(())
            })?;
        }
        roots.push(single_destination.unwrap_or(root));
    }
    Ok(roots)
}

#[cfg(test)]
mod tests {
    use super::{
        detect_save_bundles, extract_mutable_save_object, extract_mutable_save_objects,
        inspect_mutable_path, read_mutable_save_objects, save_profile, SaveGrouping,
    };
    use crate::{
        pack_to_path_with_writer_options, read_mutable_region,
        recommended_mutable_capacity_for_save_count, MutableSaveBundle, MutableSaveFile,
        PackOptions, RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY,
    };
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn psp_savedata_directories_are_separate_complete_objects() {
        let root = tempdir().unwrap();
        for (directory, marker) in [("ULUS-00001", b"one"), ("ULUS-00002", b"two")] {
            let path = root.path().join(directory);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("PARAM.SFO"), marker).unwrap();
            fs::write(path.join("ICON0.PNG"), b"icon").unwrap();
        }
        let bundles = detect_save_bundles(root.path(), "psp", "iso").unwrap();
        assert_eq!(
            save_profile("psp", "iso").grouping,
            SaveGrouping::MarkerDirectory
        );
        assert_eq!(bundles.len(), 2);
        assert_eq!(bundles[0].key, "ULUS-00001");
        assert_eq!(bundles[0].files.len(), 2);
        assert_eq!(bundles[0].files[0].path, "ICON0.PNG");
    }

    #[test]
    fn single_file_platform_does_not_group_a_top_level_directory() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("slot1.sav"), b"one").unwrap();
        fs::write(root.path().join("slot2.sav"), b"two").unwrap();
        let bundles = detect_save_bundles(root.path(), "gba", "gba").unwrap();
        assert_eq!(
            save_profile("gba", "gba").grouping,
            SaveGrouping::SingleFile
        );
        assert_eq!(bundles.len(), 2);
        assert_eq!(
            inspect_mutable_path(root.path(), "gba", "gba")
                .unwrap()
                .count,
            2
        );
    }

    #[test]
    fn directory_per_save_platform_groups_direct_children_only() {
        let root = tempdir().unwrap();
        for directory in ["slot1", "slot2"] {
            let path = root.path().join(directory);
            fs::create_dir_all(path.join("nested")).unwrap();
            fs::write(path.join("nested").join("state.dat"), b"state").unwrap();
        }
        let bundles = detect_save_bundles(root.path(), "3ds", "3ds").unwrap();
        assert_eq!(bundles.len(), 2);
        assert_eq!(bundles[0].files[0].path, "nested/state.dat");
    }

    #[test]
    fn mutable_save_objects_are_read_and_extracted() {
        let root = tempdir().unwrap();
        let save = root.path().join("slot.sav");
        fs::write(&save, b"save-bytes").unwrap();
        let rom = root.path().join("game.gba");
        fs::write(&rom, b"rom-bytes").unwrap();
        let output = root.path().join("game.romx");
        let capacity =
            recommended_mutable_capacity_for_save_count(RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY, 1);
        let options = PackOptions {
            mutable_capacity: capacity,
            mutable_entry_capacity: 8,
            mutable_save_bundles: vec![MutableSaveBundle {
                key: "slot".into(),
                files: vec![MutableSaveFile {
                    path: "slot.sav".into(),
                    source: save,
                }],
            }],
            ..Default::default()
        };
        pack_to_path_with_writer_options(&rom, None, None, &output, &options).unwrap();

        let region = read_mutable_save_objects(&output).unwrap();
        assert_eq!(region.objects.len(), 1);
        assert_eq!(region.objects[0].files[0].path, "slot.sav");
        assert_eq!(region.objects[0].files[0].bytes, b"save-bytes");
        assert_eq!(region.free_slots, 7);

        let extracted = root.path().join("extracted");
        let extracted_root = extract_mutable_save_object(&output, "slot", &extracted).unwrap();
        assert_eq!(
            fs::read(extracted_root.join("slot.sav")).unwrap(),
            b"save-bytes"
        );

        let preserved_region = read_mutable_region(&output).unwrap().unwrap();
        let preserved_output = root.path().join("preserved.romx");
        let preserved_options = PackOptions {
            mutable_capacity: capacity,
            mutable_region: Some(preserved_region.clone()),
            ..Default::default()
        };
        pack_to_path_with_writer_options(&rom, None, None, &preserved_output, &preserved_options)
            .unwrap();
        assert_eq!(
            read_mutable_region(&preserved_output).unwrap().unwrap(),
            preserved_region
        );
    }

    #[test]
    fn extracted_save_objects_can_be_scanned_again_without_merging_slots() {
        let root = tempdir().unwrap();
        let source = root.path().join("psp-saves");
        for (directory, marker) in [("ULUS-00001", b"one"), ("ULUS-00002", b"two")] {
            let path = source.join(directory);
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("PARAM.SFO"), marker).unwrap();
            fs::write(path.join("ICON0.PNG"), b"icon").unwrap();
        }
        let rom = root.path().join("game.iso");
        fs::write(&rom, b"rom-bytes").unwrap();
        let bundles = detect_save_bundles(&source, "psp", "iso").unwrap();
        let options = PackOptions {
            mutable_capacity: RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY,
            mutable_entry_capacity: 8,
            mutable_save_bundles: bundles,
            ..Default::default()
        };
        let romx = root.path().join("game.romx");
        pack_to_path_with_writer_options(&rom, None, None, &romx, &options).unwrap();

        let extracted = root.path().join("roundtrip");
        let roots = extract_mutable_save_objects(&romx, &extracted).unwrap();
        assert_eq!(roots.len(), 2);
        let rescanned = detect_save_bundles(&extracted, "psp", "iso").unwrap();
        assert_eq!(rescanned.len(), 2);
        assert_eq!(rescanned[0].files.len(), 2);
        assert_eq!(rescanned[0].files[0].path, "ICON0.PNG");
        assert_eq!(rescanned[0].files[1].path, "PARAM.SFO");
    }
}
