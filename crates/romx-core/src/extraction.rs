//! Extraction adapters for libromx.

use crate::error::c_path;
use crate::RomxError;
use std::path::Path;

pub fn extract_payload(path: &Path, destination: &Path, replace: bool) -> Result<(), RomxError> {
    let reader = crate::Reader::open(path)?;
    let struct_size = u32::try_from(std::mem::size_of::<libromx_sys::romx_extract_options_t>())
        .map_err(|_| RomxError::Invalid("extract options size exceeds u32".into()))?;
    let options = libromx_sys::romx_extract_options_t {
        struct_size,
        flags: if replace { 1 } else { 0 },
    };
    let destination = c_path(destination)?;
    let mut error: libromx_sys::romx_error_t = unsafe { std::mem::zeroed() };
    let code = unsafe {
        libromx_sys::romx_extract_payload_path(
            reader.as_ptr(),
            destination.as_ptr(),
            &options,
            &mut error,
        )
    };
    crate::error::check(code, &error)
}
