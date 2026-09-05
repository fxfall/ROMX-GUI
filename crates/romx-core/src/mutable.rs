//! RAII wrappers for libromx mutable objects and bundles.

use crate::error::{c_field, c_path, c_string, check};
use crate::{Reader, RomxError};
use libromx_sys as sys;
use std::io::SeekFrom;
use std::marker::PhantomData;
use std::ptr::NonNull;

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

pub struct MutableFile<'reader> {
    raw: NonNull<sys::romx_mutable_file_t>,
    _reader: PhantomData<&'reader Reader>,
}

impl<'reader> MutableFile<'reader> {
    pub(crate) fn open(
        reader: &'reader Reader,
        namespace: sys::romx_mutable_namespace_t,
        key: &str,
    ) -> Result<Self, RomxError> {
        let key = c_string(key, "mutable key")?;
        let mut raw = std::ptr::null_mut();
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_mutable_file_open(
                reader.as_ptr(),
                namespace,
                key.as_ptr(),
                &mut raw,
                &mut error,
            )
        };
        check(code, &error)?;
        Ok(Self {
            raw: NonNull::new(raw).ok_or_else(|| RomxError::Invalid("null mutable file".into()))?,
            _reader: PhantomData,
        })
    }

    pub fn size(&self) -> Result<u64, RomxError> {
        let mut size = 0u64;
        let mut error: sys::romx_error_t = init();
        let code =
            unsafe { sys::romx_mutable_file_get_size(self.raw.as_ptr(), &mut size, &mut error) };
        check(code, &error)?;
        Ok(size)
    }

    pub fn tell(&self) -> Result<u64, RomxError> {
        let mut position = 0u64;
        let mut error: sys::romx_error_t = init();
        let code =
            unsafe { sys::romx_mutable_file_tell(self.raw.as_ptr(), &mut position, &mut error) };
        check(code, &error)?;
        Ok(position)
    }

    pub fn seek(&mut self, position: SeekFrom) -> Result<u64, RomxError> {
        let (offset, origin) = match position {
            SeekFrom::Start(value) => (
                i64::try_from(value)
                    .map_err(|_| RomxError::Invalid("mutable seek offset overflows".into()))?,
                sys::ROMX_PAYLOAD_SEEK_START,
            ),
            SeekFrom::Current(value) => (value, sys::ROMX_PAYLOAD_SEEK_CURRENT),
            SeekFrom::End(value) => (value, sys::ROMX_PAYLOAD_SEEK_END),
        };
        let mut new_position = 0u64;
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_mutable_file_seek(
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
            sys::romx_mutable_file_read(
                self.raw.as_ptr(),
                buffer.as_mut_ptr().cast(),
                u64::try_from(buffer.len())
                    .map_err(|_| RomxError::Invalid("mutable buffer is too large".into()))?,
                &mut bytes_read,
                &mut error,
            )
        };
        check(code, &error)?;
        let count = usize::try_from(bytes_read)
            .map_err(|_| RomxError::Invalid("mutable read size exceeds usize".into()))?;
        if count > buffer.len() {
            return Err(RomxError::Invalid(
                "libromx returned a mutable read larger than the buffer".into(),
            ));
        }
        Ok(count)
    }
}

impl Drop for MutableFile<'_> {
    fn drop(&mut self) {
        unsafe { sys::romx_mutable_file_close(self.raw.as_ptr()) };
    }
}

pub struct MutableBundle<'reader> {
    raw: NonNull<sys::romx_mutable_bundle_t>,
    _reader: PhantomData<&'reader Reader>,
}

impl<'reader> MutableBundle<'reader> {
    pub(crate) fn as_ptr(&self) -> *const sys::romx_mutable_bundle_t {
        self.raw.as_ptr()
    }

