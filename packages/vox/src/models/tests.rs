//! Behavioral tests use only private temporary directories and injected byte streams.
//! No model download, microphone, desktop, cloud, or environment mutation.
use super::*;
use std::{
    fs,
    io::{self, Cursor},
    os::unix::fs::{PermissionsExt, symlink},
};

const BYTES: &[u8] = b"GGUFtest artifact, not an inference model";

fn fixture() -> ModelEntry {
    static DIGEST: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let mut model = catalog().remove(0);
    model.size_bytes = BYTES.len() as u64;
    model.sha256 = DIGEST.get_or_init(|| hex::encode(Sha256::digest(BYTES)));
    model
}

fn root(temp: &tempfile::TempDir) -> PathBuf {
    temp.path().join("vox/models")
}

fn write_fixture(root: &Path, model: &ModelEntry, bytes: &[u8]) -> Result<PathBuf> {
    install_at(root, model, &Cancellation::new(), |_, target, _| {
        target.write_all(bytes)?;
        Ok(())
    })
}

#[test]
fn catalog_is_pinned_unique_and_has_attribution() {
    let models = catalog();
    let mut ids = std::collections::BTreeSet::new();
    let mut filenames = std::collections::BTreeSet::new();
    for model in models {
        assert!(ids.insert(model.id));
        assert!(filenames.insert(model.filename));
        assert!(
            model
                .url
                .starts_with("https://huggingface.co/handy-computer/")
        );
        assert!(model.url.contains(&format!("/resolve/{}/", model.revision)));
        assert_eq!(model.revision.len(), 40);
        assert_eq!(model.sha256.len(), 64);
        assert!(model.sha256.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert!(model.size_bytes > 4);
        assert_eq!(
            Path::new(model.filename).extension(),
            Some(std::ffi::OsStr::new("gguf"))
        );
        assert_eq!(Path::new(model.filename).components().count(), 1);
        assert!(!model.license.is_empty());
        assert!(!model.attribution.is_empty());
    }
}

#[test]
fn unknown_and_traversal_ids_fail_before_any_filesystem_or_network_work() {
    for id in [
        "",
        ".",
        "..",
        "../tiny.en",
        "/tmp/file",
        "tiny.en/../../file",
        "TINY.EN",
    ] {
        assert!(matches!(
            install(id, &Cancellation::new()),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(verify(id), Err(Error::Invalid(_))));
        assert!(matches!(remove(id), Err(Error::Invalid(_))));
    }
}

#[test]
fn roots_are_absolute_and_resolution_has_no_environment_mutations() {
    assert_eq!(
        resolve_data_dir(Some("/data".into()), None).unwrap(),
        PathBuf::from("/data/vox/models")
    );
    assert_eq!(
        resolve_data_dir(Some("".into()), Some("/home/alice".into())).unwrap(),
        PathBuf::from("/home/alice/.local/share/vox/models")
    );
    assert!(resolve_data_dir(None, None).is_err());
    assert!(resolve_data_dir(Some("relative".into()), Some("/home/alice".into())).is_err());
    assert!(resolve_data_dir(Some("/data/../elsewhere".into()), None).is_err());
}

#[test]
fn listing_missing_directory_is_bounded_and_nonmutating() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let listed = list_at(&root).unwrap();
    assert_eq!(listed.len(), catalog().len());
    assert!(
        listed
            .iter()
            .all(|model| model.state == ModelState::Missing)
    );
    assert!(!root.exists());
}

#[test]
fn listing_distinguishes_missing_regular_wrong_size_and_symlink_without_hashing() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let models = catalog();
    let directory = storage::Directory::open(&root, true).unwrap();
    directory
        .create(models[0].filename)
        .unwrap()
        .set_len(models[0].size_bytes)
        .unwrap();
    directory
        .create(models[1].filename)
        .unwrap()
        .write_all(b"wrong size")
        .unwrap();
    let first = list_at(&root).unwrap();
    assert_eq!(first[0].state, ModelState::Present);
    assert_eq!(first[1].state, ModelState::Rejected);
    assert_eq!(first[2].state, ModelState::Missing);
    // Same size alone is deliberately not an integrity assertion.
    assert!(verify_at(&root, &models[0], &Cancellation::new()).is_err());
    symlink(root.join(models[0].filename), root.join(models[2].filename)).unwrap();
    assert_eq!(list_at(&root).unwrap()[2].state, ModelState::Rejected);
}

