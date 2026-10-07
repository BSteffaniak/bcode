#[test]
fn physical_range_helper_confines_symlink_targets() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("artifacts");
    std::fs::create_dir(&root).unwrap();
    let outside = directory.path().join("outside.bin");
    std::fs::write(&outside, b"synthetic outside bytes").unwrap();
    #[cfg(unix)]
    {
        let link = root.join("linked.bin");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(crate::read_artifact_file_range_at_length(&link, &root, 0, 10, None).is_err());
    }
    let inside = root.join("inside.bin");
    std::fs::write(&inside, b"inside").unwrap();
    let (length, bytes) =
        crate::read_artifact_file_range_at_length(&inside, &root, 1, 3, None).unwrap();
    assert_eq!(length, 6);
    assert_eq!(bytes, b"nsi");
}
