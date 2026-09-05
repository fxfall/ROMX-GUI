//! Raw bindings to the vendored libromx C ABI.
//!
//! This crate intentionally contains no safe wrappers or ROMX business logic.
//! The generated bindings are committed so normal builds do not require
//! libclang or bindgen.  Regenerate them with `scripts/generate-bindings.sh`
//! after changing the pinned libromx revision.

#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case)]
#![allow(clippy::all)]

pub mod bindings;
pub use bindings::*;

// bindgen cannot reliably represent integer macros which are based on
// UINT*_C/UINT64_MAX on every target.  These values are part of the public C
// ABI and are kept here as raw constants (not interpreted by this crate).
pub const ROMX_FORMAT_VERSION: u32 = 2;
pub const ROMX_FOOTER_SIZE: u32 = 128;
pub const ROMX_RIDX_HEADER_SIZE: u32 = 64;
pub const ROMX_RIDX_ENTRY_SIZE: u32 = 512;
pub const ROMX_RIDX_PATH_CAPACITY: u32 = 480;
pub const ROMX_IMMUTABLE_HASH_NONE: u32 = 0;
pub const ROMX_IMMUTABLE_HASH_SHA256: u32 = 1;
pub const ROMX_RIDX_ENTRYPOINT: u32 = 0x0000_0001;
pub const ROMX_RIDX_HAS_CRC32: u32 = 0x0000_0002;
pub const ROMX_RIDX_FLAGS_MASK: u32 = 0x0000_0003;
pub const ROMX_FORMAT_UNKNOWN: u16 = 0x0000;
pub const ROMX_FORMAT_GB: u16 = 0x0001;
pub const ROMX_FORMAT_GBC: u16 = 0x0002;
pub const ROMX_FORMAT_GBA: u16 = 0x0003;
pub const ROMX_FORMAT_NES: u16 = 0x0004;
pub const ROMX_FORMAT_UNF: u16 = 0x0005;
pub const ROMX_FORMAT_UNIF: u16 = 0x0006;
pub const ROMX_FORMAT_FDS: u16 = 0x0007;
pub const ROMX_FORMAT_SFC: u16 = 0x0008;
pub const ROMX_FORMAT_SMC: u16 = 0x0009;
pub const ROMX_FORMAT_NDS: u16 = 0x000a;
pub const ROMX_FORMAT_N3DS: u16 = 0x000b;
pub const ROMX_FORMAT_CCI: u16 = 0x000c;
pub const ROMX_FORMAT_CXI: u16 = 0x000d;
pub const ROMX_FORMAT_APP: u16 = 0x000e;
pub const ROMX_FORMAT_ISO: u16 = 0x0010;
pub const ROMX_FORMAT_CSO: u16 = 0x0011;
pub const ROMX_FORMAT_ZSO: u16 = 0x0012;
pub const ROMX_FORMAT_CHD: u16 = 0x0013;
pub const ROMX_FORMAT_PBP: u16 = 0x0014;
pub const ROMX_FORMAT_CDI: u16 = 0x0015;
pub const ROMX_FORMAT_GCM: u16 = 0x0016;
pub const ROMX_FORMAT_WBFS: u16 = 0x0017;
pub const ROMX_FORMAT_RVZ: u16 = 0x0018;
pub const ROMX_FORMAT_WIA: u16 = 0x0019;
pub const ROMX_FORMAT_WAD: u16 = 0x001a;
pub const ROMX_FORMAT_CUE: u16 = 0x0020;
pub const ROMX_FORMAT_GDI: u16 = 0x0021;
pub const ROMX_FORMAT_M3U: u16 = 0x0022;
pub const ROMX_FORMAT_CCD: u16 = 0x0023;
pub const ROMX_FORMAT_MDS: u16 = 0x0024;
pub const ROMX_FORMAT_TOC: u16 = 0x0025;
pub const ROMX_FORMAT_BIN: u16 = 0x0030;
pub const ROMX_FORMAT_WAV: u16 = 0x0031;
pub const ROMX_FORMAT_FLAC: u16 = 0x0032;
pub const ROMX_FORMAT_IMG: u16 = 0x0033;
pub const ROMX_FORMAT_MDF: u16 = 0x0034;
pub const ROMX_FORMAT_SBI: u16 = 0x0040;
pub const ROMX_FORMAT_SUB: u16 = 0x0041;
pub const ROMX_FORMAT_ECM: u16 = 0x0042;
pub const ROMX_FORMAT_Z64: u16 = 0x0050;
pub const ROMX_FORMAT_N64: u16 = 0x0051;
pub const ROMX_FORMAT_V64: u16 = 0x0052;
pub const ROMX_FORMAT_MD: u16 = 0x0060;
pub const ROMX_FORMAT_GEN: u16 = 0x0061;
pub const ROMX_FORMAT_SMD: u16 = 0x0062;
pub const ROMX_FORMAT_X32: u16 = 0x0063;
pub const ROMX_FORMAT_SMS: u16 = 0x0064;
pub const ROMX_FORMAT_GG: u16 = 0x0065;
pub const ROMX_FORMAT_PCE: u16 = 0x0066;
pub const ROMX_FORMAT_ELF: u16 = 0x0070;
pub const ROMX_FORMAT_PRX: u16 = 0x0071;
pub const ROMX_FORMAT_MSU: u16 = 0x0080;
pub const ROMX_FORMAT_PCM: u16 = 0x0081;
pub const ROMX_FORMAT_ROMX_LAUNCH_DESCRIPTOR: u16 = 0x0090;
pub const ROMX_FORMAT_ZIP: u16 = 0x0091;
pub const ROMX_PLATFORM_UNSPECIFIED: u16 = 0x0000;
pub const ROMX_PLATFORM_GAME_BOY: u16 = 0x0001;
pub const ROMX_PLATFORM_GAME_BOY_COLOR: u16 = 0x0002;
pub const ROMX_PLATFORM_GAME_BOY_ADVANCE: u16 = 0x0003;
pub const ROMX_PLATFORM_NES: u16 = 0x0004;
pub const ROMX_PLATFORM_SNES: u16 = 0x0005;
pub const ROMX_PLATFORM_NINTENDO_64: u16 = 0x0006;
pub const ROMX_PLATFORM_NINTENDO_DS: u16 = 0x0007;
pub const ROMX_PLATFORM_NINTENDO_3DS: u16 = 0x0008;
pub const ROMX_PLATFORM_MASTER_SYSTEM: u16 = 0x0010;
pub const ROMX_PLATFORM_GAME_GEAR: u16 = 0x0011;
pub const ROMX_PLATFORM_MEGA_DRIVE: u16 = 0x0012;
pub const ROMX_PLATFORM_MEGA_DRIVE_32X: u16 = 0x0013;
pub const ROMX_PLATFORM_SEGA_CD: u16 = 0x0014;
pub const ROMX_PLATFORM_SEGA_SATURN: u16 = 0x0015;
pub const ROMX_PLATFORM_DREAMCAST: u16 = 0x0016;
pub const ROMX_PLATFORM_PC_ENGINE: u16 = 0x0020;
pub const ROMX_PLATFORM_PC_ENGINE_CD: u16 = 0x0021;
pub const ROMX_PLATFORM_PLAYSTATION: u16 = 0x0030;
pub const ROMX_PLATFORM_PLAYSTATION_2: u16 = 0x0031;
pub const ROMX_PLATFORM_PSP: u16 = 0x0032;
pub const ROMX_PLATFORM_GAMECUBE: u16 = 0x0040;
pub const ROMX_PLATFORM_WII: u16 = 0x0041;
pub const ROMX_PLATFORM_ARCADE: u16 = 0x0050;
pub const ROMX_PLATFORM_SCUMMVM: u16 = 0x0060;
pub const ROMX_PLATFORM_DOS: u16 = 0x0061;
pub const ROMX_PLATFORM_AMIGA: u16 = 0x0062;
pub const ROMX_LAUNCH_UNSPECIFIED: u16 = 0x0000;
pub const ROMX_LAUNCH_RAW_SINGLE_FILE: u16 = 0x0001;
pub const ROMX_LAUNCH_CUE: u16 = 0x0002;
pub const ROMX_LAUNCH_GDI: u16 = 0x0003;
pub const ROMX_LAUNCH_M3U: u16 = 0x0004;
pub const ROMX_LAUNCH_CCD: u16 = 0x0005;
pub const ROMX_LAUNCH_MDS: u16 = 0x0006;
pub const ROMX_LAUNCH_TOC: u16 = 0x0007;
pub const ROMX_LAUNCH_DIRECTORY: u16 = 0x0008;
pub const ROMX_LAUNCH_ROMSET: u16 = 0x0009;
pub const ROMX_LAUNCH_SPLIT_FILE_SET: u16 = 0x000a;
pub const ROMX_READER_OPTIONS_INIT_SIZE: u32 = 56;
pub const ROMX_DEFAULT_MAX_METADATA_SIZE: u64 = 1_048_576;
pub const ROMX_DEFAULT_MAX_COVER_SIZE: u64 = 33_554_432;
pub const ROMX_DEFAULT_MAX_COVER_DIMENSION: u32 = 8192;
pub const ROMX_DEFAULT_IO_CHUNK_SIZE: u32 = 65_536;
pub const ROMX_PAYLOAD_FILE_VALIDATE_IMMUTABLE_SHA256: u32 = 0x0000_0001;
pub const ROMX_PAYLOAD_SEEK_START: romx_payload_seek_position_t = 0;
pub const ROMX_PAYLOAD_SEEK_CURRENT: romx_payload_seek_position_t = 1;
pub const ROMX_PAYLOAD_SEEK_END: romx_payload_seek_position_t = 2;
pub const ROMX_VALIDATE_PAYLOAD_HASHES: u32 = 0x0000_0001;
pub const ROMX_EXTRACT_REPLACE_EXISTING: u32 = 0x0000_0001;
pub const ROMX_EXTRACT_DURABLE: u32 = 0x0000_0002;
pub const ROMX_PROBE_HAS_NAME: u32 = 0x0000_0001;
pub const ROMX_PROBE_HAS_SERIAL: u32 = 0x0000_0002;
pub const ROMX_PROBE_HAS_COVER: u32 = 0x0000_0004;

