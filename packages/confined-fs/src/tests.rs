use std::ffi::OsStr;
use std::io::{Read as _, Write as _};

use crate::{Creation, Dir, EntryKind, OpenOptions, RenameMode};

fn name(value: &str) -> &OsStr {
    OsStr::new(value)
}

fn root() -> (tempfile::TempDir, Dir) {
    let temp = tempfile::tempdir().expect("temporary directory");
    let dir = Dir::open(temp.path()).expect("open temporary directory");
    (temp, dir)
}

fn write_new(dir: &Dir, file: &str, contents: &[u8]) {
    dir.open_file(
        name(file),
        OpenOptions::new().write(true).creation(Creation::New),
    )
    .expect("create file")
    .write_all(contents)
    .expect("write file");
}

fn read(dir: &Dir, file: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    dir.open_file(name(file), OpenOptions::new().read(true))
        .expect("open file")
        .read_to_end(&mut bytes)
        .expect("read file");
    bytes
}

#[test]
fn file_creation_modes_round_trip_contents() {
    let (_temp, dir) = root();
    write_new(&dir, "value.txt", b"first");
    assert_eq!(read(&dir, "value.txt"), b"first");

    let exclusive = dir.open_file(
        name("value.txt"),
        OpenOptions::new().write(true).creation(Creation::New),
    );
    assert_eq!(
        exclusive.unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    let missing = dir.open_file(name("missing.txt"), OpenOptions::new().read(true));
    assert_eq!(missing.unwrap_err().kind(), std::io::ErrorKind::NotFound);

    dir.open_file(name("value.txt"), OpenOptions::new().append(true))
        .unwrap()
        .write_all(b"-appended")
        .unwrap();
    assert_eq!(read(&dir, "value.txt"), b"first-appended");

    dir.open_file(
        name("value.txt"),
        OpenOptions::new().write(true).truncate(true),
    )
    .unwrap()
    .write_all(b"second")
    .unwrap();
    assert_eq!(read(&dir, "value.txt"), b"second");

    dir.open_file(
        name("created.txt"),
        OpenOptions::new().write(true).creation(Creation::IfMissing),
    )
    .unwrap();
    assert!(dir.metadata_at(name("created.txt")).unwrap().is_empty());
}

#[test]
fn options_without_access_or_with_inconsistent_writes_are_rejected() {
    let (_temp, dir) = root();
    for options in [
        OpenOptions::new(),
        OpenOptions::new().read(true).truncate(true),
        OpenOptions::new().read(true).creation(Creation::New),
        OpenOptions::new().append(true).truncate(true),
    ] {
        let error = dir.open_file(name("file"), options).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
}

#[test]
fn names_must_be_single_components() {
    let (_temp, dir) = root();
    let mut invalid = vec!["", ".", "..", "a/b"];
    if cfg!(windows) {
        invalid.extend([r"a\b", "a:stream", "trailing.", "trailing "]);
    }
    for candidate in invalid {
        let error = dir
            .open_file(
                name(candidate),
                OpenOptions::new().write(true).creation(Creation::New),
            )
            .unwrap_err();
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidInput,
            "{candidate:?}"
        );
        assert!(dir.create_dir(name(candidate)).is_err(), "{candidate:?}");
    }
}

#[test]
fn unicode_names_round_trip_through_enumeration() {
    let (_temp, dir) = root();
    for file in ["状态.txt", "emoji-🦀.txt", "e\u{301}.txt"] {
        write_new(&dir, file, file.as_bytes());
        assert_eq!(read(&dir, file), file.as_bytes());
    }
    let mut names = dir
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().into_name().into_string().unwrap())
        .collect::<Vec<_>>();
    names.sort();
    let mut expected = vec!["状态.txt", "emoji-🦀.txt", "e\u{301}.txt"];
    expected.sort_unstable();
    assert_eq!(names, expected);
}

