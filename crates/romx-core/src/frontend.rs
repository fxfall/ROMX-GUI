//! Frontend-facing compatibility checks shared by GUI and importers.

use crate::{format_id_for_extension, platform_id_for_name};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontendCompatibilityReport {
    pub supported: bool,
    pub platform_id: u16,
    pub format_id: u16,
    pub platform_supported: bool,
    pub format_supported: bool,
    pub core_path: Option<PathBuf>,
    pub core_name: Option<String>,
    pub errors: Vec<String>,
    pub warnings: Vec<String>,
}

/// Check the parts of a RetroArch playlist entry that ROMX can validate
/// without launching RetroArch. Core/platform matching remains a warning
/// when only a free-form core name is supplied, because RetroArch core names
/// are not a stable public registry.
pub fn check_frontend_compatibility(
    platform: &str,
    payload_format: &str,
    core_path: Option<&Path>,
    core_name: Option<&str>,
) -> FrontendCompatibilityReport {
    let platform_id = platform_id_for_name(platform);
    let format_id = format_id_for_extension(payload_format);
    let platform_supported = platform_id != 0;
    let format_supported = format_id != 0;
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    if !platform_supported {
        errors.push(format!("unsupported ROMX platform: {}", platform.trim()));
    }
    if !format_supported {
        errors.push(format!(
            "unsupported ROMX payload format: {}",
            payload_format.trim()
        ));
    }
    if let Some(path) = core_path {
        if !path.is_file() {
            errors.push(format!("RetroArch core does not exist: {}", path.display()));
        }
    } else if core_name.is_some_and(|name| !name.trim().is_empty()) {
        warnings.push(
            "core_name is set but core_path is empty; RetroArch must resolve the core".into(),
        );
    } else {
        warnings.push("no RetroArch core is pinned; DETECT will be used".into());
    }
    if core_name.is_some_and(|name| name.trim().is_empty()) && core_path.is_some() {
        warnings.push("core_path is set but core_name is empty".into());
    }
    FrontendCompatibilityReport {
        supported: errors.is_empty(),
        platform_id,
        format_id,
        platform_supported,
        format_supported,
        core_path: core_path.map(Path::to_owned),
        core_name: core_name.map(str::to_owned),
        errors,
        warnings,
    }
}

#[cfg(test)]
mod tests {
    use super::check_frontend_compatibility;
    use std::path::Path;

    #[test]
    fn reports_unknown_platform_and_format_as_errors() {
        let report = check_frontend_compatibility("unknown", "unknown", None, None);
        assert!(!report.supported);
        assert_eq!(report.errors.len(), 2);
    }

    #[test]
    fn detects_a_missing_pinned_core() {
        let report = check_frontend_compatibility(
            "gba",
            "gba",
            Some(Path::new("/definitely/missing/core")),
            Some("GBA"),
        );
        assert!(!report.supported);
        assert!(report.errors.iter().any(|error| error.contains("core")));
    }
}