pub const ROMX_OK: romx_result_t = 0;
pub const ROMX_E_INVALID_ARGUMENT: romx_result_t = -1;
pub const ROMX_E_OUT_OF_MEMORY: romx_result_t = -2;
pub const ROMX_E_IO: romx_result_t = -3;
pub const ROMX_E_TRUNCATED: romx_result_t = -4;
pub const ROMX_E_INVALID_FOOTER: romx_result_t = -5;
pub const ROMX_E_INVALID_FLAGS: romx_result_t = -6;
pub const ROMX_E_RANGE: romx_result_t = -7;
pub const ROMX_E_OVERLAP: romx_result_t = -8;
pub const ROMX_E_IMMUTABLE_HASH: romx_result_t = -9;
pub const ROMX_E_METADATA_ABSENT: romx_result_t = -10;
pub const ROMX_E_METADATA_TOO_LARGE: romx_result_t = -11;
pub const ROMX_E_METADATA_UTF8: romx_result_t = -12;
pub const ROMX_E_METADATA_JSON: romx_result_t = -13;
pub const ROMX_E_METADATA_SCHEMA: romx_result_t = -14;
pub const ROMX_E_COVER_ABSENT: romx_result_t = -15;
pub const ROMX_E_COVER_TOO_LARGE: romx_result_t = -16;
pub const ROMX_E_COVER_PNG: romx_result_t = -17;
pub const ROMX_E_EXTRACT_HASH: romx_result_t = -18;
pub const ROMX_E_BUFFER_TOO_SMALL: romx_result_t = -19;
pub const ROMX_E_WRITE: romx_result_t = -20;
pub const ROMX_E_ATOMIC_RENAME: romx_result_t = -21;
pub const ROMX_E_EXISTS: romx_result_t = -22;
pub const ROMX_E_UNSUPPORTED: romx_result_t = -23;
pub const ROMX_E_INDEX: romx_result_t = -24;
pub const ROMX_E_VIRTUAL_PATH: romx_result_t = -25;
pub const ROMX_E_ENTRY_NOT_FOUND: romx_result_t = -26;
pub const ROMX_E_ENTRY_CRC: romx_result_t = -27;
pub const ROMX_E_MUTABLE_ABSENT: romx_result_t = -28;
pub const ROMX_E_MUTABLE_HEADER: romx_result_t = -29;
pub const ROMX_E_MUTABLE_ENTRY: romx_result_t = -30;
pub const ROMX_E_MUTABLE_DATA_CRC: romx_result_t = -31;
pub const ROMX_E_MUTABLE_NO_SPACE: romx_result_t = -32;
pub const ROMX_E_MUTABLE_BUNDLE: romx_result_t = -33;
pub const ROMX_E_MUTABLE_STATS: romx_result_t = -34;

