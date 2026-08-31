//! Stable, derived identities for frontend indexing.

use crate::{crc32_u32, payload_sha256, read_path, RidxEntry, RomxDocument, RomxError};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RomxIdentity {
    /// Stable identity for the payload, independent of display title and path.
    pub romx_id: String,
    /// Stable identity for the indexed entrypoint.
    pub entry_id: String,
    pub platform_id: u16,
    pub format_id: u16,
    pub payload_crc32: String,
    pub payload_sha256: String,
    pub entry_path: String,
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

fn entrypoint(entries: &[RidxEntry]) -> Result<&RidxEntry, RomxError> {
    entries
        .iter()
        .find(|entry| entry.entrypoint)
        .ok_or_else(|| RomxError::Invalid("ROMX has no entrypoint".into()))
}

pub fn identity_from_document(document: &RomxDocument) -> Result<RomxIdentity, RomxError> {
    let entry = entrypoint(&document.entries)?;
    let crc = entry
        .crc32
        .clone()
        .unwrap_or_else(|| format!("{:08x}", crc32_u32(&document.rom)));
    let sha = payload_sha256(&document.rom);
    let sha_hex = hex(&sha);
    let identity_material = format!(
        "romx-id-v1\0{}\0{}\0{}\0{}",
        document.footer.platform_id, entry.format_id, crc, sha_hex
    );
    let romx_id = hex(&payload_sha256(identity_material.as_bytes()));
    let entry_material = format!(
        "romx-entry-v1\0{}\0{}\0{}\0{}\0{}",
        document.footer.platform_id,
        entry.format_id,
        entry.path,
        entry.data_offset,
        entry.data_size
    );
    let entry_id = hex(&payload_sha256(entry_material.as_bytes()));
    Ok(RomxIdentity {
        romx_id,
        entry_id,
        platform_id: document.footer.platform_id,
        format_id: entry.format_id,
        payload_crc32: crc,
        payload_sha256: sha_hex,
        entry_path: entry.path.clone(),
    })
}

pub fn identity_from_path(path: &Path) -> Result<RomxIdentity, RomxError> {
    identity_from_document(&read_path(path)?)
}

#[cfg(test)]
mod tests {
    use super::identity_from_document;
    use crate::{pack_bytes, read_bytes};

    #[test]
    fn identity_is_stable_for_the_same_payload() {
        let first = read_bytes(&pack_bytes(b"payload", None, None).unwrap()).unwrap();
        let second = read_bytes(&pack_bytes(b"payload", None, None).unwrap()).unwrap();
        assert_eq!(
            identity_from_document(&first).unwrap(),
            identity_from_document(&second).unwrap()
        );
    }
}
