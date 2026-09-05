//! Safe RAII wrappers around libromx readers and read-only cursors.

use crate::error::{c_field, c_path, c_string, check, copy_c_buffer};
use crate::{CoverInfo, Footer, Region, RidxEntry, RomxDocument, RomxError, RomxPreview};
use libromx_sys as sys;
use serde_json::Value;
use std::ffi::c_void;
use std::io::{SeekFrom, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;

fn init<T>() -> T {
    // All public libromx structures use a leading struct_size field and are
    // explicitly zero-initializable.  Keeping initialization in one helper
    // makes it difficult to accidentally pass an undersized ABI version.
    unsafe { std::mem::zeroed() }
}

fn set_size<T>(value: &mut T) {
    let size = std::mem::size_of::<T>();
    let bytes = value as *mut T as *mut u32;
    // SAFETY: every structure passed to this helper starts with struct_size.
    unsafe {
        bytes.write(
            u32::try_from(size).expect("libromx ABI structure size must fit in the C u32 field"),
        )
    };
}

/// An immutable ROMX reader.  The C handle owns file descriptors and all
/// validation state; this wrapper only translates values and enforces Rust
/// lifetimes.  It intentionally has no `Send`/`Sync` implementation.
pub struct Reader {
    raw: NonNull<sys::romx_reader_t>,
    path: PathBuf,
}

impl Reader {
    pub fn open(path: &Path) -> Result<Self, RomxError> {
        let path_c = c_path(path)?;
        let mut options: sys::romx_reader_options_t = init();
        set_size(&mut options);
        let mut raw = std::ptr::null_mut();
        let mut error: sys::romx_error_t = init();
        let code =
            unsafe { sys::romx_reader_open_path(path_c.as_ptr(), &options, &mut raw, &mut error) };
        check(code, &error)?;
        let raw = NonNull::new(raw).ok_or_else(|| {
            RomxError::Invalid("libromx returned a null reader on success".into())
        })?;
        Ok(Self {
            raw,
            path: path.to_owned(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn as_ptr(&self) -> *const sys::romx_reader_t {
        self.raw.as_ptr()
    }

    pub fn info(&self) -> Result<sys::romx_info_t, RomxError> {
        let mut info: sys::romx_info_t = init();
        set_size(&mut info);
        let mut error: sys::romx_error_t = init();
        let code = unsafe { sys::romx_reader_get_info(self.as_ptr(), &mut info, &mut error) };
        check(code, &error)?;
        Ok(info)
    }

    pub fn entries(&self) -> Result<Vec<sys::romx_entry_info_t>, RomxError> {
        let mut count = 0u32;
        let mut error: sys::romx_error_t = init();
        let code =
            unsafe { sys::romx_reader_get_entry_count(self.as_ptr(), &mut count, &mut error) };
        check(code, &error)?;
        let capacity = usize::try_from(count)
            .map_err(|_| RomxError::Invalid("entry count exceeds usize".into()))?;
        let mut entries = Vec::with_capacity(capacity);
        for index in 0..count {
            let mut entry: sys::romx_entry_info_t = init();
            set_size(&mut entry);
            error = init();
            let code =
                unsafe { sys::romx_reader_get_entry(self.as_ptr(), index, &mut entry, &mut error) };
            check(code, &error)?;
            entries.push(entry);
        }
        Ok(entries)
    }

    pub fn entrypoint(&self) -> Result<sys::romx_entry_info_t, RomxError> {
        let mut entry: sys::romx_entry_info_t = init();
        set_size(&mut entry);
        let mut error: sys::romx_error_t = init();
        let code =
            unsafe { sys::romx_reader_get_entrypoint(self.as_ptr(), &mut entry, &mut error) };
        check(code, &error)?;
        Ok(entry)
    }

    pub fn read_entry_to<W: Write>(
        &self,
        index: u32,
        size: u64,
        output: &mut W,
    ) -> Result<u64, RomxError> {
        let mut offset = 0u64;
        let mut total = 0u64;
        let mut buffer = vec![0u8; 1024 * 1024];
        while offset < size {
            let request = (size - offset).min(buffer.len() as u64);
            let mut read = 0u64;
            let mut error: sys::romx_error_t = init();
            let code = unsafe {
                sys::romx_reader_read_entry(
                    self.as_ptr(),
                    index,
                    offset,
                    buffer.as_mut_ptr().cast::<c_void>(),
                    request,
                    &mut read,
                    &mut error,
                )
            };
            check(code, &error)?;
            if read == 0 || read > request {
                return Err(RomxError::Invalid(
                    "libromx returned an invalid read size".into(),
                ));
            }
            let read = usize::try_from(read)
                .map_err(|_| RomxError::Invalid("entry read size exceeds usize".into()))?;
            output.write_all(&buffer[..read])?;
            let read_count_u64 = u64::try_from(read)
                .map_err(|_| RomxError::Invalid("entry read size exceeds u64".into()))?;
            offset = offset
                .checked_add(read_count_u64)
                .ok_or_else(|| RomxError::Invalid("entry read offset overflow".into()))?;
            total = total
                .checked_add(read_count_u64)
                .ok_or_else(|| RomxError::Invalid("entry read size overflow".into()))?;
        }
        Ok(total)
    }

    pub fn read_entry(&self, index: u32, size: u64) -> Result<Vec<u8>, RomxError> {
        let capacity = usize::try_from(size)
            .map_err(|_| RomxError::Invalid("entry is too large for this platform".into()))?;
        let mut output = Vec::with_capacity(capacity);
        self.read_entry_to(index, size, &mut output)?;
        Ok(output)
    }

    pub fn read_region_to<W: Write>(
        &self,
        region: sys::romx_region_t,
        size: u64,
        output: &mut W,
    ) -> Result<u64, RomxError> {
        let mut offset = 0u64;
        let mut total = 0u64;
        let mut buffer = vec![0u8; 1024 * 1024];
        while offset < size {
            let request = (size - offset).min(buffer.len() as u64);
            let mut read = 0u64;
            let mut error: sys::romx_error_t = init();
            let code = unsafe {
                sys::romx_reader_read_region(
                    self.as_ptr(),
                    region,
                    offset,
                    buffer.as_mut_ptr().cast::<c_void>(),
                    request,
                    &mut read,
                    &mut error,
                )
            };
            check(code, &error)?;
            if read == 0 || read > request {
                return Err(RomxError::Invalid(
                    "libromx returned an invalid region read size".into(),
                ));
            }
            let read = usize::try_from(read)
                .map_err(|_| RomxError::Invalid("region read size exceeds usize".into()))?;
            output.write_all(&buffer[..read])?;
            let read_count_u64 = u64::try_from(read)
                .map_err(|_| RomxError::Invalid("region read size exceeds u64".into()))?;
            offset = offset
                .checked_add(read_count_u64)
                .ok_or_else(|| RomxError::Invalid("region read offset overflow".into()))?;
            total = total
                .checked_add(read_count_u64)
                .ok_or_else(|| RomxError::Invalid("region read size overflow".into()))?;
        }
        Ok(total)
    }

    pub fn read_region(&self, region: sys::romx_region_t, size: u64) -> Result<Vec<u8>, RomxError> {
        let capacity = usize::try_from(size)
            .map_err(|_| RomxError::Invalid("region is too large for this platform".into()))?;
        let mut output = Vec::with_capacity(capacity);
        self.read_region_to(region, size, &mut output)?;
        Ok(output)
    }

    pub fn metadata(&self) -> Result<Metadata<'_>, RomxError> {
        let mut raw = std::ptr::null_mut();
        let mut error: sys::romx_error_t = init();
        let code = unsafe { sys::romx_metadata_open(self.as_ptr(), &mut raw, &mut error) };
        check(code, &error)?;
        let raw = NonNull::new(raw).ok_or_else(|| {
            RomxError::Invalid("libromx returned a null metadata handle on success".into())
        })?;
        Ok(Metadata {
            raw,
            _reader: PhantomData,
        })
    }

    /// Open a VFS cursor for one indexed path.  The cursor borrows this
    /// reader, so it cannot outlive the container handle.
    pub fn vfs_file(&self, virtual_path: &str) -> Result<VfsFile<'_>, RomxError> {
        let virtual_path = c_string(virtual_path, "virtual path")?;
        let mut raw = std::ptr::null_mut();
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_vfs_file_open(self.as_ptr(), virtual_path.as_ptr(), &mut raw, &mut error)
        };
        check(code, &error)?;
        Ok(VfsFile {
            raw: NonNull::new(raw).ok_or_else(|| RomxError::Invalid("null VFS cursor".into()))?,
            _reader: PhantomData,
        })
    }

    pub fn payload_vfs_file(&self) -> Result<VfsFile<'_>, RomxError> {
        let mut raw = std::ptr::null_mut();
        let mut error: sys::romx_error_t = init();
        let code =
            unsafe { sys::romx_vfs_file_open_entrypoint(self.as_ptr(), &mut raw, &mut error) };
        check(code, &error)?;
        Ok(VfsFile {
            raw: NonNull::new(raw).ok_or_else(|| RomxError::Invalid("null VFS cursor".into()))?,
            _reader: PhantomData,
        })
    }

    pub fn metadata_json(&self) -> Result<Option<Vec<u8>>, RomxError> {
        match self.metadata() {
            Ok(metadata) => metadata.json().map(Some),
            Err(RomxError::Libromx { code, .. }) if code == sys::ROMX_E_METADATA_ABSENT => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub fn cover_info(&self) -> Result<Option<CoverInfo>, RomxError> {
        let mut info: sys::romx_cover_info_t = init();
        set_size(&mut info);
        let mut error: sys::romx_error_t = init();
        let code = unsafe { sys::romx_reader_get_cover_info(self.as_ptr(), &mut info, &mut error) };
        if code == sys::ROMX_E_COVER_ABSENT {
            return Ok(None);
        }
        check(code, &error)?;
        Ok(Some(CoverInfo {
            width: info.width,
            height: info.height,
        }))
    }

    pub fn cover_bytes(&self) -> Result<Option<Vec<u8>>, RomxError> {
        let info = self.info()?;
        if info.cover.size == 0 {
            return Ok(None);
        }
        self.read_region(sys::ROMX_REGION_COVER, info.cover.size)
            .map(Some)
    }

    pub fn validate(
        &self,
        flags: sys::romx_validate_flags_t,
    ) -> Result<sys::romx_validation_report_t, RomxError> {
        let mut report: sys::romx_validation_report_t = init();
        set_size(&mut report);
        let mut error: sys::romx_error_t = init();
        let code =
            unsafe { sys::romx_reader_validate(self.as_ptr(), flags, &mut report, &mut error) };
        check(code, &error)?;
        Ok(report)
    }

    pub fn mutable_object_count(&self) -> Result<u32, RomxError> {
        let mut count = 0u32;
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_reader_get_mutable_object_count(self.as_ptr(), &mut count, &mut error)
        };
        check(code, &error)?;
        Ok(count)
    }

    pub fn mutable_object(&self, index: u32) -> Result<sys::romx_mutable_object_info_t, RomxError> {
        let mut object: sys::romx_mutable_object_info_t = init();
        set_size(&mut object);
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_reader_get_mutable_object(self.as_ptr(), index, &mut object, &mut error)
        };
        check(code, &error)?;
        Ok(object)
    }

    pub fn mutable_status(&self) -> Result<sys::romx_mutable_status_t, RomxError> {
        let mut status = sys::ROMX_MUTABLE_ABSENT;
        let mut error: sys::romx_error_t = init();
        let code =
            unsafe { sys::romx_reader_get_mutable_status(self.as_ptr(), &mut status, &mut error) };
        check(code, &error)?;
        Ok(status)
    }

    pub fn mutable_file(
        &self,
        namespace: sys::romx_mutable_namespace_t,
        key: &str,
    ) -> Result<crate::mutable::MutableFile<'_>, RomxError> {
        crate::mutable::MutableFile::open(self, namespace, key)
    }

    pub fn mutable_bundle(
        &self,
        key: &str,
        options: Option<sys::romx_mutable_bundle_options_t>,
    ) -> Result<crate::mutable::MutableBundle<'_>, RomxError> {
        crate::mutable::MutableBundle::open(self, sys::ROMX_MUTABLE_NAMESPACE_SAVE, key, options)
    }

    pub fn to_preview(&self) -> Result<RomxPreview, RomxError> {
        let info = self.info()?;
        let entries = self
            .entries()?
            .into_iter()
            .map(entry_to_public)
            .collect::<Result<Vec<_>, _>>()?;
        let metadata = self
            .metadata_json()?
            .map(|bytes| serde_json::from_slice::<Value>(&bytes))
            .transpose()?;
        let cover = self.cover_bytes()?;
        Ok(RomxPreview {
            footer: info_to_footer(&info),
            metadata,
            cover,
            entries,
        })
    }

    pub fn to_document(&self) -> Result<RomxDocument, RomxError> {
        let preview = self.to_preview()?;
        let entry = self.entrypoint()?;
        let rom = self.read_entry(entry.index, entry.data_size)?;
        Ok(RomxDocument {
            footer: preview.footer,
            rom,
            metadata: preview.metadata,
            cover: preview.cover,
            entries: preview.entries,
        })
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        unsafe { sys::romx_reader_close(self.raw.as_ptr()) };
    }
}

