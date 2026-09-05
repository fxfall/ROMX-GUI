use romx_core::{
    extract_mutable_save_object, pack_bytes_with_writer_options, read_mutable_save_objects,
    save_profile, MutableSaveBundle, MutableSaveFile, PackOptions, SaveCatalog, SaveScope,
    SaveSourceFormat, RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY,
};

fn write_fixture_file(path: &std::path::Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn strict_savedatafiler_extdata_is_one_candidate_and_one_object() {
    let root = tempfile::tempdir().unwrap();
    let timestamp = root.path().join("用户可编辑的标签");
    let extdata = timestamp.join("000016e1");
    for name in [
        "mhr_dlc.sav",
        "mhr_dlc_bk.sav",
        "mhr_game0.sav",
        "mhr_game0_bk.sav",
        "extra.bin",
    ] {
        write_fixture_file(&extdata.join(name), b"save-data");
    }
    for name in ["000016e1.dat", "000016e1_.dat", "export.log"] {
        write_fixture_file(&timestamp.join(name), b"savedatafiler");
    }

    let profile = save_profile("3ds", "cci");
    let catalog = SaveCatalog::open(root.path(), &profile).unwrap();
    assert_eq!(catalog.candidate_count().unwrap(), 1);
    let candidate = catalog.candidate(0).unwrap();
    assert_eq!(
        candidate.source_format,
        SaveSourceFormat::ThreeDsSavedatafiler
    );
    assert_eq!(candidate.scope, SaveScope::ThreeDsExtData);
    assert_eq!(candidate.extdata_id.as_deref(), Some("00000000000016E1"));
    assert_eq!(candidate.files.len(), 8);
    assert!(candidate.files.iter().any(|file| file.path == "export.log"));
    assert!(candidate
        .files
        .iter()
        .any(|file| file.path == "000016e1/extra.bin"));

    let measured = catalog.measure_candidate(0).unwrap();
    assert!(measured > 0);

    let romx = root.path().join("fixture.romx");
    let bytes = pack_bytes_with_writer_options(
        b"fixture-payload",
        None,
        None,
        &PackOptions {
            mutable_capacity: RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY,
            ..Default::default()
        },
    )
    .unwrap();
    std::fs::write(&romx, bytes).unwrap();
    let written = catalog.write_candidate(0, &romx, None, None, None).unwrap();
    assert_eq!(written.data_size, measured);

    let objects = read_mutable_save_objects(&romx).unwrap();
    assert_eq!(objects.objects.len(), 1);
    assert_eq!(objects.objects[0].key, candidate.key);
    assert_eq!(objects.objects[0].files.len(), 8);
    assert!(objects.objects[0]
        .files
        .iter()
        .any(|file| file.path == "export.log"));
}

#[test]
fn root_as_save_is_explicit_and_does_not_change_default_collection_scan() {
    let root = tempfile::tempdir().unwrap();
    write_fixture_file(&root.path().join("slot-a/save00.bin"), b"a");
    write_fixture_file(&root.path().join("slot-b/save00.bin"), b"b");
    let profile = save_profile("3ds", "cci");

    let collection = SaveCatalog::open(root.path(), &profile).unwrap();
    assert_eq!(collection.candidate_count().unwrap(), 2);

    let root_as_save = SaveCatalog::open_with_flags(
        root.path(),
        &profile,
        libromx_sys::ROMX_SAVE_SCAN_TREAT_ROOT_AS_SAVE,
    )
    .unwrap();
    assert_eq!(root_as_save.candidate_count().unwrap(), 1);
    assert_eq!(root_as_save.candidate(0).unwrap().files.len(), 2);
}

#[cfg(unix)]
#[test]
fn mutable_save_extraction_rejects_symlinked_directories() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("save-source.bin");
    std::fs::write(&source, b"save-bytes").unwrap();
    let romx = root.path().join("symlink-fixture.romx");
    let bytes = pack_bytes_with_writer_options(
        b"payload",
        None,
        None,
        &PackOptions {
            mutable_capacity: RECOMMENDED_CARTRIDGE_MUTABLE_CAPACITY,
            mutable_save_bundles: vec![MutableSaveBundle {
                key: "slot".into(),
                files: vec![MutableSaveFile {
                    path: "nested/save.sav".into(),
                    source,
                }],
            }],
            ..Default::default()
        },
    )
    .unwrap();
    std::fs::write(&romx, bytes).unwrap();

    let output = root.path().join("output");
    let outside = root.path().join("outside");
    std::fs::create_dir_all(output.join("slot")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    symlink(&outside, output.join("slot/nested")).unwrap();

    assert!(extract_mutable_save_object(&romx, "slot", &output).is_err());
    assert!(!outside.join("save.sav").exists());
}
