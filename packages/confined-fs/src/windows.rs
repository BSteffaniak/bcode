//! `NtCreateFile` backend relative to retained handles.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::{Component, Path, Prefix};

use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FILE_CREATE, FILE_DIRECTORY_FILE, FILE_DISPOSITION_DELETE,
    FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_INFORMATION,
    FILE_DISPOSITION_INFORMATION_EX, FILE_DISPOSITION_POSIX_SEMANTICS, FILE_INFORMATION_CLASS,
    FILE_LINK_INFORMATION, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_IF,
    FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
    FileDispositionInformation, FileDispositionInformationEx, FileFsFullSizeInformation,
    FileLinkInformation, FileRenameInformation, FileRenameInformationEx,
    NTCREATEFILE_CREATE_DISPOSITION, NTCREATEFILE_CREATE_OPTIONS, NtCreateFile,
    NtQueryVolumeInformationFile, NtSetInformationFile,
};
use windows_sys::Wdk::System::SystemServices::FILE_FS_FULL_SIZE_INFORMATION;
use windows_sys::Win32::Foundation::{
    ERROR_CANT_RESOLVE_FILENAME, ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_FILES, ERROR_SUCCESS,
    HANDLE, INVALID_HANDLE_VALUE, LocalFree, NTSTATUS, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError,
    STATUS_INVALID_INFO_CLASS, STATUS_INVALID_PARAMETER, STATUS_NOT_IMPLEMENTED,
    STATUS_NOT_SUPPORTED, UNICODE_STRING,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SDDL_REVISION_1, SE_FILE_OBJECT,
};
use windows_sys::Win32::Security::{
    ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetTokenInformation,
    INHERIT_ONLY_ACE, IsWellKnownSid, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser, WinBuiltinAdministratorsSid,
    WinLocalSystemSid,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CREATE_NEW, CreateDirectoryW, CreateFileW, DELETE,
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_TAG_INFO, FILE_BASIC_INFO, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_ID_BOTH_DIR_INFO,
    FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
    FILE_SHARE_WRITE, FILE_TRAVERSE, FILE_WRITE_ATTRIBUTES, FILE_WRITE_DATA, FileAttributeTagInfo,
    FileBasicInfo, FileIdBothDirectoryInfo, FileIdBothDirectoryRestartInfo, FileIdInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, OPEN_ALWAYS, OPEN_EXISTING,
    READ_CONTROL, SYNCHRONIZE,
};
use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;
use windows_sys::Win32::System::SystemServices::{ACCESS_ALLOWED_ACE_TYPE, ACCESS_DENIED_ACE_TYPE};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::{
    Creation, Entry, EntryKind, FileIdentity, Metadata, OpenOptions, RenameMode, Timestamp,
};

/// `IsReparseTagNameSurrogate`: symbolic links, junctions, and mount points.
const NAME_SURROGATE: u32 = 0x2000_0000;
const RENAME_FLAG_REPLACE_IF_EXISTS: u32 = 0x1;
const RENAME_FLAG_POSIX_SEMANTICS: u32 = 0x2;
const DIRECTORY_ACCESS: u32 = FILE_LIST_DIRECTORY | FILE_TRAVERSE | READ_CONTROL;
const SHARE_ALL: u32 = FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE;
const ENUMERATION_BUFFER_WORDS: usize = 8 * 1024;

pub fn validate_non_utf8_name(name: &OsStr) -> Result<(), ()> {
    let units = name.encode_wide().collect::<Vec<_>>();
    let rejected =
        |unit: u16| [0, u16::from(b'/'), u16::from(b'\\'), u16::from(b':')].contains(&unit);
    let trailing = units
        .last()
        .is_some_and(|&unit| unit == u16::from(b'.') || unit == u16::from(b' '));
    if units.is_empty() || units.iter().copied().any(rejected) || trailing {
        return Err(());
    }
    Ok(())
}

fn size_u32<T>() -> u32 {
    u32::try_from(size_of::<T>()).unwrap_or(u32::MAX)
}