pub struct Metadata<'reader> {
    raw: NonNull<sys::romx_metadata_t>,
    _reader: PhantomData<&'reader Reader>,
}

impl Metadata<'_> {
    pub fn json(&self) -> Result<Vec<u8>, RomxError> {
        copy_c_buffer(|buffer, capacity, required, error| unsafe {
            sys::romx_metadata_copy_json(self.raw.as_ptr(), buffer, capacity, required, error)
        })
    }

    pub fn get_string(&self, key: &str) -> Result<String, RomxError> {
        let key = c_string(key, "metadata key")?;
        let bytes = copy_c_buffer(|buffer, capacity, required, error| unsafe {
            sys::romx_metadata_get_string(
                self.raw.as_ptr(),
                key.as_ptr(),
                buffer.cast(),
                capacity,
                required,
                error,
            )
        })?;
        String::from_utf8(bytes)
            .map_err(|_| RomxError::Metadata("metadata string is not UTF-8".into()))
    }

    pub fn crc32(&self) -> Result<u32, RomxError> {
        let mut value = 0u32;
        let mut error: sys::romx_error_t = init();
        let code =
            unsafe { sys::romx_metadata_get_crc32(self.raw.as_ptr(), &mut value, &mut error) };
        check(code, &error)?;
        Ok(value)
    }
}

