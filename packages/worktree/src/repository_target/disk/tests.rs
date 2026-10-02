use super::*;

fn git_run(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "{args:?}");
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn commit(root: &Path) -> String {
    git_run(root, &["-c", "core.autocrlf=false", "add", "."]);
    git_run(
        root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    git_run(root, &["rev-parse", "HEAD"])
}

#[test]
fn complete_large_export_streams_deterministically_and_detects_changes() {
    let source = tempfile::tempdir().unwrap();
    git_run(source.path(), &["init", "-q"]);
    // A single blob larger than the old whole-target limit, plus another file:
    // fixture construction, export, archive and comparison all use bounded buffers.
    let mut file = std::fs::File::create(source.path().join("large")).unwrap();
    for _ in 0..4608 {
        file.write_all(&[42; 8192]).unwrap();
    }
    drop(file);
    std::fs::write(source.path().join("script"), b"#!/bin/sh\nexit 0\n").unwrap();
    set_mode(&source.path().join("script"), "100755").unwrap();
    let revision = commit(source.path());
    let cancellation = ServiceCancellation::default();
    assert!(super::super::export(source.path(), &revision, &cancellation).is_err());
    let spool = export_spooled(
        source.path(),
        &revision,
        RepositoryExportLimits::default(),
        &cancellation,
    )
    .unwrap();
    assert_eq!(spool.byte_len(), 37_748_736 + 17);
    assert_eq!(spool.file_count(), 2);
    assert_eq!(spool.commit(), revision);
    let archive = tempfile::tempfile().unwrap();
    spool.write_archive(&mut &archive, &cancellation).unwrap();
    assert!(archive.metadata().unwrap().len() > spool.byte_len());
    let again = export_spooled(
        source.path(),
        &revision,
        RepositoryExportLimits::default(),
        &cancellation,
    )
    .unwrap();
    assert_eq!(spool.identity(), again.identity());
    std::fs::write(source.path().join("large"), b"dirty user work").unwrap();
    let destination = tempfile::tempdir().unwrap();
    spool
        .materialize(destination.path(), &cancellation)
        .unwrap();
    assert!(spool.matches(destination.path(), &cancellation).unwrap());
    assert_eq!(
        std::fs::read(source.path().join("large")).unwrap(),
        b"dirty user work"
    );
    set_mode(&destination.path().join("script"), "100644").unwrap();
    assert!(!spool.matches(destination.path(), &cancellation).unwrap());
    set_mode(&destination.path().join("script"), "100755").unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(destination.path().join("large"))
        .unwrap()
        .write_all(b"x")
        .unwrap();
    assert!(!spool.matches(destination.path(), &cancellation).unwrap());
    let storage = spool.storage.path().to_path_buf();
    drop(spool);
    assert!(!storage.exists());
}

#[test]
fn limits_cancellation_unsupported_forms_and_redirects_fail_closed() {
    let source = tempfile::tempdir().unwrap();
    git_run(source.path(), &["init", "-q"]);
    std::fs::write(source.path().join("file"), b"contents").unwrap();
    let revision = commit(source.path());
    let cancellation = ServiceCancellation::default();
    let limits = RepositoryExportLimits::default();
    for limits in [
        RepositoryExportLimits {
            max_bytes: 7,
            ..limits
        },
        RepositoryExportLimits {
            max_files: 0,
            ..limits
        },
        RepositoryExportLimits {
            max_tree_bytes: 1,
            ..limits
        },
        RepositoryExportLimits {
            blob_timeout: Duration::ZERO,
            ..limits
        },
    ] {
        assert!(export_spooled(source.path(), &revision, limits, &cancellation).is_err());
    }
    assert!(export_spooled(source.path(), "HEAD", limits, &cancellation).is_err());
    let spool = export_spooled(source.path(), &revision, limits, &cancellation).unwrap();
    let destination = tempfile::tempdir().unwrap();
    let redirect = source.path().join("redirect");
    std::os::unix::fs::symlink(destination.path(), &redirect).unwrap();
    assert!(spool.materialize(&redirect, &cancellation).is_err());
    assert!(!spool.matches(&redirect, &cancellation).unwrap());
    std::fs::remove_file(&redirect).unwrap();
    cancellation.cancel();
    assert!(export_spooled(source.path(), &revision, limits, &cancellation).is_err());
    assert!(
        spool
            .materialize(destination.path(), &cancellation)
            .is_err()
    );
    assert!(spool.matches(destination.path(), &cancellation).is_err());
    assert!(
        spool
            .write_archive(&mut std::io::sink(), &cancellation)
            .is_err()
    );
    let cancellation = ServiceCancellation::default();
    for bytes in [
        b"version https://git-lfs.github.com/spec/v1\n".as_slice(),
        b"version https://git-lfs.github.com/spec/v1\r\n",
    ] {
        std::fs::write(source.path().join("file"), bytes).unwrap();
        let revision = commit(source.path());
        assert!(
            export_spooled(source.path(), &revision, limits, &cancellation)
                .unwrap_err()
                .contains("LFS")
        );
    }
    std::os::unix::fs::symlink("file", source.path().join("link")).unwrap();
    // Delete pointer so the symlink is the rejecting entry.
    std::fs::write(source.path().join("file"), b"ordinary").unwrap();
    let revision = commit(source.path());
    assert!(
        export_spooled(source.path(), &revision, limits, &cancellation)
            .unwrap_err()
            .contains("unsupported")
    );
    std::fs::remove_file(source.path().join("link")).unwrap();
    std::fs::write(source.path().join("unsafe\\name"), b"ordinary").unwrap();
    let revision = commit(source.path());
    assert!(
        export_spooled(source.path(), &revision, limits, &cancellation)
            .unwrap_err()
            .contains("unsafe")
    );
}

#[test]
fn cancellation_during_archive_and_same_length_changes_are_detected() {
    struct CancellingWriter(ServiceCancellation);
    impl Write for CancellingWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.cancel();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let source = tempfile::tempdir().unwrap();
    git_run(source.path(), &["init", "-q"]);
    std::fs::write(source.path().join("file"), vec![42; 32_768]).unwrap();
    let revision = commit(source.path());
    let cancellation = ServiceCancellation::default();
    let spool = export_spooled(
        source.path(),
        &revision,
        RepositoryExportLimits::default(),
        &cancellation,
    )
    .unwrap();
    let destination = tempfile::tempdir().unwrap();
    spool
        .materialize(destination.path(), &cancellation)
        .unwrap();
    std::fs::write(destination.path().join("file"), vec![43; 32_768]).unwrap();
    assert!(!spool.matches(destination.path(), &cancellation).unwrap());
    assert!(
        spool
            .write_archive(&mut CancellingWriter(cancellation.clone()), &cancellation)
            .is_err()
    );
}