fn win32_error(code: u32) -> io::Error {
    io::Error::from_raw_os_error(code.cast_signed())
}

fn nt_error(status: NTSTATUS) -> io::Error {
    // SAFETY: status conversion has no preconditions.
    win32_error(unsafe { RtlNtStatusToDosError(status) })
}

fn link_error() -> io::Error {
    win32_error(ERROR_CANT_RESOLVE_FILENAME)
}

fn handle(file: &File) -> HANDLE {
    file.as_raw_handle()
}

fn wide(name: &OsStr) -> Vec<u16> {
    name.encode_wide().collect()
}

fn wide_path(path: &Path) -> io::Result<Vec<u16>> {
    let mut units = wide(path.as_os_str());
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains NUL",
        ));
    }
    units.push(0);
    Ok(units)
}

/// Opens `name` relative to `root`; an empty name reopens `root` itself.
fn nt_open(
    root: HANDLE,
    name: &[u16],
    access: u32,
    disposition: NTCREATEFILE_CREATE_DISPOSITION,
    options: NTCREATEFILE_CREATE_OPTIONS,
) -> io::Result<File> {
    nt_create(root, name, access, disposition, options, std::ptr::null())
}

/// Like `nt_open`, applying `security` to an object this call creates.
fn nt_create(
    root: HANDLE,
    name: &[u16],
    access: u32,
    disposition: NTCREATEFILE_CREATE_DISPOSITION,
    options: NTCREATEFILE_CREATE_OPTIONS,
    security: *const c_void,
) -> io::Result<File> {
    let length = u16::try_from(name.len() * 2)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "confined name is too long"))?;
    let object_name = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: name.as_ptr().cast_mut(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_u32::<OBJECT_ATTRIBUTES>(),
        RootDirectory: root,
        ObjectName: &raw const object_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: security.cast(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut opened: HANDLE = std::ptr::null_mut();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: every pointer refers to live, initialized storage for the call's duration.
    let status = unsafe {
        NtCreateFile(
            &raw mut opened,
            access | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
            &raw const attributes,
            &raw mut status_block,
            std::ptr::null(),
            FILE_ATTRIBUTE_NORMAL,
            SHARE_ALL,
            disposition,
            options | FILE_SYNCHRONOUS_IO_NONALERT,
            std::ptr::null(),
            0,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    // SAFETY: NtCreateFile returned a new synchronous handle owned by this File.
    Ok(unsafe { File::from_raw_handle(opened) })
}

#[derive(Clone, Copy)]
struct Attributes {
    attributes: u32,
    tag: u32,
}

impl Attributes {
    const fn is_reparse(self) -> bool {
        self.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }

    const fn is_link(self) -> bool {
        self.is_reparse() && self.tag & NAME_SURROGATE != 0
    }

    const fn is_directory(self) -> bool {
        self.attributes & FILE_ATTRIBUTE_DIRECTORY != 0
    }

    const fn kind(self) -> EntryKind {
        if self.is_link() {
            EntryKind::Symlink
        } else if self.is_directory() {
            EntryKind::Directory
        } else {
            EntryKind::File
        }
    }
}

fn attributes(file: &File) -> io::Result<Attributes> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    // SAFETY: info is writable storage of the size passed.
    if unsafe {
        GetFileInformationByHandleEx(
            handle(file),
            FileAttributeTagInfo,
            (&raw mut info).cast::<c_void>(),
            size_u32::<FILE_ATTRIBUTE_TAG_INFO>(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Attributes {
        attributes: info.FileAttributes,
        tag: info.ReparseTag,
    })
}

/// Opens one entry without following a name-surrogate reparse point. Other reparse
/// points are reopened through the same object so their filter can interpret them.
fn open_resolved(
    root: HANDLE,
    name: &[u16],
    access: u32,
    disposition: NTCREATEFILE_CREATE_DISPOSITION,
    options: NTCREATEFILE_CREATE_OPTIONS,
    security: *const c_void,
) -> io::Result<File> {
    let file = nt_create(
        root,
        name,
        access,
        disposition,
        options | FILE_OPEN_REPARSE_POINT,
        security,
    )?;
    finish_resolved(file, access, options)
}

fn finish_resolved(
    file: File,
    access: u32,
    options: NTCREATEFILE_CREATE_OPTIONS,
) -> io::Result<File> {
    let observed = attributes(&file)?;
    if observed.is_link() {
        return Err(link_error());
    }
    if observed.is_reparse() {
        return nt_open(handle(&file), &[], access, FILE_OPEN, options);
    }
    Ok(file)
}

fn ensure_directory(file: File) -> io::Result<File> {
    if attributes(&file)?.is_directory() {
        Ok(file)
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "confined entry is not a directory",
        ))
    }
}

fn open_root_path(path: &Path) -> io::Result<File> {
    let wide = wide_path(path)?;
    // SAFETY: wide is NUL-terminated; the returned handle is checked before ownership.
    let opened = unsafe {
        CreateFileW(
            wide.as_ptr(),
            DIRECTORY_ACCESS | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if opened == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a new synchronous handle owned by this File.
    let file = unsafe { File::from_raw_handle(opened) };
    ensure_directory(finish_resolved(
        file,
        DIRECTORY_ACCESS,
        FILE_DIRECTORY_FILE,
    )?)
}

pub fn open_dir_path(path: &Path) -> io::Result<File> {
    open_root_path(path)
}

pub fn open_dir_path_following(path: &Path) -> io::Result<File> {
    let wide = wide_path(path)?;
    // SAFETY: wide is NUL-terminated; the returned handle is checked before ownership.
    let opened = unsafe {
        CreateFileW(
            wide.as_ptr(),
            DIRECTORY_ACCESS | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
            SHARE_ALL,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS,
            std::ptr::null_mut(),
        )
    };
    if opened == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a new synchronous handle owned by this File.
    ensure_directory(unsafe { File::from_raw_handle(opened) })
}

pub fn open_dir_path_strict(path: &Path) -> io::Result<File> {
    let mut components = path.components();
    let root = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                format!(r"\\?\{}:\", char::from(letter))
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
                r"\\?\UNC\{}\{}\",
                server.to_string_lossy(),
                share.to_string_lossy()
            ),
            _ => return Err(strict_path_error()),
        },
        _ => return Err(strict_path_error()),
    };
    if components.next() != Some(Component::RootDir) {
        return Err(strict_path_error());
    }
    let mut directory = open_root_path(Path::new(&root))?;
    for component in components {
        let Component::Normal(name) = component else {
            return Err(strict_path_error());
        };
        crate::validate_name(name)?;
        directory = open_dir_at(&directory, name)?;
    }
    Ok(directory)
}

fn strict_path_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "expected an absolute, normalized drive or UNC directory path",
    )
}