impl Drop for Metadata<'_> {
    fn drop(&mut self) {
        unsafe { sys::romx_metadata_close(self.raw.as_ptr()) };
    }
}

/// Independent payload cursor.  C owns the file descriptor and bounds every
/// operation to the entrypoint, so large payloads are never copied by open.
pub struct PayloadFile {
    raw: NonNull<sys::romx_payload_file_t>,
}

impl PayloadFile {
    pub fn open(path: &Path, validate_immutable_sha256: bool) -> Result<Self, RomxError> {
        let path = c_path(path)?;
        let mut reader_options: sys::romx_reader_options_t = init();
        set_size(&mut reader_options);
        let mut options: sys::romx_payload_file_options_t = init();
        set_size(&mut options);
        if validate_immutable_sha256 {
            options.flags = sys::ROMX_PAYLOAD_FILE_VALIDATE_IMMUTABLE_SHA256;
        }
        let mut raw = std::ptr::null_mut();
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_payload_file_open_path(
                path.as_ptr(),
                &reader_options,
                &options,
                &mut raw,
                &mut error,
            )
        };
        check(code, &error)?;
        Ok(Self {
            raw: NonNull::new(raw)
                .ok_or_else(|| RomxError::Invalid("null payload cursor".into()))?,
        })
    }

    pub fn size(&self) -> Result<u64, RomxError> {
        let mut size = 0u64;
        let mut error: sys::romx_error_t = init();
        check(
            unsafe { sys::romx_payload_file_get_size(self.raw.as_ptr(), &mut size, &mut error) },
            &error,
        )?;
        Ok(size)
    }

    pub fn tell(&self) -> Result<u64, RomxError> {
        let mut position = 0u64;
        let mut error: sys::romx_error_t = init();
        let code =
            unsafe { sys::romx_payload_file_tell(self.raw.as_ptr(), &mut position, &mut error) };
        check(code, &error)?;
        Ok(position)
    }

    pub fn seek(&mut self, position: SeekFrom) -> Result<u64, RomxError> {
        let (offset, origin) = match position {
            SeekFrom::Start(value) => (
                i64::try_from(value)
                    .map_err(|_| RomxError::Invalid("payload seek offset overflows".into()))?,
                sys::ROMX_PAYLOAD_SEEK_START,
            ),
            SeekFrom::Current(value) => (value, sys::ROMX_PAYLOAD_SEEK_CURRENT),
            SeekFrom::End(value) => (value, sys::ROMX_PAYLOAD_SEEK_END),
        };
        let mut new_position = 0u64;
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_payload_file_seek(
                self.raw.as_ptr(),
                offset,
                origin,
                &mut new_position,
                &mut error,
            )
        };
        check(code, &error)?;
        Ok(new_position)
    }

    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize, RomxError> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut bytes_read = 0u64;
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_payload_file_read(
                self.raw.as_ptr(),
                buffer.as_mut_ptr().cast(),
                u64::try_from(buffer.len())
                    .map_err(|_| RomxError::Invalid("payload buffer is too large".into()))?,
                &mut bytes_read,
                &mut error,
            )
        };
        check(code, &error)?;
        let count = usize::try_from(bytes_read)
            .map_err(|_| RomxError::Invalid("payload read size exceeds usize".into()))?;
        if count > buffer.len() {
            return Err(RomxError::Invalid(
                "libromx returned a payload read larger than the buffer".into(),
            ));
        }
        Ok(count)
    }
}