#[test]
fn install_is_private_verified_and_reused_without_fetching() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let model = fixture();
    let path = write_fixture(&root, &model, BYTES).unwrap();
    assert_eq!(fs::read(&path).unwrap(), BYTES);
    assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o077, 0);
    assert_eq!(fs::metadata(&root).unwrap().permissions().mode() & 0o077, 0);
    assert_eq!(
        verify_at(&root, &model, &Cancellation::new()).unwrap(),
        path
    );
    assert_eq!(
        install_at(&root, &model, &Cancellation::new(), |_, _, _| panic!(
            "must not fetch twice"
        ))
        .unwrap(),
        path
    );
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
}

#[test]
fn verification_rejects_bad_magic_hash_truncation_and_trailing_data() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let model = fixture();
    let path = write_fixture(&root, &model, BYTES).unwrap();
    let mut bad_magic = BYTES.to_vec();
    bad_magic[0] = b'X';
    let mut bad_hash = BYTES.to_vec();
    bad_hash[4] ^= 1;
    let mut extra = BYTES.to_vec();
    extra.push(0);
    for bad in [
        bad_magic,
        bad_hash,
        BYTES[..BYTES.len() - 1].to_vec(),
        extra,
    ] {
        fs::write(&path, &bad).unwrap();
        assert!(verify_at(&root, &model, &Cancellation::new()).is_err());
        assert!(
            install_at(&root, &model, &Cancellation::new(), |_, _, _| panic!(
                "never replace existing corruption"
            ))
            .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), bad);
    }
}

#[test]
fn failed_download_does_not_publish_and_cleans_only_its_temporary() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let model = fixture();
    let directory = storage::Directory::open(&root, true).unwrap();
    directory
        .create(".download-unrelated.part")
        .unwrap()
        .write_all(b"retain")
        .unwrap();
    let result = install_at(&root, &model, &Cancellation::new(), |_, target, _| {
        target.write_all(b"GGUFpartial")?;
        Err(Error::Backend("injected transport failure".into()))
    });
    assert!(result.is_err());
    assert!(!root.join(model.filename).exists());
    assert_eq!(
        fs::read(root.join(".download-unrelated.part")).unwrap(),
        b"retain"
    );
    assert_eq!(fs::read_dir(&root).unwrap().count(), 1);
}

#[test]
fn digest_failure_does_not_publish() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let model = fixture();
    let mut bad = BYTES.to_vec();
    bad[4] ^= 1;
    assert!(write_fixture(&root, &model, &bad).is_err());
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
}

#[test]
fn cancellation_before_install_does_not_create_directories() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let cancel = Cancellation::new();
    cancel.cancel();
    assert!(matches!(
        install_at(&root, &fixture(), &cancel, |_, _, _| panic!("cancelled")),
        Err(Error::Cancelled)
    ));
    assert!(!root.exists());
}

#[test]
fn cancellation_after_download_prevents_publication_and_cleans_up() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let model = fixture();
    let cancel = Cancellation::new();
    let result = install_at(&root, &model, &cancel, |_, target, cancel| {
        target.write_all(BYTES)?;
        cancel.cancel();
        Ok(())
    });
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(!root.join(model.filename).exists());
    assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
}

#[test]
fn cancelled_verification_returns_cancelled_without_reading() {
    let model = fixture();
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let path = write_fixture(&root, &model, BYTES).unwrap();
    let cancel = Cancellation::new();
    cancel.cancel();
    assert!(matches!(
        verify_file(&mut File::open(path).unwrap(), &model, &cancel),
        Err(Error::Cancelled)
    ));
}

#[test]
fn byte_copy_is_bounded_and_rejects_short_and_long_streams() {
    let cancel = Cancellation::new();
    let mut target = Vec::new();
    copy_exact_bounded(
        &mut Cursor::new(BYTES),
        &mut target,
        BYTES.len() as u64,
        &cancel,
    )
    .unwrap();
    assert_eq!(target, BYTES);
    assert!(
        copy_exact_bounded(
            &mut Cursor::new(BYTES),
            &mut Vec::new(),
            BYTES.len() as u64 + 1,
            &cancel
        )
        .is_err()
    );
    let mut target = Vec::new();
    assert!(copy_exact_bounded(&mut Cursor::new(BYTES), &mut target, 4, &cancel).is_err());
    assert_eq!(target, b"GGUF");
}