pub fn open_dir_at(directory: &File, name: &OsStr) -> io::Result<File> {
    ensure_directory(open_resolved(
        handle(directory),
        &wide(name),
        DIRECTORY_ACCESS,
        FILE_OPEN,
        FILE_DIRECTORY_FILE,
        std::ptr::null(),
    )?)
}

const fn file_access(options: OpenOptions) -> u32 {
    let mut access = READ_CONTROL;
    if options.read {
        access |= FILE_GENERIC_READ;
    }
    if options.write {
        access |= FILE_GENERIC_WRITE;
    } else if options.append {
        access |= FILE_GENERIC_WRITE & !FILE_WRITE_DATA;
    }
    access
}

pub fn open_file_at(directory: &File, name: &OsStr, options: OpenOptions) -> io::Result<File> {
    let disposition = match options.creation {
        Creation::Existing => FILE_OPEN,
        Creation::IfMissing => FILE_OPEN_IF,
        Creation::New => FILE_CREATE,
    };
    let descriptor = options
        .private
        .then(|| private_descriptor(false))
        .transpose()?;
    // Truncation waits until the opened object is verified, so a link or placeholder
    // is never modified through the reparse point itself.
    let file = open_resolved(
        handle(directory),
        &wide(name),
        file_access(options),
        disposition,
        FILE_NON_DIRECTORY_FILE,
        descriptor
            .as_ref()
            .map_or(std::ptr::null(), |descriptor| descriptor.0.cast_const()),
    )?;
    if options.truncate {
        file.set_len(0)?;
    }
    Ok(file)
}