impl Drop for PayloadFile {
    fn drop(&mut self) {
        unsafe { sys::romx_payload_file_close(self.raw.as_ptr()) };
    }
}

/// An independently-owned mmap of the immutable payload.  The mapping is
/// intentionally not `Send`/`Sync`; callers can borrow the bytes for as long
/// as this handle remains alive.
pub struct PayloadMapping {
    raw: NonNull<sys::romx_payload_mapping_t>,
}

impl PayloadMapping {
    pub fn open(reader: &Reader) -> Result<Self, RomxError> {
        let mut raw = std::ptr::null_mut();
        let mut error: sys::romx_error_t = init();
        check(
            unsafe { sys::romx_reader_map_payload(reader.as_ptr(), &mut raw, &mut error) },
            &error,
        )?;
        Ok(Self {
            raw: NonNull::new(raw)
                .ok_or_else(|| RomxError::Invalid("null payload mapping".into()))?,
        })
    }

    pub fn try_as_bytes(&self) -> Result<&[u8], RomxError> {
        let ptr = unsafe { sys::romx_payload_mapping_data(self.raw.as_ptr()) };
        let size = unsafe { sys::romx_payload_mapping_size(self.raw.as_ptr()) };
        let size = usize::try_from(size)
            .map_err(|_| RomxError::Invalid("payload mapping exceeds usize".into()))?;
        if ptr.is_null() {
            if size == 0 {
                return Ok(&[]);
            }
            return Err(RomxError::Invalid(
                "libromx returned a null payload mapping".into(),
            ));
        }
        // SAFETY: libromx guarantees the mapping remains valid until Drop.
        Ok(unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) })
    }

    /// Borrow the mapped payload, returning an empty slice if the C handle is
    /// malformed. New code should prefer [`Self::try_as_bytes`] to preserve
    /// the diagnostic error.
    pub fn as_bytes(&self) -> &[u8] {
        self.try_as_bytes().unwrap_or(&[])
    }
}

