//! Shared error type and safety helpers for the libromx bindings.

use libromx_sys as sys;
use std::ffi::{CStr, CString};
use std::io;
use std::os::raw::c_char;
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RomxError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid JSON metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("image processing error: {0}")]
    Image(#[from] image::ImageError),
    #[error("invalid ROMX: {0}")]
    Invalid(String),
    #[error("ROMX immutable SHA-256 mismatch")]
    BodyHashMismatch,
    #[error("metadata is invalid: {0}")]
    Metadata(String),
    #[error("cover is invalid: {0}")]
    Cover(String),
    #[error("output already exists: {0}")]
    Exists(std::path::PathBuf),
    #[error("operation cancelled")]
    Cancelled,
    #[error("libromx error {code} (system {system_code}, offset {byte_offset}): {message}")]
    Libromx {
        code: i32,
        system_code: i32,
        byte_offset: u64,
        message: String,
    },
}

/// Convert a host path to the UTF-8 path representation required by libromx.
pub(crate) fn c_path(path: &Path) -> Result<CString, RomxError> {
    let value = path.to_str().ok_or_else(|| {
        RomxError::Invalid(format!("path is not valid UTF-8: {}", path.display()))
    })?;
    CString::new(value).map_err(|_| RomxError::Invalid("path contains an embedded NUL".into()))
}

pub(crate) fn c_string(value: &str, field: &str) -> Result<CString, RomxError> {
    CString::new(value).map_err(|_| RomxError::Invalid(format!("{field} contains an embedded NUL")))
}

pub(crate) fn c_error(error: &sys::romx_error_t, fallback_code: sys::romx_result_t) -> RomxError {
    let message = c_char_buffer(&error.message);
    RomxError::Libromx {
        code: if error.code == 0 {
            fallback_code
        } else {
            error.code
        },
        system_code: error.system_code,
        byte_offset: error.byte_offset,
        message: if message.is_empty() {
            // The C ABI normally guarantees a descriptive message, but keep
            // diagnostics useful (and memory-safe) if an older implementation
            // returns a null result string.
            let fallback = unsafe { sys::romx_result_string(fallback_code) };
            if fallback.is_null() {
                format!("libromx error code {fallback_code}")
            } else {
                unsafe { CStr::from_ptr(fallback) }
                    .to_string_lossy()
                    .into_owned()
            }
        } else {
            message
        },
    }
}

/// Decode a fixed-size C character buffer without reading past its declared
/// ABI boundary.  C implementations normally NUL-terminate this field, but
/// malformed or older implementations must not be able to trigger an
/// unbounded `CStr::from_ptr` read in the safe wrapper.
fn c_char_buffer(bytes: &[c_char]) -> String {
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let bytes = bytes[..length]
        .iter()
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub(crate) fn check(code: sys::romx_result_t, error: &sys::romx_error_t) -> Result<(), RomxError> {
    if code == sys::ROMX_OK {
        Ok(())
    } else {
        Err(c_error(error, code))
    }
}

pub(crate) fn checked_u32(value: usize, field: &str) -> Result<u32, RomxError> {
    u32::try_from(value).map_err(|_| RomxError::Invalid(format!("{field} exceeds u32")))
}

/// Decode a bounded NUL-terminated C field without trusting the advertised
/// length.  All libromx fixed buffers reserve one byte for the terminator.
pub(crate) fn c_field(bytes: &[c_char]) -> String {
    let length = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    let bytes = bytes[..length]
        .iter()
        .map(|byte| *byte as u8)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

pub(crate) fn copy_c_buffer<F>(mut call: F) -> Result<Vec<u8>, RomxError>
where
    F: FnMut(*mut std::ffi::c_void, u64, *mut u64, *mut sys::romx_error_t) -> sys::romx_result_t,
{
    let mut error = unsafe { std::mem::zeroed::<sys::romx_error_t>() };
    let mut required = 0u64;
    let code = call(std::ptr::null_mut(), 0, &mut required, &mut error);
    if code != sys::ROMX_OK && code != sys::ROMX_E_BUFFER_TOO_SMALL {
        return Err(c_error(&error, code));
    }
    let capacity = usize::try_from(required)
        .map_err(|_| RomxError::Invalid("libromx buffer size exceeds usize".into()))?;
    if capacity == 0 {
        return Ok(Vec::new());
    }
    let mut output = vec![0u8; capacity];
    error = unsafe { std::mem::zeroed() };
    let code = call(
        output.as_mut_ptr().cast(),
        required,
        &mut required,
        &mut error,
    );
    check(code, &error)?;
    let final_size = usize::try_from(required)
        .map_err(|_| RomxError::Invalid("libromx returned an oversized buffer length".into()))?;
    if final_size > output.len() {
        return Err(RomxError::Invalid(
            "libromx returned a buffer length larger than the supplied capacity".into(),
        ));
    }
    output.truncate(final_size);
    Ok(output)
}