pub fn open_file_path(path: &Path, options: OpenOptions) -> io::Result<File> {
    let access = file_access(options);
    let disposition = match options.creation {
        Creation::Existing => OPEN_EXISTING,
        Creation::IfMissing => OPEN_ALWAYS,
        Creation::New => CREATE_NEW,
    };
    let descriptor = options
        .private
        .then(|| private_descriptor(false))
        .transpose()?;
    let attributes = descriptor.as_ref().map(|descriptor| SECURITY_ATTRIBUTES {
        nLength: size_u32::<SECURITY_ATTRIBUTES>(),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    });
    let wide = wide_path(path)?;
    // SAFETY: wide is NUL-terminated; the returned handle is checked before ownership.
    // Without FILE_FLAG_BACKUP_SEMANTICS, directories cannot be opened.
    let opened = unsafe {
        CreateFileW(
            wide.as_ptr(),
            access | SYNCHRONIZE | FILE_READ_ATTRIBUTES,
            SHARE_ALL,
            attributes
                .as_ref()
                .map_or(std::ptr::null(), std::ptr::from_ref),
            disposition,
            FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if opened == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW returned a new synchronous handle owned by this File.
    let file = finish_resolved(
        unsafe { File::from_raw_handle(opened) },
        access,
        FILE_NON_DIRECTORY_FILE,
    )?;
    if options.truncate {
        file.set_len(0)?;
    }
    Ok(file)
}

#[allow(
    clippy::unnecessary_wraps,
    clippy::missing_const_for_fn,
    reason = "same fallible, non-const interface as Unix directory synchronization"
)]
pub fn sync_dir_path(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub fn file_metadata(file: &File) -> io::Result<Metadata> {
    let observed = attributes(file)?;
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: information is writable storage for the call.
    if unsafe { GetFileInformationByHandle(handle(file), &raw mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut id = FILE_ID_INFO::default();
    // SAFETY: id is writable storage of the size passed.
    let identity = if unsafe {
        GetFileInformationByHandleEx(
            handle(file),
            FileIdInfo,
            (&raw mut id).cast::<c_void>(),
            size_u32::<FILE_ID_INFO>(),
        )
    } == 0
    {
        // File systems without 128-bit IDs report the classic 64-bit index.
        FileIdentity {
            volume: u64::from(information.dwVolumeSerialNumber),
            file: (u128::from(information.nFileIndexHigh) << 32)
                | u128::from(information.nFileIndexLow),
        }
    } else {
        FileIdentity {
            volume: id.VolumeSerialNumber,
            file: u128::from_le_bytes(id.FileId.Identifier),
        }
    };
    let mut basic = FILE_BASIC_INFO::default();
    // SAFETY: basic is writable storage of the size passed.
    if unsafe {
        GetFileInformationByHandleEx(
            handle(file),
            FileBasicInfo,
            (&raw mut basic).cast::<c_void>(),
            size_u32::<FILE_BASIC_INFO>(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(Metadata {
        kind: observed.kind(),
        len: (u64::from(information.nFileSizeHigh) << 32) | u64::from(information.nFileSizeLow),
        link_count: u64::from(information.nNumberOfLinks),
        identity,
        modified: Timestamp(i128::from(basic.LastWriteTime)),
        changed: Timestamp(i128::from(basic.ChangeTime)),
    })
}

pub fn metadata_at(directory: &File, name: &OsStr) -> io::Result<Metadata> {
    let file = nt_open(
        handle(directory),
        &wide(name),
        0,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT,
    )?;
    file_metadata(&file)
}

pub fn create_dir_at(directory: &File, name: &OsStr) -> io::Result<()> {
    nt_open(
        handle(directory),
        &wide(name),
        FILE_LIST_DIRECTORY,
        FILE_CREATE,
        FILE_DIRECTORY_FILE,
    )
    .map(drop)
}

/// Applies one `NtSetInformationFile` request.
fn set_information(
    file: &File,
    class: FILE_INFORMATION_CLASS,
    information: *const c_void,
    length: u32,
) -> Result<(), NTSTATUS> {
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: information points to an initialized request of the given length.
    let status = unsafe {
        NtSetInformationFile(
            handle(file),
            &raw mut status_block,
            information,
            length,
            class,
        )
    };
    if status < 0 { Err(status) } else { Ok(()) }
}

/// Status codes for which an older information class is attempted. Other failures,
/// including existing destinations and sharing violations, are authoritative.
fn is_unsupported_class(status: NTSTATUS) -> bool {
    [
        STATUS_INVALID_INFO_CLASS,
        STATUS_NOT_IMPLEMENTED,
        STATUS_INVALID_PARAMETER,
        STATUS_NOT_SUPPORTED,
    ]
    .contains(&status)
}

fn delete_opened(file: &File) -> io::Result<()> {
    let info = FILE_DISPOSITION_INFORMATION_EX {
        Flags: FILE_DISPOSITION_DELETE
            | FILE_DISPOSITION_POSIX_SEMANTICS
            | FILE_DISPOSITION_IGNORE_READONLY_ATTRIBUTE,
    };
    match set_information(
        file,
        FileDispositionInformationEx,
        (&raw const info).cast::<c_void>(),
        size_u32::<FILE_DISPOSITION_INFORMATION_EX>(),
    ) {
        Ok(()) => return Ok(()),
        Err(status) if !is_unsupported_class(status) => return Err(nt_error(status)),
        Err(_) => {}
    }
    // Older systems and some file systems only support delete-on-last-close.
    let info = FILE_DISPOSITION_INFORMATION { DeleteFile: true };
    set_information(
        file,
        FileDispositionInformation,
        (&raw const info).cast::<c_void>(),
        size_u32::<FILE_DISPOSITION_INFORMATION>(),
    )
    .map_err(nt_error)
}

pub fn remove_file_at(directory: &File, name: &OsStr) -> io::Result<()> {
    let file = nt_open(
        handle(directory),
        &wide(name),
        DELETE,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT,
    )?;
    let observed = attributes(&file)?;
    // Like unlink, a link to a directory is removed itself; a real directory is not.
    if observed.is_directory() && !observed.is_link() {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "confined entry is a directory",
        ));
    }
    delete_opened(&file)
}

pub fn remove_dir_at(directory: &File, name: &OsStr) -> io::Result<()> {
    let file = nt_open(
        handle(directory),
        &wide(name),
        DELETE,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT,
    )?;
    let observed = attributes(&file)?;
    if observed.is_link() || !observed.is_directory() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "confined entry is not a directory",
        ));
    }
    delete_opened(&file)
}

/// Builds a variable-length rename or link request. `FILE_RENAME_INFORMATION` and
/// `FILE_LINK_INFORMATION` share a flags union, a root directory handle, a byte length,
/// and a UTF-16 name interpreted relative to that root.
fn name_information(flags: u32, root: HANDLE, name: &[u16]) -> io::Result<(Vec<u64>, u32)> {
    let header = std::mem::offset_of!(FILE_RENAME_INFORMATION, FileName);
    debug_assert_eq!(
        header,
        std::mem::offset_of!(FILE_LINK_INFORMATION, FileName)
    );
    let too_long = || io::Error::new(io::ErrorKind::InvalidInput, "name is too long");
    let name_bytes = name.len() * 2;
    let total = (header + name_bytes).max(size_of::<FILE_RENAME_INFORMATION>());
    let name_length = u32::try_from(name_bytes).map_err(|_| too_long())?;
    let length = u32::try_from(total).map_err(|_| too_long())?;
    let mut buffer = vec![0_u64; total.div_ceil(8)];
    let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    // SAFETY: the buffer is 8-byte aligned and large enough for the header and name; the
    // name is written through a pointer derived from the whole allocation.
    unsafe {
        (*info).Anonymous.Flags = flags;
        (*info).RootDirectory = root;
        (*info).FileNameLength = name_length;
        std::ptr::copy_nonoverlapping(
            name.as_ptr(),
            (&raw mut (*info).FileName).cast::<u16>(),
            name.len(),
        );
    }
    Ok((buffer, length))
}

pub fn rename_at(
    from_directory: &File,
    from: &OsStr,
    to_directory: &File,
    to: &OsStr,
    mode: RenameMode,
) -> io::Result<()> {
    let source = nt_open(
        handle(from_directory),
        &wide(from),
        DELETE,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT,
    )?;
    let target = wide(to);
    let replace = mode == RenameMode::Replace;
    let flags = RENAME_FLAG_POSIX_SEMANTICS
        | if replace {
            RENAME_FLAG_REPLACE_IF_EXISTS
        } else {
            0
        };
    let (buffer, length) = name_information(flags, handle(to_directory), &target)?;
    match set_information(
        &source,
        FileRenameInformationEx,
        buffer.as_ptr().cast::<c_void>(),
        length,
    ) {
        Ok(()) => return Ok(()),
        Err(status) if !is_unsupported_class(status) => return Err(nt_error(status)),
        Err(_) => {}
    }
    // FILE_RENAME_INFORMATION's leading union member is a one-byte ReplaceIfExists flag.
    let (buffer, length) = name_information(u32::from(replace), handle(to_directory), &target)?;
    set_information(
        &source,
        FileRenameInformation,
        buffer.as_ptr().cast::<c_void>(),
        length,
    )
    .map_err(nt_error)
}

pub fn hard_link_at(
    from_directory: &File,
    from: &OsStr,
    to_directory: &File,
    to: &OsStr,
) -> io::Result<()> {
    let source = nt_open(
        handle(from_directory),
        &wide(from),
        FILE_WRITE_ATTRIBUTES,
        FILE_OPEN,
        FILE_OPEN_REPARSE_POINT | FILE_NON_DIRECTORY_FILE,
    )?;
    let (buffer, length) = name_information(0, handle(to_directory), &wide(to))?;
    set_information(
        &source,
        FileLinkInformation,
        buffer.as_ptr().cast::<c_void>(),
        length,
    )
    .map_err(nt_error)
}

/// Enumerates over a private handle so the retained directory's position is untouched.
pub struct Entries {
    directory: File,
    buffer: Vec<u64>,
    pending: VecDeque<Entry>,
    restart: bool,
    finished: bool,
}

pub fn entries(directory: &File) -> io::Result<Entries> {
    Ok(Entries {
        directory: nt_open(
            handle(directory),
            &[],
            FILE_LIST_DIRECTORY,
            FILE_OPEN,
            FILE_DIRECTORY_FILE,
        )?,
        buffer: vec![0; ENUMERATION_BUFFER_WORDS],
        pending: VecDeque::new(),
        restart: true,
        finished: false,
    })
}

impl Entries {
    fn refill(&mut self) -> io::Result<()> {
        let class = if std::mem::take(&mut self.restart) {
            FileIdBothDirectoryRestartInfo
        } else {
            FileIdBothDirectoryInfo
        };
        let bytes = u32::try_from(self.buffer.len() * 8).unwrap_or(u32::MAX);
        // SAFETY: the buffer is writable, 8-byte aligned storage of the size passed.
        if unsafe {
            GetFileInformationByHandleEx(
                handle(&self.directory),
                class,
                self.buffer.as_mut_ptr().cast::<c_void>(),
                bytes,
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES.cast_signed()) {
                self.finished = true;
                return Ok(());
            }
            return Err(error);
        }
        let capacity = self.buffer.len() * 8;
        let name_offset = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        let mut offset = 0_usize;
        loop {
            let malformed = || io::Error::other("malformed directory enumeration record");
            if !offset.is_multiple_of(8) || offset + name_offset > capacity {
                return Err(malformed());
            }
            // SAFETY: the record header lies within the aligned buffer, checked above.
            let record =
                unsafe { self.buffer.as_ptr().byte_add(offset) }.cast::<FILE_ID_BOTH_DIR_INFO>();
            // SAFETY: record points to an initialized header written by the system.
            let length = unsafe { (*record).FileNameLength } as usize / 2;
            if offset + name_offset + length * 2 > capacity {
                return Err(malformed());
            }
            // SAFETY: the record's name lies within the buffer, checked above, and is
            // read through a pointer derived from the whole allocation.
            let (next, attributes, tag, name) = unsafe {
                let name = std::slice::from_raw_parts(
                    (&raw const (*record).FileName).cast::<u16>(),
                    length,
                );
                (
                    (*record).NextEntryOffset as usize,
                    (*record).FileAttributes,
                    (*record).EaSize,
                    OsString::from_wide(name),
                )
            };
            if name != "." && name != ".." {
                // For reparse points, EaSize carries the reparse tag.
                let kind = Attributes { attributes, tag }.kind();
                self.pending.push_back(Entry { name, kind });
            }
            if next == 0 {
                return Ok(());
            }
            offset += next;
        }
    }
}

impl Iterator for Entries {
    type Item = io::Result<Entry>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.pending.is_empty() && !self.finished {
            if let Err(error) = self.refill() {
                self.finished = true;
                return Some(Err(error));
            }
        }
        self.pending.pop_front().map(Ok)
    }
}