#[test]
fn files_and_directories_are_not_interchangeable() {
    let (_temp, dir) = root();
    dir.create_dir(name("child")).unwrap();
    write_new(&dir, "file", b"");
    assert!(
        dir.open_file(name("child"), OpenOptions::new().read(true))
            .is_err()
    );
    assert!(dir.open_dir(name("file")).is_err());
    assert!(dir.remove_file(name("child")).is_err());
    assert!(dir.remove_dir(name("file")).is_err());
    assert_eq!(
        dir.create_dir(name("child")).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
}

#[test]
fn renames_replace_or_refuse_existing_destinations() {
    let (_temp, dir) = root();
    dir.create_dir(name("other")).unwrap();
    let other = dir.open_dir(name("other")).unwrap();
    write_new(&dir, "source", b"source");
    write_new(&other, "target", b"target");

    let refused = dir.rename(
        name("source"),
        &other,
        name("target"),
        RenameMode::NoReplace,
    );
    assert_eq!(
        refused.unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(read(&other, "target"), b"target");

    dir.rename(name("source"), &other, name("target"), RenameMode::Replace)
        .unwrap();
    assert_eq!(read(&other, "target"), b"source");
    assert_eq!(
        dir.metadata_at(name("source")).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );

    other
        .rename(name("target"), &dir, name("moved"), RenameMode::NoReplace)
        .unwrap();
    assert_eq!(read(&dir, "moved"), b"source");
}

#[test]
fn hard_links_share_identity_and_count() {
    let (_temp, dir) = root();
    write_new(&dir, "original", b"shared");
    dir.hard_link(name("original"), &dir, name("alias"))
        .unwrap();
    let original = dir.metadata_at(name("original")).unwrap();
    let alias = dir.metadata_at(name("alias")).unwrap();
    assert_eq!(original.identity(), alias.identity());
    assert_eq!(original.link_count(), 2);
    assert_eq!(
        dir.hard_link(name("original"), &dir, name("alias"))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    dir.remove_file(name("alias")).unwrap();
    assert_eq!(dir.metadata_at(name("original")).unwrap().link_count(), 1);
}

#[test]
fn handle_metadata_matches_entry_metadata() {
    let (_temp, dir) = root();
    write_new(&dir, "file", b"12345");
    let file = dir
        .open_file(name("file"), OpenOptions::new().read(true))
        .unwrap();
    let from_handle = crate::file_metadata(&file).unwrap();
    let from_entry = dir.metadata_at(name("file")).unwrap();
    assert_eq!(from_handle, from_entry);
    assert_eq!(from_entry.kind(), EntryKind::File);
    assert_eq!(from_entry.len(), 5);
    assert_eq!(dir.metadata().unwrap().kind(), EntryKind::Directory);
    write_new(&dir, "different", b"");
    assert_ne!(
        dir.metadata_at(name("different")).unwrap().identity(),
        from_entry.identity()
    );
}

#[test]
fn removal_requires_empty_directories() {
    let (_temp, dir) = root();
    dir.create_dir(name("parent")).unwrap();
    let parent = dir.open_dir(name("parent")).unwrap();
    write_new(&parent, "file", b"");
    assert!(dir.remove_dir(name("parent")).is_err());
    parent.remove_file(name("file")).unwrap();
    drop(parent);
    dir.remove_dir(name("parent")).unwrap();
    assert_eq!(dir.entries().unwrap().count(), 0);
}

#[test]
fn enumeration_reports_kinds_and_restarts_per_call() {
    let (_temp, dir) = root();
    dir.create_dir(name("directory")).unwrap();
    write_new(&dir, "file", b"");
    let mut entries = dir
        .entries()
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.name().to_owned(), entry.kind())
        })
        .collect::<Vec<_>>();
    entries.sort();
    assert_eq!(
        entries,
        [
            ("directory".into(), EntryKind::Directory),
            ("file".into(), EntryKind::File)
        ]
    );
    // A partially consumed enumeration does not move a later one's position.
    let mut first = dir.entries().unwrap();
    first.next().unwrap().unwrap();
    assert_eq!(dir.entries().unwrap().count(), 2);
}

#[test]
fn enumeration_spans_multiple_system_batches() {
    let (_temp, dir) = root();
    let count = 1_500;
    for index in 0..count {
        write_new(&dir, &format!("entry-with-a-longer-name-{index:05}"), b"");
    }
    let names = dir
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().into_name())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(names.len(), count);
}