impl Drop for PayloadMapping {
    fn drop(&mut self) {
        unsafe { sys::romx_payload_mapping_close(self.raw.as_ptr()) };
    }
}

/// A cursor over an indexed multi-file entry, borrowing its reader.
pub struct VfsFile<'reader> {
    raw: NonNull<sys::romx_vfs_file_t>,
    _reader: PhantomData<&'reader Reader>,
}

impl VfsFile<'_> {
    pub fn size(&self) -> Result<u64, RomxError> {
        let mut size = 0u64;
        let mut error: sys::romx_error_t = init();
        check(
            unsafe { sys::romx_vfs_file_get_size(self.raw.as_ptr(), &mut size, &mut error) },
            &error,
        )?;
        Ok(size)
    }

    pub fn tell(&self) -> Result<u64, RomxError> {
        let mut position = 0u64;
        let mut error: sys::romx_error_t = init();
        let code = unsafe { sys::romx_vfs_file_tell(self.raw.as_ptr(), &mut position, &mut error) };
        check(code, &error)?;
        Ok(position)
    }

    pub fn seek(&mut self, position: SeekFrom) -> Result<u64, RomxError> {
        let (offset, origin) = match position {
            SeekFrom::Start(value) => (
                i64::try_from(value)
                    .map_err(|_| RomxError::Invalid("VFS seek offset overflows".into()))?,
                sys::ROMX_PAYLOAD_SEEK_START,
            ),
            SeekFrom::Current(value) => (value, sys::ROMX_PAYLOAD_SEEK_CURRENT),
            SeekFrom::End(value) => (value, sys::ROMX_PAYLOAD_SEEK_END),
        };
        let mut new_position = 0u64;
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_vfs_file_seek(
                self.raw.as_ptr(),
                offset,
                origin,
                &mut new_position,
                &mut error,
            )
        };
        check(code, &error)?;
        Ok(new_position)
    }

    pub fn read(&mut self, buffer: &mut [u8]) -> Result<usize, RomxError> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut bytes_read = 0u64;
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_vfs_file_read(
                self.raw.as_ptr(),
                buffer.as_mut_ptr().cast(),
                u64::try_from(buffer.len())
                    .map_err(|_| RomxError::Invalid("VFS buffer is too large".into()))?,
                &mut bytes_read,
                &mut error,
            )
        };
        check(code, &error)?;
        if bytes_read > buffer.len() as u64 {
            return Err(RomxError::Invalid(
                "libromx returned more VFS bytes than requested".into(),
            ));
        }
        usize::try_from(bytes_read)
            .map_err(|_| RomxError::Invalid("VFS read size exceeds usize".into()))
    }
}