pub const ROMX_WRITER_IMMUTABLE_SHA256: u32 = 0x01;
pub const ROMX_WRITER_REPLACE_EXISTING: u32 = 0x02;
pub const ROMX_WRITER_DURABLE: u32 = 0x04;
pub const ROMX_WRITER_PROBE_PAYLOAD: u32 = 0x08;
pub const ROMX_WRITER_COMPUTE_METADATA_CRC32: u32 = 0x10;
pub const ROMX_WRITER_DIRECT_OUTPUT: u32 = 0x20;
pub const ROMX_VALIDATE_IMMUTABLE_SHA256: u32 = 0x02;
pub const ROMX_VALIDATE_METADATA: u32 = 0x04;
pub const ROMX_VALIDATE_COVER: u32 = 0x08;
pub const ROMX_VALIDATE_ENTRY_CRC32: u32 = 0x10;
pub const ROMX_VALIDATE_ALL: u32 = 0x1f;

pub const ROMX_REGION_PAYLOAD: romx_region_t = 1;
pub const ROMX_REGION_METADATA: romx_region_t = 2;
pub const ROMX_REGION_COVER: romx_region_t = 3;
pub const ROMX_REGION_PAYLOAD_INDEX: romx_region_t = 4;
pub const ROMX_REGION_MUTABLE: romx_region_t = 5;
pub const ROMX_REGION_IMMUTABLE: romx_region_t = 6;

