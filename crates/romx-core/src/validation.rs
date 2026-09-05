//! Validation adapters for the libromx reader.

use crate::{CoverInfo, Crc32Status, Reader, RomxError, ValidationReport, ValidationStatus};
use libromx_sys as sys;

fn status(value: sys::romx_validation_status_t) -> ValidationStatus {
    match value {
        1 => ValidationStatus::Valid,
        2 => ValidationStatus::Invalid,
        3 => ValidationStatus::Absent,
        _ => ValidationStatus::NotChecked,
    }
}

fn result_message(value: sys::romx_result_t) -> Option<String> {
    if value == sys::ROMX_OK {
        return None;
    }
    let pointer = unsafe { sys::romx_result_string(value) };
    (!pointer.is_null()).then(|| {
        unsafe { std::ffi::CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned()
    })
}

pub(crate) fn from_c_report(report: &sys::romx_validation_report_t) -> ValidationReport {
    ValidationReport {
        structure: status(report.structure),
        payload_hashes: status(report.payload_hashes),
        body_sha256: status(report.immutable_sha256),
        metadata: status(report.metadata),
        cover: status(report.cover),
        cover_hashes: status(report.cover_hashes),
        metadata_result: result_message(report.metadata_result),
        cover_result: result_message(report.cover_result),
        metadata_crc32: match status(report.metadata) {
            ValidationStatus::Valid => Crc32Status::ValidLookup,
            ValidationStatus::Invalid => Crc32Status::Invalid,
            ValidationStatus::Absent => Crc32Status::Absent,
            ValidationStatus::NotChecked => Crc32Status::NotChecked,
        },
        computed_payload_crc32: (report.payload_hashes == 1)
            .then(|| format!("{:08x}", report.computed_payload_crc32)),
        computed_payload_sha256: report.computed_payload_sha256,
        computed_body_sha256: report.computed_immutable_sha256,
        computed_cover_sha256: report.computed_cover_sha256,
        cover_info: (report.cover_width != 0 && report.cover_height != 0).then_some(CoverInfo {
            width: report.cover_width,
            height: report.cover_height,
        }),
    }
}

/// Validate a ROMX path using libromx's structural and optional content checks.
pub fn validate_with_libromx(path: &std::path::Path) -> Result<ValidationReport, RomxError> {
    let reader = Reader::open(path)?;
    let report = reader.validate(sys::ROMX_VALIDATE_ALL)?;
    Ok(from_c_report(&report))
}

pub(crate) fn validate_path(path: &std::path::Path) -> Result<ValidationReport, RomxError> {
    validate_with_libromx(path)
}