impl Drop for VfsFile<'_> {
    fn drop(&mut self) {
        unsafe { sys::romx_vfs_file_close(self.raw.as_ptr()) };
    }
}

fn entry_to_public(entry: sys::romx_entry_info_t) -> Result<RidxEntry, RomxError> {
    let path = c_field(&entry.path);
    let entrypoint = entry.flags & sys::ROMX_RIDX_ENTRYPOINT != 0;
    let crc32 =
        (entry.flags & sys::ROMX_RIDX_HAS_CRC32 != 0).then(|| format!("{:08x}", entry.crc32));
    Ok(RidxEntry {
        flags: entry.flags,
        format_id: entry.format_id,
        path,
        data_offset: entry.data_offset,
        data_size: entry.data_size,
        crc32,
        entrypoint,
    })
}

pub(crate) fn info_to_footer(info: &sys::romx_info_t) -> Footer {
    let mut flags = 0u32;
    if info.metadata.size != 0 {
        flags |= crate::FLAG_METADATA;
    }
    if info.cover.size != 0 {
        flags |= crate::FLAG_COVER;
    }
    if info.immutable_hash_algorithm == sys::ROMX_IMMUTABLE_HASH_SHA256 {
        flags |= crate::FLAG_BODY_SHA256;
    }
    Footer {
        version: info.version,
        rom: Region {
            offset: info.payload.offset,
            size: info.payload.size,
        },
        metadata: Region {
            offset: info.metadata.offset,
            size: info.metadata.size,
        },
        cover: Region {
            offset: info.cover.offset,
            size: info.cover.size,
        },
        mutable_capacity: info.mutable_region.size,
        platform_id: info.platform_id,
        launch_format_id: info.launch_format_id,
        immutable_hash_algorithm: info.immutable_hash_algorithm,
        immutable_sha256: info.immutable_sha256,
        footer_crc32: info.footer_crc32,
        reserved: [0; 44],
        flags,
        body_sha256: info.immutable_sha256,
    }
}

pub(crate) fn read_preview_path(path: &Path) -> Result<RomxPreview, RomxError> {
    Reader::open(path)?.to_preview()
}

pub(crate) fn read_document_path(path: &Path) -> Result<RomxDocument, RomxError> {
    Reader::open(path)?.to_document()
}