#[allow(
    clippy::unnecessary_wraps,
    clippy::missing_const_for_fn,
    reason = "same fallible, non-const interface as Unix directory synchronization"
)]
pub fn sync_dir(_directory: &File) -> io::Result<()> {
    Ok(())
}

pub fn available_bytes(directory: &File) -> io::Result<u64> {
    let mut info = FILE_FS_FULL_SIZE_INFORMATION::default();
    let mut status_block = IO_STATUS_BLOCK::default();
    // SAFETY: info is writable storage of the size passed.
    let status = unsafe {
        NtQueryVolumeInformationFile(
            handle(directory),
            &raw mut status_block,
            (&raw mut info).cast::<c_void>(),
            size_u32::<FILE_FS_FULL_SIZE_INFORMATION>(),
            FileFsFullSizeInformation,
        )
    };
    if status < 0 {
        return Err(nt_error(status));
    }
    let invalid = || io::Error::other("invalid available storage measurement");
    let units = u64::try_from(info.CallerAvailableAllocationUnits).map_err(|_| invalid())?;
    if info.SectorsPerAllocationUnit == 0 || info.BytesPerSector == 0 {
        return Err(io::Error::other("unknown filesystem allocation unit"));
    }
    units
        .checked_mul(u64::from(info.SectorsPerAllocationUnit))
        .and_then(|sectors| sectors.checked_mul(u64::from(info.BytesPerSector)))
        .ok_or_else(invalid)
}

