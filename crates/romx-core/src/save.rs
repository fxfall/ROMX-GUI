//! Safe adapters for libromx's host-side SAVE catalog and mutable bundles.
//!
//! `libromx` owns save classification. This module deliberately does not
//! inspect directory depth, extensions, PSP markers, or 3DS IDs itself. It
//! translates C catalog values into ergonomic Rust data and keeps host
//! extraction policy outside of the ROMX wire-format implementation.

use crate::error::{c_field, c_path, c_string, check, copy_c_buffer};
use crate::{MutableSaveBundle, MutableSaveFile, Reader, RomxError};
use libromx_sys as sys;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::ptr::NonNull;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveGrouping {
    SingleFile,
    DirectoryPerSave,
    MarkerDirectory,
    Unspecified,
}

impl SaveGrouping {
    fn from_raw(value: sys::romx_save_grouping_t) -> Self {
        match value {
            sys::ROMX_SAVE_GROUP_SINGLE_FILE => Self::SingleFile,
            sys::ROMX_SAVE_GROUP_DIRECTORY_PER_SAVE => Self::DirectoryPerSave,
            sys::ROMX_SAVE_GROUP_MARKER_DIRECTORY => Self::MarkerDirectory,
            _ => Self::Unspecified,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveScope {
    Unspecified,
    ThreeDsTitle,
    ThreeDsExtData,
}

impl SaveScope {
    fn from_raw(value: sys::romx_save_scope_t) -> Self {
        match value {
            sys::ROMX_SAVE_SCOPE_3DS_TITLE => Self::ThreeDsTitle,
            sys::ROMX_SAVE_SCOPE_3DS_EXTDATA => Self::ThreeDsExtData,
            _ => Self::Unspecified,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SaveSourceFormat {
    Auto,
    File,
    Directory,
    PspSavedata,
    ThreeDsGateway,
    ThreeDsSavedatafiler,
    ThreeDsCitra,
    ThreeDsBackup,
    RomxBundle,
    Unknown(u16),
}

impl SaveSourceFormat {
    fn from_raw(value: sys::romx_save_source_format_t) -> Self {
        match value {
            sys::ROMX_SAVE_SOURCE_AUTO => Self::Auto,
            sys::ROMX_SAVE_SOURCE_FILE => Self::File,
            sys::ROMX_SAVE_SOURCE_DIRECTORY => Self::Directory,
            sys::ROMX_SAVE_SOURCE_PSP_SAVEDATA => Self::PspSavedata,
            sys::ROMX_SAVE_SOURCE_3DS_GATEWAY => Self::ThreeDsGateway,
            sys::ROMX_SAVE_SOURCE_3DS_SAVEDATAFILER => Self::ThreeDsSavedatafiler,
            sys::ROMX_SAVE_SOURCE_3DS_CITRA => Self::ThreeDsCitra,
            sys::ROMX_SAVE_SOURCE_3DS_BACKUP => Self::ThreeDsBackup,
            sys::ROMX_SAVE_SOURCE_ROMX_BUNDLE => Self::RomxBundle,
            other => Self::Unknown(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveProfile {
    pub platform: String,
    pub payload_format: String,
    pub grouping: SaveGrouping,
    pub marker: Option<String>,
    pub platform_id: u16,
    pub format_id: u16,
    pub launch_format_id: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveCandidateFile {
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveCandidate {
    pub index: u32,
    pub key: String,
    pub display_name: String,
    pub title_id: Option<String>,
    pub extdata_id: Option<String>,
    pub source_format: SaveSourceFormat,
    pub grouping: SaveGrouping,
    pub scope: SaveScope,
    pub is_directory: bool,
    pub files: Vec<SaveCandidateFile>,
    pub data_size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveLayout {
    pub scope: SaveScope,
    pub strict_extdata: bool,
    pub extdata_id: Option<String>,
    pub entry_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaveSlot {
    pub index: u32,
    pub key: String,
    pub display_name: String,
    pub data_size: u64,
    pub is_directory: bool,
    pub entries: Vec<SaveCandidateFile>,
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
    pub namespace: sys::romx_mutable_namespace_t,
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

fn init<T>() -> T {
    unsafe { std::mem::zeroed() }
}

fn set_size<T>(value: &mut T) {
    unsafe {
        (value as *mut T as *mut u32).write(
            u32::try_from(std::mem::size_of::<T>())
                .expect("libromx ABI structure size must fit in the C u32 field"),
        )
    };
}

fn platform_id(name: &str) -> u16 {
    crate::platform_id_for_name(&name.trim().to_ascii_lowercase())
}

fn format_id(name: &str) -> u16 {
    crate::format_id_for_extension(name.trim().trim_start_matches('.'))
}

fn profile_from_c(
    platform: &str,
    payload_format: &str,
    profile: sys::romx_save_profile_info_t,
) -> SaveProfile {
    let marker = (profile.marker_size != 0).then(|| c_field(&profile.marker));
    SaveProfile {
        platform: platform.trim().to_ascii_lowercase(),
        payload_format: payload_format
            .trim()
            .trim_start_matches('.')
            .to_ascii_lowercase(),
        grouping: SaveGrouping::from_raw(profile.grouping),
        marker,
        platform_id: profile.platform_id,
        format_id: profile.format_id,
        launch_format_id: profile.launch_format_id,
    }
}

/// Return the canonical grouping policy from libromx.
pub fn save_profile(platform: &str, payload_format: &str) -> SaveProfile {
    let platform_id = platform_id(platform);
    let format_id = format_id(payload_format);
    let launch_format_id = crate::launch_format_id_for_extension(payload_format);
    let mut profile: sys::romx_save_profile_info_t = init();
    set_size(&mut profile);
    let mut error: sys::romx_error_t = init();
    let code = unsafe {
        sys::romx_save_profile_get(
            platform_id,
            format_id,
            launch_format_id,
            &mut profile,
            &mut error,
        )
    };
    if code == sys::ROMX_OK {
        profile_from_c(platform, payload_format, profile)
    } else {
        // Unknown platform/format pairs remain explicitly unspecified. The C
        // catalog is still the only authority for all recognized profiles;
        // this value merely keeps the descriptive API total for callers that
        // use it to populate a UI before opening a catalog.
        SaveProfile {
            platform: platform.trim().to_ascii_lowercase(),
            payload_format: payload_format
                .trim()
                .trim_start_matches('.')
                .to_ascii_lowercase(),
            grouping: SaveGrouping::Unspecified,
            marker: None,
            platform_id,
            format_id,
            launch_format_id,
        }
    }
}

/// Owning RAII wrapper around a libromx SAVE catalog.
pub struct SaveCatalog {
    raw: NonNull<sys::romx_save_catalog_t>,
    source: PathBuf,
    profile: SaveProfile,
}

impl SaveCatalog {
    pub fn open(path: &Path, profile: &SaveProfile) -> Result<Self, RomxError> {
        Self::open_with_flags(path, profile, 0)
    }

    /// Open a SAVE catalog with explicit scan flags from libromx.
    ///
    /// In particular, `ROMX_SAVE_SCAN_TREAT_ROOT_AS_SAVE` is intentionally
    /// opt-in: callers should set it only when the user selected a directory
    /// that is itself one save object. A collection directory must use the
    /// default flags so libromx can discover all candidates.
    pub fn open_with_flags(
        path: &Path,
        profile: &SaveProfile,
        scan_flags: u32,
    ) -> Result<Self, RomxError> {
        if scan_flags & !sys::ROMX_SAVE_SCAN_FLAGS_MASK != 0 {
            return Err(RomxError::Invalid(format!(
                "unsupported SAVE scan flags: 0x{scan_flags:08x}"
            )));
        }
        let source = c_path(path)?;
        let mut options: sys::romx_save_scan_options_t = init();
        set_size(&mut options);
        options.platform_id = profile.platform_id;
        options.format_id = profile.format_id;
        options.launch_format_id = profile.launch_format_id;
        options.flags = scan_flags;
        let mut raw = std::ptr::null_mut();
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_save_catalog_open_path(source.as_ptr(), &options, &mut raw, &mut error)
        };
        check(code, &error)?;
        Ok(Self {
            raw: NonNull::new(raw)
                .ok_or_else(|| RomxError::Invalid("libromx returned a null SAVE catalog".into()))?,
            source: path.to_owned(),
            profile: profile.clone(),
        })
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn profile(&self) -> &SaveProfile {
        &self.profile
    }

    fn as_ptr(&self) -> *const sys::romx_save_catalog_t {
        self.raw.as_ptr()
    }

    pub fn candidate_count(&self) -> Result<u32, RomxError> {
        let mut count = 0u32;
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_save_catalog_get_candidate_count(self.as_ptr(), &mut count, &mut error)
        };
        check(code, &error)?;
        Ok(count)
    }

    pub fn candidate(&self, index: u32) -> Result<SaveCandidate, RomxError> {
        let mut candidate: sys::romx_save_candidate_info_t = init();
        set_size(&mut candidate);
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_save_catalog_get_candidate(self.as_ptr(), index, &mut candidate, &mut error)
        };
        check(code, &error)?;
        let files = self.candidate_files(index)?;
        Ok(SaveCandidate {
            index: candidate.index,
            key: c_field(&candidate.key),
            display_name: c_field(&candidate.display_name),
            title_id: (candidate.title_id[0] != 0).then(|| c_field(&candidate.title_id)),
            extdata_id: (candidate.extdata_id[0] != 0).then(|| c_field(&candidate.extdata_id)),
            source_format: SaveSourceFormat::from_raw(candidate.source_format),
            grouping: SaveGrouping::from_raw(candidate.grouping),
            scope: SaveScope::from_raw(candidate.scope),
            is_directory: candidate.flags & sys::ROMX_SAVE_CANDIDATE_IS_DIRECTORY != 0,
            files,
            data_size: candidate.data_size,
        })
    }

    pub fn candidates(&self) -> Result<Vec<SaveCandidate>, RomxError> {
        let count = self.candidate_count()?;
        (0..count).map(|index| self.candidate(index)).collect()
    }

    pub fn candidate_files(
        &self,
        candidate_index: u32,
    ) -> Result<Vec<SaveCandidateFile>, RomxError> {
        let mut count = 0u32;
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_save_catalog_get_file_count(
                self.as_ptr(),
                candidate_index,
                &mut count,
                &mut error,
            )
        };
        check(code, &error)?;
        let capacity = usize::try_from(count)
            .map_err(|_| RomxError::Invalid("SAVE file count exceeds usize".into()))?;
        let mut files = Vec::with_capacity(capacity);
        for file_index in 0..count {
            let mut file: sys::romx_save_file_info_t = init();
            set_size(&mut file);
            error = init();
            let code = unsafe {
                sys::romx_save_catalog_get_file(
                    self.as_ptr(),
                    candidate_index,
                    file_index,
                    &mut file,
                    &mut error,
                )
            };
            check(code, &error)?;
            files.push(SaveCandidateFile {
                path: c_field(&file.path),
                size: file.data_size,
            });
        }
        Ok(files)
    }

    pub fn candidate_source_path(&self, candidate_index: u32) -> Result<PathBuf, RomxError> {
        let bytes = copy_c_buffer(|buffer, capacity, required, error| unsafe {
            sys::romx_save_catalog_copy_candidate_source_path(
                self.as_ptr(),
                candidate_index,
                buffer,
                capacity,
                required,
                error,
            )
        })?;
        let bytes = bytes.strip_suffix(&[0]).unwrap_or(&bytes);
        let value = String::from_utf8(bytes.to_vec())
            .map_err(|_| RomxError::Invalid("SAVE source path is not UTF-8".into()))?;
        Ok(PathBuf::from(value))
    }

    /// Ask libromx for the exact serialized candidate size. The catalog owns
    /// all platform-specific path normalization (including PSP and 3DS
    /// ExtData), so the Rust layer never duplicates the RMBL size formula.
    pub fn measure_candidate(&self, candidate_index: u32) -> Result<u64, RomxError> {
        let mut bundle_options: sys::romx_mutable_bundle_options_t = init();
        set_size(&mut bundle_options);
        let mut serialized_size = 0u64;
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_save_catalog_measure_candidate(
                self.as_ptr(),
                candidate_index,
                &bundle_options,
                &mut serialized_size,
                &mut error,
            )
        };
        check(code, &error)?;
        Ok(serialized_size)
    }

    pub fn write_candidate(
        &self,
        candidate_index: u32,
        romx: &Path,
        object_key: Option<&str>,
        bundle_options: Option<sys::romx_mutable_bundle_options_t>,
        write_options: Option<sys::romx_mutable_write_options_t>,
    ) -> Result<sys::romx_mutable_object_info_t, RomxError> {
        let romx = c_path(romx)?;
        let key = object_key
            .map(|value| c_string(value, "mutable save key"))
            .transpose()?;
        let mut bundle_options = bundle_options.unwrap_or_else(init);
        set_size(&mut bundle_options);
        let mut write_options = write_options.unwrap_or_else(init);
        set_size(&mut write_options);
        let mut written: sys::romx_mutable_object_info_t = init();
        set_size(&mut written);
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_save_catalog_write_candidate(
                self.as_ptr(),
                candidate_index,
                romx.as_ptr(),
                key.as_ref()
                    .map_or(std::ptr::null(), |value| value.as_ptr()),
                &bundle_options,
                &write_options,
                &mut written,
                &mut error,
            )
        };
        check(code, &error)?;
        Ok(written)
    }
}

impl Drop for SaveCatalog {
    fn drop(&mut self) {
        unsafe { sys::romx_save_catalog_close(self.raw.as_ptr()) };
    }
}

/// Discover candidates exactly as libromx classifies them.
pub fn detect_save_bundles(
    path: &Path,
    platform: &str,
    payload_format: &str,
) -> Result<Vec<MutableSaveBundle>, RomxError> {
    detect_save_bundles_with_flags(path, platform, payload_format, 0)
}

/// Discover save bundles with an explicit libromx scan policy.
pub fn detect_save_bundles_with_flags(
    path: &Path,
    platform: &str,
    payload_format: &str,
    scan_flags: u32,
) -> Result<Vec<MutableSaveBundle>, RomxError> {
    let profile = save_profile(platform, payload_format);
    let catalog = SaveCatalog::open_with_flags(path, &profile, scan_flags)?;
    let mut output = Vec::new();
    for candidate in catalog.candidates()? {
        let root = catalog.candidate_source_path(candidate.index)?;
        let files = candidate
            .files
            .iter()
            .map(|file| {
                let source = if candidate.is_directory {
                    root.join(Path::new(&file.path))
                } else {
                    root.clone()
                };
                MutableSaveFile {
                    path: file.path.clone(),
                    source,
                }
            })
            .collect();
        output.push(MutableSaveBundle {
            key: candidate.key,
            files,
        });
    }
    Ok(output)
}

pub fn inspect_mutable_path(
    path: &Path,
    platform: &str,
    payload_format: &str,
) -> Result<SaveInventory, RomxError> {
    inspect_mutable_path_with_flags(path, platform, payload_format, 0)
}

/// Inspect save candidates with an explicit libromx scan policy.
pub fn inspect_mutable_path_with_flags(
    path: &Path,
    platform: &str,
    payload_format: &str,
    scan_flags: u32,
) -> Result<SaveInventory, RomxError> {
    let profile = save_profile(platform, payload_format);
    let catalog = SaveCatalog::open_with_flags(path, &profile, scan_flags)?;
    let candidates = catalog.candidates()?;
    Ok(SaveInventory {
        count: candidates.len(),
        bytes: candidates.iter().map(|candidate| candidate.data_size).sum(),
        files: candidates
            .iter()
            .map(|candidate| candidate.files.len())
            .sum(),
    })
}

/// Legacy UI helper retained for file-dialog filtering. Classification and
/// grouping never call it; the catalog always returns every file it owns.
pub fn is_supported_save_file(path: &Path) -> bool {
    const EXTENSIONS: &[&str] = &[
        "sav", "save", "srm", "dsv", "eep", "eeprom", "ram", "sra", "fla", "flash", "rtc", "mcr",
        "gci", "dat",
    ];
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            EXTENSIONS
                .iter()
                .any(|candidate| extension.eq_ignore_ascii_case(candidate))
        })
}

fn portable_relative_path(path: &str) -> Result<(), RomxError> {
    let value = Path::new(path);
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.as_bytes().contains(&0)
        || value
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(RomxError::Invalid(format!(
            "invalid mutable SAVE path: {path}"
        )));
    }
    Ok(())
}

fn safe_destination(root: &Path, relative: &str) -> Result<PathBuf, RomxError> {
    portable_relative_path(relative)?;
    Ok(root.join(relative))
}

/// Create a destination directory without traversing a pre-existing symlink.
/// `create_dir_all` follows symlinks, which would allow a malicious SAVE path
/// to redirect extraction outside the selected output directory. The selected
/// `root` itself is checked once, then only relative components below it are
/// walked; symlinks in system ancestors such as `/var` are not part of the
/// extraction boundary.
fn ensure_directory_without_symlinks(root: &Path, relative: &Path) -> Result<(), RomxError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(RomxError::Invalid(format!(
                    "SAVE extraction root is a symlink or non-directory: {}",
                    root.display()
                )));
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(root)?;
            let metadata = fs::symlink_metadata(root)?;
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                return Err(RomxError::Invalid(format!(
                    "SAVE extraction root is not a directory: {}",
                    root.display()
                )));
            }
        }
        Err(error) => return Err(error.into()),
    }

    let mut current = root.to_owned();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            continue;
        };
        current.push(name);
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(RomxError::Invalid(format!(
                        "SAVE extraction path contains a symlink or non-directory: {}",
                        current.display()
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current)?;
                let metadata = fs::symlink_metadata(&current)?;
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(RomxError::Invalid(format!(
                        "SAVE extraction path is not a directory: {}",
                        current.display()
                    )));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn object_from_reader(
    reader: &Reader,
    slot: u32,
    object: sys::romx_mutable_object_info_t,
) -> Result<MutableSaveObject, RomxError> {
    let key = c_field(&object.key);
    let mut files = Vec::new();
    if object.object_namespace == sys::ROMX_MUTABLE_NAMESPACE_SAVE {
        let mut bundle = reader.mutable_bundle(&key, None)?;
        for index in 0..bundle.entry_count()? {
            let entry = bundle.entry(index)?;
            let path = c_field(&entry.path);
            let mut bytes = Vec::with_capacity(
                usize::try_from(entry.data_size)
                    .map_err(|_| RomxError::Invalid("SAVE entry is too large".into()))?,
            );
            bundle.read_entry_to(index, entry.data_size, &mut bytes)?;
            files.push(MutableSaveFileData {
                path,
                bytes,
                crc32: format!("{:08x}", entry.data_crc32),
            });
        }
    }
    Ok(MutableSaveObject {
        slot: usize::try_from(slot)
            .map_err(|_| RomxError::Invalid("mutable slot exceeds usize".into()))?,
        key,
        files,
        data_size: object.data_size,
        data_capacity: object.data_capacity,
        generation: object.generation,
        modified_at: object.modified_unix_seconds,
        crc32: format!("{:08x}", object.data_crc32),
        namespace: object.object_namespace,
    })
}

/// Read mutable objects through Reader/MutableBundle. Unknown namespaces are
/// included in accounting but their files are not interpreted; this prevents
/// frontend edits from deleting CHEAT/STATS data.
pub fn read_mutable_save_objects(path: &Path) -> Result<MutableRegionInfo, RomxError> {
    let reader = Reader::open(path)?;
    let info = reader.info()?;
    let object_count = reader.mutable_object_count()?;
    let mut objects = Vec::new();
    let mut used_bytes = 0u64;
    let mut highest_slot = 0usize;
    for index in 0..object_count {
        let object = reader.mutable_object(index)?;
        highest_slot = highest_slot.max(usize::try_from(object.slot_index).unwrap_or(usize::MAX));
        used_bytes = used_bytes.saturating_add(object.data_capacity);
        if object.object_namespace == sys::ROMX_MUTABLE_NAMESPACE_SAVE {
            objects.push(object_from_reader(&reader, object.slot_index, object)?);
        }
    }
    let entry_capacity = if object_count == 0 {
        0
    } else {
        highest_slot
            .saturating_add(1)
            .div_ceil(8)
            .saturating_mul(8)
            .max(8)
    };
    let free_slots =
        entry_capacity.saturating_sub(usize::try_from(object_count).unwrap_or(entry_capacity));
    Ok(MutableRegionInfo {
        offset: info.mutable_region.offset,
        capacity: info.mutable_region.size,
        entry_capacity,
        data_capacity: info.mutable_region.size,
        free_slots,
        free_bytes: info.mutable_region.size.saturating_sub(used_bytes),
        objects,
    })
}

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
        let destination = safe_destination(&root, &file.path)?;
        if let Some(parent) = destination.parent() {
            let relative = parent
                .strip_prefix(output)
                .map_err(|_| RomxError::Invalid("SAVE destination escaped output root".into()))?;
            ensure_directory_without_symlinks(output, relative)?;
        }
        crate::write_atomic_stream(&destination, true, |writer| {
            writer.write_all(&file.bytes)?;
            Ok(())
        })?;
    }
    Ok(root)
}

/// Stream one SAVE object at a time to the host filesystem. Mutable bytes are
/// supplied by libromx's validated bundle cursor; no RMBL offsets or checksums
/// are interpreted in Rust.
pub(crate) fn extract_mutable_save_objects_from_path<C, O>(
    romx: &Path,
    output: &Path,
    temporary_output: bool,
    mut is_cancelled: C,
    mut on_output: O,
) -> Result<Vec<PathBuf>, RomxError>
where
    C: FnMut() -> bool,
    O: FnMut(&Path) -> Result<Option<PathBuf>, RomxError>,
{
    let reader = Reader::open(romx)?;
    let mut roots = Vec::new();
    let object_count = reader.mutable_object_count()?;
    for object_index in 0..object_count {
        if is_cancelled() {
            return Err(RomxError::Cancelled);
        }
        let object = reader.mutable_object(object_index)?;
        if object.object_namespace != sys::ROMX_MUTABLE_NAMESPACE_SAVE {
            continue;
        }
        let key = c_field(&object.key);
        // libromx validates object keys when they are written. Keep the host
        // extraction boundary explicit before joining the selected output.
        portable_relative_path(&key)?;
        let mut bundle = reader.mutable_bundle(&key, None)?;
        let entry_count = bundle.entry_count()?;
        let slot_count = bundle.save_slot_count()?;
        let single_file =
            slot_count == 1 && entry_count == 1 && bundle.save_slot(0)?.is_directory == 0;
        let first_path = if single_file {
            Some(bundle.entry_path(0)?)
        } else {
            None
        };
        let root = output.join(&key);
        let single_destination = first_path.as_deref().map(|path| {
            let extension = Path::new(path)
                .extension()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .map(|value| format!(".{value}"))
                .unwrap_or_default();
            output.join(format!("{key}{extension}"))
        });
        for entry_index in 0..entry_count {
            let entry = bundle.entry(entry_index)?;
            let entry_path = c_field(&entry.path);
            let destination = if let Some(single_destination) = single_destination.as_ref() {
                single_destination.clone()
            } else {
                safe_destination(&root, &entry_path)?
            };
            let staged = if temporary_output {
                let filename = destination
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("save");
                destination.with_file_name(format!("{filename}.tmp"))
            } else {
                destination
            };
            if let Some(parent) = staged.parent() {
                let relative = parent.strip_prefix(output).map_err(|_| {
                    RomxError::Invalid("SAVE destination escaped output root".into())
                })?;
                ensure_directory_without_symlinks(output, relative)?;
            }
            crate::write_atomic_stream(&staged, true, |writer| {
                let copied = bundle.read_entry_to(entry_index, entry.data_size, writer)?;
                if copied != entry.data_size {
                    return Err(RomxError::Invalid(
                        "mutable SAVE entry size changed while extracting".into(),
                    ));
                }
                Ok(())
            })?;
            let _ = on_output(&staged)?;
        }
        roots.push(single_destination.unwrap_or(root));
    }
    Ok(roots)
}

pub fn extract_mutable_save_objects(romx: &Path, output: &Path) -> Result<Vec<PathBuf>, RomxError> {
    extract_mutable_save_objects_from_path(
        romx,
        output,
        false,
        || false,
        |path| Ok(Some(path.to_owned())),
    )
}

/// Inspect the platform-aware logical SAVE projection for one bundle.
pub fn save_layout(reader: &Reader, key: &str) -> Result<SaveLayout, RomxError> {
    let bundle = reader.mutable_bundle(key, None)?;
    let mut layout: sys::romx_mutable_save_layout_info_t = init();
    set_size(&mut layout);
    let mut error: sys::romx_error_t = init();
    let code = unsafe {
        sys::romx_mutable_bundle_get_save_layout(bundle.as_ptr(), &mut layout, &mut error)
    };
    check(code, &error)?;
    Ok(SaveLayout {
        scope: SaveScope::from_raw(layout.scope),
        strict_extdata: layout.flags & sys::ROMX_MUTABLE_SAVE_LAYOUT_STRICT_EXTDATA != 0,
        extdata_id: (layout.extdata_id[0] != 0).then(|| c_field(&layout.extdata_id)),
        entry_count: layout.entry_count,
    })
}
