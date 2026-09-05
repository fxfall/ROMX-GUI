//! ROMX writer orchestration.
//!
//! The wire format is deliberately absent from this module.  All container
//! layout, hashing, path validation, and atomic publication are delegated to
//! libromx; Rust only prepares validated UTF-8 arguments and source paths.

use crate::error::{c_path, c_string, check, checked_u32};
use crate::{metadata_bytes, PackEntry, PackOptions, RomxError};
use libromx_sys as sys;
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

/// Marker type for the stateless libromx writer API.
pub struct Writer;

struct TempInput {
    path: PathBuf,
}

/// A destination reserved for the complete write transaction.  libromx
/// atomically publishes the immutable container to this path; SAVE objects
/// are then added and validated before the path is moved into place.  If any
/// post-write step fails, Drop removes only this staging file and leaves an
/// existing destination untouched.
struct StagedOutput {
    path: PathBuf,
    committed: bool,
}

impl StagedOutput {
    fn new(destination: &Path) -> Result<Self, RomxError> {
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let stem = destination
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("romx");
        let mut attempts = 0u32;
        loop {
            let serial = super::TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!(".{stem}.stage-{serial}"));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => {
                    fs::remove_file(&path)?;
                    return Ok(Self {
                        path,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                    attempts += 1;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn publish(mut self, destination: &Path, replace: bool) -> Result<(), RomxError> {
        if destination.exists() && !replace {
            return Err(RomxError::Exists(destination.to_owned()));
        }
        #[cfg(windows)]
        if destination.exists() && replace {
            // Windows does not let std::fs::rename replace an existing file.
            // The staged file is complete and validated at this point; remove
            // only the requested destination before the final move.
            fs::remove_file(destination)?;
        }
        fs::rename(&self.path, destination)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for StagedOutput {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

impl TempInput {
    fn create(suffix: &str, bytes: &[u8]) -> Result<Self, RomxError> {
        let mut attempts = 0u32;
        loop {
            let serial = super::TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "romx-core-{}-{serial}.{suffix}",
                std::process::id()
            ));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    file.write_all(bytes)?;
                    return Ok(Self { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                    attempts += 1;
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
}

impl Drop for TempInput {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl Writer {
    /// Stream one or more source files into an atomically published ROMX.
    pub fn write_path_entries(
        entries: &[PackEntry],
        metadata: Option<&Value>,
        cover: Option<&[u8]>,
        output: &Path,
        options: &PackOptions,
    ) -> Result<sys::romx_writer_report_t, RomxError> {
        if options.mutable_region.is_some() {
            return Err(RomxError::Invalid(
                "in-memory mutable_region is no longer supported; use mutable_region_source".into(),
            ));
        }
        if entries.is_empty() {
            return Err(RomxError::Invalid(
                "a ROMX writer needs at least one entry".into(),
            ));
        }

        let mut normalized = entries.to_vec();
        for entry in &mut normalized {
            if entry.format_id == 0 {
                entry.format_id = super::format_id_for_extension(
                    Path::new(&entry.path)
                        .extension()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default(),
                );
            }
            if entry.format_id == 0 {
                return Err(RomxError::Invalid(format!(
                    "entry format is not registered: {}",
                    entry.path
                )));
            }
            let metadata = fs::symlink_metadata(&entry.source)?;
            if !metadata.file_type().is_file() {
                return Err(RomxError::Invalid(format!(
                    "ROMX entry source is not a regular file: {}",
                    entry.source.display()
                )));
            }
        }
        normalized.sort_by(|left, right| {
            right
                .entrypoint
                .cmp(&left.entrypoint)
                .then_with(|| left.path.as_bytes().cmp(right.path.as_bytes()))
        });
        if normalized.iter().filter(|entry| entry.entrypoint).count() != 1 {
            return Err(RomxError::Invalid(
                "ROMX payload must contain exactly one entrypoint".into(),
            ));
        }
        for pair in normalized.windows(2) {
            if pair[0].path.eq_ignore_ascii_case(&pair[1].path) {
                return Err(RomxError::Invalid(
                    "ROMX paths collide after case folding".into(),
                ));
            }
        }

        let entrypoint = normalized
            .iter()
            .find(|entry| entry.entrypoint)
            .ok_or_else(|| RomxError::Invalid("ROMX entrypoint is missing".into()))?;
        let compute_metadata_crc = metadata.is_some() && options.crc32_override.is_none();
        // libromx computes the entrypoint CRC while it streams the payload.
        // Use a fixed-width placeholder here so metadata no longer requires a
        // separate pre-write pass over a potentially multi-gigabyte ROM.
        let payload_crc = if compute_metadata_crc { "00000000" } else { "" };
        let normalized_cover = cover
            .map(|bytes| super::normalize_cover_bytes(bytes, options.cover_target))
            .transpose()?;
        let metadata_bytes = metadata_bytes(
            metadata,
            normalized_cover.as_deref(),
            payload_crc,
            options.crc32_override.as_deref(),
        )?;

        let metadata_temp = metadata_bytes
            .as_deref()
            .map(|bytes| TempInput::create("json", bytes))
            .transpose()?;
        let cover_temp = normalized_cover
            .as_deref()
            .map(|bytes| TempInput::create("png", bytes))
            .transpose()?;

        let mut path_entries = Vec::with_capacity(normalized.len());
        let mut virtual_paths = Vec::with_capacity(normalized.len());
        let mut source_paths = Vec::with_capacity(normalized.len());
        for entry in &normalized {
            let virtual_path = c_string(&entry.path, "entry path")?;
            let source_path = c_path(&entry.source)?;
            let mut flags = 0u32;
            if entry.entrypoint {
                flags |= sys::ROMX_RIDX_ENTRYPOINT;
            }
            if options.include_entry_crc32 {
                flags |= sys::ROMX_RIDX_HAS_CRC32;
            }
            virtual_paths.push(virtual_path);
            source_paths.push(source_path);
            let mut path_entry: sys::romx_writer_path_entry_t = unsafe { std::mem::zeroed() };
            path_entry.struct_size =
                u32::try_from(std::mem::size_of::<sys::romx_writer_path_entry_t>())
                    .expect("libromx ABI structure size must fit in the C u32 field");
            path_entry.flags = flags;
            path_entry.virtual_path = virtual_paths.last().expect("path just pushed").as_ptr();
            path_entry.source_path = source_paths.last().expect("path just pushed").as_ptr();
            path_entry.format_id = entry.format_id;
            path_entries.push(path_entry);
        }

        if output.exists() && !options.replace_existing {
            return Err(RomxError::Exists(output.to_owned()));
        }
        let staged_output = if options.output_is_temporary {
            None
        } else {
            (options.mutable_region_source.is_some() || !options.mutable_save_bundles.is_empty())
                .then(|| StagedOutput::new(output))
                .transpose()?
        };
        let destination_path = staged_output.as_ref().map_or(output, StagedOutput::path);
        let destination = c_path(destination_path)?;
        let metadata_path = metadata_temp
            .as_ref()
            .map(|value| c_path(&value.path))
            .transpose()?;
        let cover_path = cover_temp
            .as_ref()
            .map(|value| c_path(&value.path))
            .transpose()?;
        let mut writer_options: sys::romx_writer_options_t = unsafe { std::mem::zeroed() };
        writer_options.struct_size =
            u32::try_from(std::mem::size_of::<sys::romx_writer_options_t>())
                .expect("libromx ABI structure size must fit in the C u32 field");
        writer_options.flags = (if options.body_sha256 {
            sys::ROMX_WRITER_IMMUTABLE_SHA256
        } else {
            0
        }) | (if options.replace_existing {
            sys::ROMX_WRITER_REPLACE_EXISTING
        } else {
            0
        }) | (if compute_metadata_crc {
            sys::ROMX_WRITER_COMPUTE_METADATA_CRC32
        } else {
            0
        }) | (if options.output_is_temporary {
            sys::ROMX_WRITER_DIRECT_OUTPUT
        } else {
            0
        });
        writer_options.platform_id = if options.platform_id != 0 {
            options.platform_id
        } else {
            infer_platform_id(&entrypoint.path)
        };
        writer_options.launch_format_id = if options.launch_format_id == 1 {
            super::launch_format_id_for_extension(
                Path::new(&entrypoint.path)
                    .extension()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default(),
            )
        } else {
            options.launch_format_id
        };
        writer_options.mutable_capacity = options.mutable_capacity;
        writer_options.mutable_entry_capacity = if options.mutable_entry_capacity == 0 {
            super::DEFAULT_MUTABLE_ENTRY_CAPACITY
        } else {
            options.mutable_entry_capacity
        };
        writer_options.max_metadata_size = super::DEFAULT_MAX_METADATA_SIZE;
        writer_options.max_cover_size = super::DEFAULT_MAX_COVER_SIZE;
        writer_options.max_cover_dimension = super::DEFAULT_MAX_COVER_DIMENSION;
        writer_options.io_chunk_size = 1024 * 1024;
        let mut report: sys::romx_writer_report_t = unsafe { std::mem::zeroed() };
        report.struct_size = u32::try_from(std::mem::size_of::<sys::romx_writer_report_t>())
            .expect("libromx ABI structure size must fit in the C u32 field");
        let mut error: sys::romx_error_t = unsafe { std::mem::zeroed() };
        let code = unsafe {
            sys::romx_writer_write_path_entries(
                destination.as_ptr(),
                path_entries.as_ptr(),
                checked_u32(path_entries.len(), "entry count")?,
                metadata_path
                    .as_ref()
                    .map_or(std::ptr::null(), |path| path.as_ptr()),
                cover_path
                    .as_ref()
                    .map_or(std::ptr::null(), |path| path.as_ptr()),
                &writer_options,
                &mut report,
                &mut error,
            )
        };
        check(code, &error)?;

        // A source mutable region is copied before any new SAVE objects are
        // written. libromx owns the copy and keeps unknown namespaces opaque.
        let mutable_target = staged_output.as_ref().map_or(output, StagedOutput::path);
        if let Some(source) = options.mutable_region_source.as_deref() {
            crate::copy_mutable_region(source, mutable_target)?;
        }

        // SAVE bundles are written through libromx after the immutable file is
        // created. This keeps the streaming writer independent from the host
        // save scanner and preserves the C implementation's unknown namespace
        // semantics.
        if !options.mutable_save_bundles.is_empty() {
            if options.mutable_capacity == 0 {
                return Err(RomxError::Invalid(
                    "mutable save bundles require a reserved mutable region".into(),
                ));
            }
            write_save_bundles(mutable_target, &options.mutable_save_bundles, options)?;
        }
        if options.post_write_validation {
            if let Some(staged_output) = staged_output.as_ref() {
                // Validate the final mutable directory and all immutable hashes
                // before exposing the staged file. This is intentionally done
                // through Reader/Validation rather than duplicating container
                // checks in the writer.
                let reader = crate::Reader::open(staged_output.path())?;
                reader.validate(sys::ROMX_VALIDATE_ALL)?;
            }
        }
        if let Some(staged_output) = staged_output {
            staged_output.publish(output, options.replace_existing)?;
        }
        Ok(report)
    }
}

fn infer_platform_id(path: &str) -> u16 {
    let extension = Path::new(path)
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let name = match extension.as_str() {
        "gb" => "gb",
        "gbc" => "gbc",
        "gba" => "gba",
        "nes" => "nes",
        "sfc" | "smc" => "snes",
        "nds" => "nds",
        "3ds" | "cci" | "cxi" | "app" => "3ds",
        "sms" => "sms",
        "gg" => "gg",
        "md" | "gen" | "smd" | "32x" => "genesis",
        "pce" => "pce",
        "pbp" | "prx" => "psp",
        "gcm" | "wbfs" | "rvz" | "wia" | "wad" => "gamecube",
        "zip" => "arcade",
        "iso" | "cso" | "zso" | "chd" | "cdi" | "cue" | "gdi" | "m3u" | "ccd" | "mds" | "toc"
        | "bin" | "img" | "mdf" | "sbi" | "sub" | "ecm" => "playstation",
        _ => "arcade",
    };
    super::platform_id_for_name(name).max(1)
}

fn write_save_bundles(
    output: &Path,
    bundles: &[crate::MutableSaveBundle],
    _options: &PackOptions,
) -> Result<(), RomxError> {
    let destination = c_path(output)?;
    for bundle in bundles {
        let key = c_string(&bundle.key, "mutable save key")?;
        let mut entries = Vec::with_capacity(bundle.files.len());
        let mut relative_paths = Vec::with_capacity(bundle.files.len());
        let mut source_paths = Vec::with_capacity(bundle.files.len());
        for file in &bundle.files {
            let relative = c_string(&file.path, "mutable save path")?;
            let source = c_path(&file.source)?;
            relative_paths.push(relative);
            source_paths.push(source);
            let mut entry: sys::romx_mutable_bundle_path_entry_t = unsafe { std::mem::zeroed() };
            entry.struct_size =
                u32::try_from(std::mem::size_of::<sys::romx_mutable_bundle_path_entry_t>())
                    .expect("libromx ABI structure size must fit in the C u32 field");
            entry.relative_path = relative_paths.last().expect("path just pushed").as_ptr();
            entry.source_path = source_paths.last().expect("path just pushed").as_ptr();
            entries.push(entry);
        }
        let mut bundle_options: sys::romx_mutable_bundle_options_t = unsafe { std::mem::zeroed() };
        bundle_options.struct_size =
            u32::try_from(std::mem::size_of::<sys::romx_mutable_bundle_options_t>())
                .expect("libromx ABI structure size must fit in the C u32 field");
        // `mutable_entry_capacity` reserves RMBL directory slots, whereas
        // `max_entry_count` limits files inside one bundle. They are separate
        // units; a PSP/3DS directory can legitimately contain more files
        // than the default eight object slots.
        bundle_options.max_entry_count = sys::ROMX_MUTABLE_BUNDLE_DEFAULT_MAX_ENTRIES
            .max(checked_u32(entries.len(), "mutable entry count")?);
        bundle_options.io_chunk_size = 1024 * 1024;

        // The write call owns path validation and bundle sizing. Avoid a
        // separate measure pass: measuring first would reread every save
        // source before the streaming write starts.
        let mut write_options: sys::romx_mutable_write_options_t = unsafe { std::mem::zeroed() };
        write_options.struct_size =
            u32::try_from(std::mem::size_of::<sys::romx_mutable_write_options_t>())
                .expect("libromx ABI structure size must fit in the C u32 field");
        write_options.io_chunk_size = 1024 * 1024;
        let mut written: sys::romx_mutable_object_info_t = unsafe { std::mem::zeroed() };
        written.struct_size = u32::try_from(std::mem::size_of::<sys::romx_mutable_object_info_t>())
            .expect("libromx ABI structure size must fit in the C u32 field");
        let mut error: sys::romx_error_t = unsafe { std::mem::zeroed() };
        let code = unsafe {
            sys::romx_mutable_bundle_write_path_entries(
                destination.as_ptr(),
                sys::ROMX_MUTABLE_NAMESPACE_SAVE,
                key.as_ptr(),
                entries.as_ptr(),
                checked_u32(entries.len(), "mutable entry count")?,
                &bundle_options,
                &write_options,
                &mut written,
                &mut error,
            )
        };
        check(code, &error)?;
    }
    Ok(())
}
