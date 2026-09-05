//! Registry helpers backed by libromx's canonical 0.2.0 IDs.

use crate::RomxError;

pub fn platform_name(id: u16) -> Option<String> {
    let ptr = unsafe { libromx_sys::romx_platform_name(id) };
    (!ptr.is_null()).then(|| {
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    })
}

pub fn format_name(id: u16) -> Option<String> {
    let ptr = unsafe { libromx_sys::romx_file_format_name(id) };
    (!ptr.is_null()).then(|| {
        unsafe { std::ffi::CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned()
    })
}

pub fn require_known_platform(id: u16) -> Result<(), RomxError> {
    if unsafe { libromx_sys::romx_platform_status(id) } == 1 {
        Ok(())
    } else {
        Err(RomxError::Invalid(format!(
            "unknown ROMX platform id: {id}"
        )))
    }
}
