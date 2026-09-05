use libromx_sys as sys;
use romx_core::{
    pack_to_path_with_writer_options, write_mutable_file, PackOptions, Reader,
    RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY,
};
use std::ffi::CStr;
use std::path::Path;

fn object_bytes(reader: &Reader, object: &sys::romx_mutable_object_info_t) -> Vec<u8> {
    let key = unsafe { CStr::from_ptr(object.key.as_ptr()) }
        .to_str()
        .unwrap()
        .to_owned();
    let mut file = reader.mutable_file(object.object_namespace, &key).unwrap();
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    bytes
}

fn snapshot_objects(path: &Path) -> Vec<(u16, String, u64, u64, Vec<u8>)> {
    let reader = Reader::open(path).unwrap();
    let count = reader.mutable_object_count().unwrap();
    (0..count)
        .map(|index| {
            let object = reader.mutable_object(index).unwrap();
            let key = unsafe { CStr::from_ptr(object.key.as_ptr()) }
                .to_str()
                .unwrap()
                .to_owned();
            (
                object.object_namespace,
                key,
                object.generation,
                object.modified_unix_seconds,
                object_bytes(&reader, &object),
            )
        })
        .collect()
}

#[test]
fn mutable_copy_preserves_unknown_namespaces_and_immutable_hash() {
    let root = tempfile::tempdir().unwrap();
    let payload = root.path().join("game.gba");
    let original = root.path().join("original.romx");
    let edited = root.path().join("edited.romx");
    let save = root.path().join("slot.sav");
    let cheat = root.path().join("opaque.bin");
    std::fs::write(&payload, b"same payload").unwrap();
    std::fs::write(&save, b"battery save bytes").unwrap();
    std::fs::write(&cheat, b"unknown namespace bytes").unwrap();

    let capacity = RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY;
    let options = PackOptions {
        body_sha256: true,
        mutable_capacity: capacity,
        mutable_entry_capacity: 8,
        ..Default::default()
    };
    pack_to_path_with_writer_options(&payload, None, None, &original, &options).unwrap();
    write_mutable_file(&original, sys::ROMX_MUTABLE_NAMESPACE_SAVE, "slot", &save).unwrap();
    write_mutable_file(
        &original,
        sys::ROMX_MUTABLE_NAMESPACE_CHEAT,
        "opaque",
        &cheat,
    )
    .unwrap();

    let before_reader = Reader::open(&original).unwrap();
    let before_info = before_reader.info().unwrap();
    let before_objects = snapshot_objects(&original);
    assert_eq!(before_objects.len(), 2);

    let edit_options = PackOptions {
        body_sha256: true,
        mutable_capacity: capacity,
        mutable_entry_capacity: 8,
        mutable_region_source: Some(original.clone()),
        ..Default::default()
    };
    pack_to_path_with_writer_options(&payload, None, None, &edited, &edit_options).unwrap();

    let after_reader = Reader::open(&edited).unwrap();
    let after_info = after_reader.info().unwrap();
    let after_objects = snapshot_objects(&edited);
    assert_eq!(after_info.immutable_sha256, before_info.immutable_sha256);
    assert_eq!(after_objects, before_objects);
}