pub const ROMX_MUTABLE_NAMESPACE_SAVE: romx_mutable_namespace_t = 1;
pub const ROMX_MUTABLE_NAMESPACE_CHEAT: romx_mutable_namespace_t = 2;
pub const ROMX_MUTABLE_NAMESPACE_STATS: romx_mutable_namespace_t = 3;
pub const ROMX_MUTABLE_NAMESPACE_PRIVATE: romx_mutable_namespace_t = 4;
pub const ROMX_MUTABLE_ABSENT: romx_mutable_status_t = 0;
pub const ROMX_MUTABLE_VALID: romx_mutable_status_t = 1;
pub const ROMX_MUTABLE_DEGRADED: romx_mutable_status_t = 2;
pub const ROMX_MUTABLE_INVALID: romx_mutable_status_t = 3;
pub const ROMX_MUTABLE_KEY_CAPACITY: u32 = 448;
pub const ROMX_MUTABLE_BUNDLE_VERSION: u16 = 1;
pub const ROMX_MUTABLE_BUNDLE_PATH_CAPACITY: u32 = 1024;
pub const ROMX_MUTABLE_BUNDLE_DEFAULT_MAX_ENTRIES: u32 = 4096;
pub const ROMX_MUTABLE_BUNDLE_DEFAULT_MAX_SIZE: u64 = 134_217_728;
pub const ROMX_SAVE_DEFAULT_MAX_CANDIDATES: u32 = 4096;
pub const ROMX_SAVE_DEFAULT_MAX_FILES: u32 = 4096;
pub const ROMX_SAVE_DEFAULT_MAX_SIZE: u64 = 134_217_728;
pub const ROMX_SAVE_DEFAULT_MAX_DEPTH: u32 = 64;
pub const ROMX_SAVE_TITLE_ID_CAPACITY: u32 = 16;
pub const ROMX_SAVE_EXTDATA_ID_CAPACITY: u32 = 16;
pub const ROMX_SAVE_SCAN_INCLUDE_HIDDEN: u32 = 0x0000_0001;
pub const ROMX_SAVE_SCAN_TREAT_ROOT_AS_SAVE: u32 = 0x0000_0002;
pub const ROMX_SAVE_SCAN_FLAGS_MASK: u32 = 0x0000_0003;
pub const ROMX_SAVE_GROUP_UNSPECIFIED: u16 = 0;
pub const ROMX_SAVE_GROUP_SINGLE_FILE: u16 = 1;
pub const ROMX_SAVE_GROUP_DIRECTORY_PER_SAVE: u16 = 2;
pub const ROMX_SAVE_GROUP_MARKER_DIRECTORY: u16 = 3;
pub const ROMX_SAVE_SCOPE_UNSPECIFIED: u16 = 0;
pub const ROMX_SAVE_SCOPE_3DS_TITLE: u16 = 1;
pub const ROMX_SAVE_SCOPE_3DS_EXTDATA: u16 = 2;
pub const ROMX_SAVE_SOURCE_AUTO: u16 = 0;
pub const ROMX_SAVE_SOURCE_FILE: u16 = 1;
pub const ROMX_SAVE_SOURCE_DIRECTORY: u16 = 2;
pub const ROMX_SAVE_SOURCE_PSP_SAVEDATA: u16 = 3;
pub const ROMX_SAVE_SOURCE_3DS_GATEWAY: u16 = 4;
pub const ROMX_SAVE_SOURCE_3DS_SAVEDATAFILER: u16 = 5;
pub const ROMX_SAVE_SOURCE_3DS_CITRA: u16 = 6;
pub const ROMX_SAVE_SOURCE_3DS_BACKUP: u16 = 7;
pub const ROMX_SAVE_SOURCE_ROMX_BUNDLE: u16 = 8;
pub const ROMX_SAVE_CANDIDATE_IS_DIRECTORY: u32 = 0x0000_0001;
pub const ROMX_SAVE_CANDIDATE_IS_MULTI_FILE: u32 = 0x0000_0002;
pub const ROMX_SAVE_CANDIDATE_HAS_TITLE_ID: u32 = 0x0000_0004;
pub const ROMX_SAVE_CANDIDATE_HAS_MARKER: u32 = 0x0000_0008;
pub const ROMX_SAVE_CANDIDATE_NEEDS_TITLE_MAP: u32 = 0x0000_0010;
pub const ROMX_MUTABLE_SAVE_LAYOUT_HAS_EXTDATA_ID: u32 = 0x0000_0001;
pub const ROMX_MUTABLE_SAVE_LAYOUT_STRICT_EXTDATA: u32 = 0x0000_0002;