#[test]
fn retained_handles_follow_the_directory_not_its_former_path() {
    let (temp, dir) = root();
    dir.create_dir(name("before")).unwrap();
    let pinned = dir.open_dir(name("before")).unwrap();
    dir.rename(name("before"), &dir, name("after"), RenameMode::NoReplace)
        .unwrap();
    write_new(&pinned, "file", b"pinned");
    assert_eq!(
        std::fs::read(temp.path().join("after").join("file")).unwrap(),
        b"pinned"
    );
    assert!(!temp.path().join("before").exists());
}

#[test]
fn strict_opens_require_normalized_absolute_paths() {
    let (temp, _dir) = root();
    std::fs::create_dir(temp.path().join("child")).unwrap();
    let canonical = temp.path().canonicalize().unwrap();
    let strict = Dir::open_strict(&canonical.join("child")).unwrap();
    assert_eq!(
        strict.metadata().unwrap().identity(),
        Dir::open(&temp.path().join("child"))
            .unwrap()
            .metadata()
            .unwrap()
            .identity()
    );
    // Joining onto a Windows verbatim path normalizes `..` away, so build the parent
    // reference from the non-verbatim temporary path.
    for path in [
        std::path::PathBuf::from("relative"),
        temp.path().join("child").join(".."),
    ] {
        assert_eq!(
            Dir::open_strict(&path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput,
            "{path:?}"
        );
    }
}

#[test]
fn volume_space_is_reported() {
    let (_temp, dir) = root();
    assert!(dir.available_bytes().unwrap() > 0);
}

#[test]
fn private_directories_and_their_files_are_private() {
    let (temp, _dir) = root();
    let path = temp.path().join("private");
    crate::create_private_dir(&path).unwrap();
    let private = Dir::open(&path).unwrap();
    assert!(private.is_private().unwrap());
    write_new(&private, "secret", b"value");
    let file = private
        .open_file(name("secret"), OpenOptions::new().read(true))
        .unwrap();
    assert!(crate::is_private(&file).unwrap());
    assert_eq!(
        crate::create_private_dir(&path).unwrap_err().kind(),
        std::io::ErrorKind::AlreadyExists
    );
}

#[test]
fn explicitly_private_files_do_not_depend_on_inheritance() {
    let (temp, dir) = root();
    let file = dir
        .open_file(
            name("secret"),
            OpenOptions::new()
                .write(true)
                .creation(Creation::New)
                .private(true),
        )
        .unwrap();
    assert!(crate::is_private(&file).unwrap());
    let path_file = crate::open_file_path(
        &temp.path().join("path-secret"),
        OpenOptions::new()
            .write(true)
            .creation(Creation::New)
            .private(true),
    )
    .unwrap();
    assert!(crate::is_private(&path_file).unwrap());
}

#[cfg(unix)]
#[test]
fn group_or_world_access_is_not_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let (temp, _dir) = root();
    let path = temp.path().join("shared");
    crate::create_private_dir(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o750)).unwrap();
    assert!(!Dir::open(&path).unwrap().is_private().unwrap());
}

/// Creates a directory link to `target`, or returns false when this host forbids it.
#[cfg(unix)]
fn directory_link(target: &std::path::Path, link: &std::path::Path) -> bool {
    std::os::unix::fs::symlink(target, link).unwrap();
    true
}

#[cfg(windows)]
fn directory_link(target: &std::path::Path, link: &std::path::Path) -> bool {
    // Junctions need no privilege on Windows; unsupported hosts skip link coverage.
    std::process::Command::new("cmd")
        .arg("/D")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(link)
        .arg(target)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
        && std::fs::symlink_metadata(link).is_ok()
}