#[test]
fn cancellation_during_read_prevents_the_next_write() {
    struct CancelRead(Cancellation);
    impl Read for CancelRead {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let Some(first) = buffer.first_mut() else {
                return Ok(0);
            };
            *first = 7;
            self.0.cancel();
            Ok(1)
        }
    }
    let cancel = Cancellation::new();
    let mut target = Vec::new();
    assert!(matches!(
        copy_exact_bounded(&mut CancelRead(cancel.clone()), &mut target, 8, &cancel),
        Err(Error::Cancelled)
    ));
    assert!(target.is_empty());
}

#[test]
fn storage_rejects_leaf_symlink_and_does_not_remove_its_target() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let model = fixture();
    storage::Directory::open(&root, true).unwrap();
    let outside = temp.path().join("precious");
    fs::write(&outside, BYTES).unwrap();
    symlink(&outside, root.join(model.filename)).unwrap();
    assert!(verify_at(&root, &model, &Cancellation::new()).is_err());
    assert!(remove_at(&root, &model).is_err());
    assert!(write_fixture(&root, &model, BYTES).is_err());
    assert_eq!(fs::read(outside).unwrap(), BYTES);
    assert!(
        fs::symlink_metadata(root.join(model.filename))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn storage_rejects_ancestor_symlinks_and_insecure_directories() {
    let temp = tempfile::tempdir().unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, temp.path().join("vox")).unwrap();
    assert!(storage::Directory::open(&root(&temp), true).is_err());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
    let other = tempfile::tempdir().unwrap();
    let root = root(&other);
    storage::Directory::open(&root, true).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(write_fixture(&root, &fixture(), BYTES).is_err());
}

#[test]
fn storage_rejects_hardlinks_and_directories_instead_of_removing_them() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let model = fixture();
    let path = write_fixture(&root, &model, BYTES).unwrap();
    let outside = temp.path().join("hardlink");
    fs::hard_link(&path, &outside).unwrap();
    assert!(remove_at(&root, &model).is_err());
    assert!(verify_at(&root, &model, &Cancellation::new()).is_err());
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(remove_at(&root, &model).is_err());
    assert!(path.is_dir());
    assert_eq!(fs::read(outside).unwrap(), BYTES);
}

#[test]
fn concurrent_operations_are_busy_and_publication_never_clobbers() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let model = fixture();
    let directory = storage::Directory::open(&root, true).unwrap();
    directory.lock().unwrap();
    assert!(matches!(
        write_fixture(&root, &model, BYTES),
        Err(Error::Busy)
    ));
    let mut temporary = Temporary::create(&directory).unwrap();
    temporary.file.write_all(BYTES).unwrap();
    directory
        .create(model.filename)
        .unwrap()
        .write_all(b"winner")
        .unwrap();
    assert!(directory.publish(&temporary.name, model.filename).is_err());
    assert_eq!(fs::read(root.join(model.filename)).unwrap(), b"winner");
}

#[test]
fn removal_is_exact_and_idempotent_including_a_missing_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let model = fixture();
    remove_at(&root, &model).unwrap();
    assert!(!root.exists());
    let path = write_fixture(&root, &model, BYTES).unwrap();
    let unrelated = root.join("unrelated.gguf");
    fs::write(&unrelated, b"keep").unwrap();
    remove_at(&root, &model).unwrap();
    remove_at(&root, &model).unwrap();
    assert!(!path.exists());
    assert_eq!(fs::read(unrelated).unwrap(), b"keep");
}

#[test]
fn directory_handles_remain_confined_after_path_replacement() {
    let temp = tempfile::tempdir().unwrap();
    let root = root(&temp);
    let directory = storage::Directory::open(&root, true).unwrap();
    let moved = temp.path().join("moved-models");
    fs::rename(&root, &moved).unwrap();
    let outside = temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, &root).unwrap();
    directory
        .create("confined.gguf")
        .unwrap()
        .write_all(BYTES)
        .unwrap();
    assert_eq!(fs::read(moved.join("confined.gguf")).unwrap(), BYTES);
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
}