#[cfg(test)]
mod abi_tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn core_abi_layout_matches_libromx_020() {
        assert_eq!(size_of::<romx_error_t>(), 272);
        assert_eq!(offset_of!(romx_error_t, message), 16);
        assert_eq!(size_of::<romx_info_t>(), 192);
        assert_eq!(offset_of!(romx_info_t, mutable_region), 96);
        assert_eq!(offset_of!(romx_info_t, immutable_sha256), 156);
        assert_eq!(size_of::<romx_reader_options_t>(), 32);
        assert_eq!(offset_of!(romx_reader_options_t, io_chunk_size), 28);
        assert_eq!(size_of::<romx_writer_options_t>(), 56);
        assert_eq!(offset_of!(romx_writer_options_t, mutable_capacity), 16);
    }

    #[test]
    fn mutable_and_save_abi_layout_matches_libromx_020() {
        assert_eq!(size_of::<romx_mutable_object_info_t>(), 520);
        assert_eq!(offset_of!(romx_mutable_object_info_t, key), 64);
        assert_eq!(size_of::<romx_mutable_write_options_t>(), 32);
        assert_eq!(offset_of!(romx_mutable_write_options_t, data_capacity), 8);
        assert_eq!(size_of::<romx_mutable_bundle_options_t>(), 32);
        assert_eq!(
            offset_of!(romx_mutable_bundle_options_t, max_bundle_size),
            16
        );
        assert_eq!(size_of::<romx_mutable_bundle_path_entry_t>(), 24);
        assert_eq!(
            offset_of!(romx_mutable_bundle_path_entry_t, source_path),
            16
        );
        assert_eq!(size_of::<romx_mutable_bundle_entry_info_t>(), 1056);
        assert_eq!(offset_of!(romx_mutable_bundle_entry_info_t, path), 24);
        assert_eq!(size_of::<romx_mutable_save_layout_info_t>(), 40);
        assert_eq!(offset_of!(romx_mutable_save_layout_info_t, extdata_id), 20);
        assert_eq!(size_of::<romx_mutable_save_slot_info_t>(), 2088);
        assert_eq!(
            offset_of!(romx_mutable_save_slot_info_t, display_name),
            1057
        );
        assert_eq!(size_of::<romx_save_profile_info_t>(), 1048);
        assert_eq!(offset_of!(romx_save_profile_info_t, marker), 20);
        assert_eq!(size_of::<romx_save_scan_options_t>(), 40);
        assert_eq!(offset_of!(romx_save_scan_options_t, max_total_size), 24);
        assert_eq!(size_of::<romx_save_candidate_info_t>(), 1560);
        assert_eq!(offset_of!(romx_save_candidate_info_t, extdata_id), 1540);
        assert_eq!(size_of::<romx_save_file_info_t>(), 1056);
        assert_eq!(offset_of!(romx_save_file_info_t, path), 24);
        assert_eq!(size_of::<romx_validation_report_t>(), 148);
        assert_eq!(
            offset_of!(romx_validation_report_t, computed_immutable_sha256),
            72
        );
        assert_eq!(size_of::<romx_extract_options_t>(), 8);
        assert_eq!(ROMX_FORMAT_VERSION, 2);
        assert_eq!(ROMX_RIDX_ENTRY_SIZE, 512);
        assert_eq!(ROMX_MUTABLE_NAMESPACE_SAVE, 1);
        assert_eq!(ROMX_OFFSET_UNKNOWN, u64::MAX);
    }
}