/// Memory returned by security APIs that must be released with `LocalFree`.
struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer was allocated by a LocalAlloc-based API and is freed once.
            unsafe { LocalFree(self.0) };
        }
    }
}

/// The current process token's user, retained in an aligned buffer.
struct CurrentUser(Vec<u64>);

impl CurrentUser {
    fn query() -> io::Result<Self> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: the pseudo-handle is always valid and token is writable storage.
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: OpenProcessToken returned a new handle owned here.
        let token = unsafe { OwnedHandle::from_raw_handle(token) };
        let mut length = 0_u32;
        // SAFETY: a null buffer with zero length only queries the required size.
        unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                std::ptr::null_mut(),
                0,
                &raw mut length,
            )
        };
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER.cast_signed()) {
            return Err(error);
        }
        let mut buffer = vec![0_u64; (length as usize).div_ceil(8)];
        // SAFETY: buffer is writable, aligned storage of at least the reported length.
        if unsafe {
            GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast::<c_void>(),
                length,
                &raw mut length,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(buffer))
    }

    const fn sid(&self) -> PSID {
        // SAFETY: the buffer holds a TOKEN_USER whose SID points into the same buffer.
        unsafe { (*self.0.as_ptr().cast::<TOKEN_USER>()).User.Sid }
    }
}