#[test]
fn directory_links_are_reported_and_never_followed() {
    let (temp, dir) = root();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), b"outside").unwrap();
    if !directory_link(outside.path(), &temp.path().join("escape")) {
        eprintln!("skipping: this host cannot create directory links");
        return;
    }
    assert_eq!(
        dir.metadata_at(name("escape")).unwrap().kind(),
        EntryKind::Symlink
    );
    assert!(dir.open_dir(name("escape")).is_err());
    assert!(Dir::open(&temp.path().join("escape")).is_err());
    // Trusted roots may themselves be links.
    let followed = Dir::open_following_links(&temp.path().join("escape")).unwrap();
    assert_eq!(read(&followed, "secret"), b"outside");
    assert!(
        Dir::open_strict(&temp.path().canonicalize().unwrap().join("escape").join("x")).is_err()
    );
    let kinds = dir
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().kind())
        .collect::<Vec<_>>();
    assert_eq!(kinds, [EntryKind::Symlink]);
    assert!(dir.remove_dir(name("escape")).is_err());
    dir.remove_file(name("escape")).unwrap();
    assert_eq!(
        std::fs::read(outside.path().join("secret")).unwrap(),
        b"outside"
    );
}

#[cfg(unix)]
#[test]
fn file_links_are_never_followed() {
    let (temp, dir) = root();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("secret");
    std::fs::write(&target, b"outside").unwrap();
    std::os::unix::fs::symlink(&target, temp.path().join("link")).unwrap();
    for options in [
        OpenOptions::new().read(true),
        OpenOptions::new().write(true).truncate(true),
        OpenOptions::new().write(true).creation(Creation::IfMissing),
    ] {
        assert!(dir.open_file(name("link"), options).is_err());
    }
    assert_eq!(std::fs::read(&target).unwrap(), b"outside");
}

#[test]
fn path_opens_accept_regular_files_only() {
    let (temp, dir) = root();
    write_new(&dir, "file", b"by path");
    dir.create_dir(name("directory")).unwrap();
    let mut contents = Vec::new();
    crate::open_file_path(&temp.path().join("file"), OpenOptions::new().read(true))
        .unwrap()
        .read_to_end(&mut contents)
        .unwrap();
    assert_eq!(contents, b"by path");
    assert!(
        crate::open_file_path(
            &temp.path().join("directory"),
            OpenOptions::new().read(true)
        )
        .is_err()
    );
    crate::open_file_path(
        &temp.path().join("created"),
        OpenOptions::new().write(true).creation(Creation::New),
    )
    .unwrap();
    assert_eq!(
        crate::open_file_path(
            &temp.path().join("created"),
            OpenOptions::new().write(true).creation(Creation::New),
        )
        .unwrap_err()
        .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    crate::sync_directory_path(temp.path()).unwrap();
}

#[cfg(unix)]
#[test]
fn path_opens_refuse_a_final_link_but_follow_ancestors() {
    let (temp, dir) = root();
    dir.create_dir(name("real")).unwrap();
    write_new(&dir.open_dir(name("real")).unwrap(), "file", b"inside");
    std::os::unix::fs::symlink(temp.path().join("real"), temp.path().join("alias")).unwrap();
    std::os::unix::fs::symlink(
        temp.path().join("real").join("file"),
        temp.path().join("file-link"),
    )
    .unwrap();
    assert!(
        crate::open_file_path(
            &temp.path().join("alias").join("file"),
            OpenOptions::new().read(true)
        )
        .is_ok()
    );
    assert!(
        crate::open_file_path(
            &temp.path().join("file-link"),
            OpenOptions::new().read(true)
        )
        .is_err()
    );
}

#[test]
fn persist_replaces_destinations_held_open_by_readers() {
    let (temp, _dir) = root();
    let destination = temp.path().join("published");
    std::fs::write(&destination, b"old").unwrap();
    // Readers open with std's default delete sharing; replacement must still succeed.
    let reader = std::fs::File::open(&destination).unwrap();
    let mut temporary = tempfile::NamedTempFile::new_in(temp.path()).unwrap();
    temporary.write_all(b"new").unwrap();
    let temporary_path = temporary.path().to_path_buf();
    crate::persist(temporary, &destination).unwrap();
    drop(reader);
    assert_eq!(std::fs::read(&destination).unwrap(), b"new");
    assert!(!temporary_path.exists());
}

#[test]
fn failed_persist_removes_the_temporary() {
    let (temp, _dir) = root();
    let temporary = tempfile::NamedTempFile::new_in(temp.path()).unwrap();
    let temporary_path = temporary.path().to_path_buf();
    assert!(crate::persist(temporary, &temp.path().join("missing").join("file")).is_err());
    assert!(!temporary_path.exists());
}