    pub(crate) fn open(
        reader: &'reader Reader,
        namespace: sys::romx_mutable_namespace_t,
        key: &str,
        options: Option<sys::romx_mutable_bundle_options_t>,
    ) -> Result<Self, RomxError> {
        let key = c_string(key, "mutable key")?;
        let mut bundle_options = options.unwrap_or_else(init);
        set_size(&mut bundle_options);
        let mut raw = std::ptr::null_mut();
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_mutable_bundle_open(
                reader.as_ptr(),
                namespace,
                key.as_ptr(),
                &bundle_options,
                &mut raw,
                &mut error,
            )
        };
        check(code, &error)?;
        Ok(Self {
            raw: NonNull::new(raw)
                .ok_or_else(|| RomxError::Invalid("null mutable bundle".into()))?,
            _reader: PhantomData,
        })
    }

    pub fn entry_count(&self) -> Result<u32, RomxError> {
        let mut count = 0u32;
        let mut error: sys::romx_error_t = init();
        check(
            unsafe {
                sys::romx_mutable_bundle_get_entry_count(self.raw.as_ptr(), &mut count, &mut error)
            },
            &error,
        )?;
        Ok(count)
    }

    pub fn entry(&self, index: u32) -> Result<sys::romx_mutable_bundle_entry_info_t, RomxError> {
        let mut entry: sys::romx_mutable_bundle_entry_info_t = init();
        set_size(&mut entry);
        let mut error: sys::romx_error_t = init();
        check(
            unsafe {
                sys::romx_mutable_bundle_get_entry(self.raw.as_ptr(), index, &mut entry, &mut error)
            },
            &error,
        )?;
        Ok(entry)
    }

    pub fn read_entry_to(
        &mut self,
        index: u32,
        size: u64,
        output: &mut impl std::io::Write,
    ) -> Result<u64, RomxError> {
        let mut offset = 0u64;
        let mut total = 0u64;
        let mut buffer = vec![0u8; 1024 * 1024];
        while offset < size {
            let request = (size - offset).min(buffer.len() as u64);
            let mut read = 0u64;
            let mut error: sys::romx_error_t = init();
            let code = unsafe {
                sys::romx_mutable_bundle_read_entry(
                    self.raw.as_ptr(),
                    index,
                    offset,
                    buffer.as_mut_ptr().cast(),
                    request,
                    &mut read,
                    &mut error,
                )
            };
            check(code, &error)?;
            if read == 0 || read > request {
                return Err(RomxError::Invalid(
                    "invalid mutable bundle read size".into(),
                ));
            }
            let read = usize::try_from(read)
                .map_err(|_| RomxError::Invalid("mutable read size exceeds usize".into()))?;
            output.write_all(&buffer[..read])?;
            let read = u64::try_from(read)
                .map_err(|_| RomxError::Invalid("mutable read size exceeds u64".into()))?;
            offset = offset
                .checked_add(read)
                .ok_or_else(|| RomxError::Invalid("mutable offset overflow".into()))?;
            total = total
                .checked_add(read)
                .ok_or_else(|| RomxError::Invalid("mutable size overflow".into()))?;
        }
        Ok(total)
    }

    pub fn entry_path(&self, index: u32) -> Result<String, RomxError> {
        Ok(c_field(&self.entry(index)?.path))
    }

    /// Return the logical SAVE-slot count computed by libromx from the
    /// bundle's platform profile and relative paths.
    pub(crate) fn save_slot_count(&self) -> Result<u32, RomxError> {
        let mut count = 0u32;
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_mutable_bundle_get_save_slot_count(self.raw.as_ptr(), &mut count, &mut error)
        };
        check(code, &error)?;
        Ok(count)
    }

    /// Return one logical SAVE slot. The `is_directory` flag is authoritative
    /// for extraction; no PSP/3DS path heuristics are needed in Rust.
    pub(crate) fn save_slot(
        &self,
        index: u32,
    ) -> Result<sys::romx_mutable_save_slot_info_t, RomxError> {
        let mut slot: sys::romx_mutable_save_slot_info_t = init();
        set_size(&mut slot);
        let mut error: sys::romx_error_t = init();
        let code = unsafe {
            sys::romx_mutable_bundle_get_save_slot(self.raw.as_ptr(), index, &mut slot, &mut error)
        };
        check(code, &error)?;
        Ok(slot)
    }
}

impl Drop for MutableBundle<'_> {
    fn drop(&mut self) {
        unsafe { sys::romx_mutable_bundle_close(self.raw.as_ptr()) };
    }
}

/// Write a single mutable object through libromx without parsing RMBL bytes.
pub fn write_file(
    romx: &std::path::Path,
    namespace: sys::romx_mutable_namespace_t,
    key: &str,
    source: &std::path::Path,
) -> Result<(), RomxError> {
    let romx = c_path(romx)?;
    let key = c_string(key, "mutable key")?;
    let source = c_path(source)?;
    let mut options: sys::romx_mutable_write_options_t = init();
    set_size(&mut options);
    let mut written: sys::romx_mutable_object_info_t = init();
    set_size(&mut written);
    let mut error: sys::romx_error_t = init();
    let code = unsafe {
        sys::romx_mutable_write_path(
            romx.as_ptr(),
            namespace,
            key.as_ptr(),
            source.as_ptr(),
            &options,
            &mut written,
            &mut error,
        )
    };
    check(code, &error)
}

/// Copy a complete mutable region between two ROMX containers through
/// libromx. The C API validates both containers and streams the bytes without
/// exposing or interpreting namespaces unknown to this crate.
pub fn copy_region(
    source: &std::path::Path,
    destination: &std::path::Path,
) -> Result<(), RomxError> {
    let source = c_path(source)?;
    let destination = c_path(destination)?;
    let mut error: sys::romx_error_t = init();
    let code = unsafe {
        sys::romx_mutable_copy_region_path(source.as_ptr(), destination.as_ptr(), &mut error)
    };
    check(code, &error)
}