pub fn is_private(file: &File) -> io::Result<bool> {
    let mut owner: PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: every out-pointer is writable; the descriptor owns owner and dacl.
    let status = unsafe {
        GetSecurityInfo(
            handle(file),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut descriptor,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(win32_error(status));
    }
    let _descriptor = LocalAllocation(descriptor);
    let user = CurrentUser::query()?;
    // SAFETY: owner and the user SID are valid SIDs for the duration of the call.
    if owner.is_null() || unsafe { EqualSid(owner, user.sid()) } == 0 {
        return Ok(false);
    }
    // A NULL DACL grants everyone full access.
    if dacl.is_null() {
        return Ok(false);
    }
    // SAFETY: dacl points into the retained security descriptor.
    let count = unsafe { (*dacl).AceCount };
    for index in 0..count {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: index is within the ACL's ACE count.
        if unsafe { GetAce(dacl, u32::from(index), &raw mut ace) } == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: GetAce returned a pointer to a complete ACE within the ACL.
        let header = unsafe { *ace.cast::<ACE_HEADER>() };
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 {
            continue;
        }
        match u32::from(header.AceType) {
            ACCESS_DENIED_ACE_TYPE => {}
            ACCESS_ALLOWED_ACE_TYPE => {
                // SAFETY: an ACCESS_ALLOWED_ACE stores its SID after the header and mask.
                let sid: PSID =
                    unsafe { ace.cast::<u8>().add(size_of::<ACE_HEADER>() + 4) }.cast::<c_void>();
                // SAFETY: sid and the user SID are valid for these comparisons.
                let trusted = unsafe {
                    EqualSid(sid, user.sid()) != 0
                        || IsWellKnownSid(sid, WinLocalSystemSid) != 0
                        || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0
                };
                if !trusted {
                    return Ok(false);
                }
            }
            // Conditional, object, and callback grants cannot be proven private.
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// A protected descriptor granting full control only to the current user, SYSTEM, and
/// Administrators. Directory grants are inherited by children created inside them.
fn private_descriptor(inheritable: bool) -> io::Result<LocalAllocation> {
    let user = CurrentUser::query()?;
    let mut sid_text: *mut u16 = std::ptr::null_mut();
    // SAFETY: the user SID is valid and sid_text is writable storage.
    if unsafe { ConvertSidToStringSidW(user.sid(), &raw mut sid_text) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let sid_allocation = LocalAllocation(sid_text.cast::<c_void>());
    // SAFETY: ConvertSidToStringSidW returned a NUL-terminated string.
    let sid = unsafe {
        let mut length = 0;
        while *sid_text.add(length) != 0 {
            length += 1;
        }
        OsString::from_wide(std::slice::from_raw_parts(sid_text, length))
    };
    drop(sid_allocation);
    let sid = sid.to_string_lossy();
    let flags = if inheritable { "OICI" } else { "" };
    let sddl = format!("O:{sid}D:P(A;{flags};FA;;;{sid})(A;{flags};FA;;;SY)(A;{flags};FA;;;BA)");
    let sddl = OsStr::new(&sddl)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: sddl is NUL-terminated and descriptor is writable storage.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &raw mut descriptor,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(LocalAllocation(descriptor))
}

pub fn create_private_dir(path: &Path) -> io::Result<()> {
    let descriptor = private_descriptor(true)?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_u32::<SECURITY_ATTRIBUTES>(),
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    let path = wide_path(path)?;
    // SAFETY: path is NUL-terminated and attributes reference a live descriptor.
    if unsafe { CreateDirectoryW(path.as_ptr(), &raw const attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
