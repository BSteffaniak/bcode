# Windows support

Bcode builds and runs natively on x86-64 Windows (`x86_64-pc-windows-msvc`). The daemon,
clients, and bundled plugins share the same application boundaries as on Unix; platform
differences are confined to the adapters described here.

## Platform status

Available natively:

* Daemon startup, artifact identity, and IPC over per-user named pipes.
* Sessions, model turns, history, catalog discovery, access tracking, and working documents.
* Filesystem read, write, edit, multi-edit, and path search, including Git-index scope.
* Shell commands through `cmd /C`, in pipe and terminal (ConPTY) modes.
* Provider authentication, including OAuth refresh coordination, credential custody,
  external credential discovery, and the auth broker's private lease files.
* Configuration edits and remembered startup choices.

Unavailable on Windows; these fail closed with an explicit status:

* Session storage tiering: artifact compression, storage admission, and legacy admission
  compaction.
* Xray credential files and retained trace export.
* TUI session resurrection over BMUX.
* The Vox voice service, which targets Linux desktop sessions.

## Confined filesystem access

`bcode_confined_fs` provides handle-relative, no-follow access used wherever untrusted names
are resolved beneath an authorized root. A `Dir` retains an open directory handle and resolves
one component at a time relative to it, so substituting an ancestor cannot redirect an
operation. Unix uses the `openat` family with `O_NOFOLLOW`; Windows uses `NtCreateFile`
relative to the retained handle with `FILE_OPEN_REPARSE_POINT`.

Windows semantics:

* **Links.** Only name-surrogate reparse points (symbolic links, junctions, and mount points)
  are links and are refused. Other reparse points, such as cloud-file placeholders, are
  reopened through the same object so their filter can interpret them.
* **Names.** Components containing `\`, `:` (alternate data streams), or a trailing dot or space
  are rejected; Win32 path normalization would make such names unreachable.
* **Privacy.** "Private" means owned by the current user with a DACL granting access only to
  that user, SYSTEM, and Administrators. Private directories carry a protected inheritable DACL,
  so files created inside inherit it; explicitly private files receive their own descriptor.
  Other created files inherit their directory's access control, preserving user ACLs.
* **Replacement and deletion.** Renames and deletions request POSIX semantics, so a file
  another process holds open with delete sharing can still be replaced or removed. Volumes
  without POSIX semantics (FAT, exFAT, and many network shares) fall back to classic
  semantics, where replacing an open file fails. Multi-edit releases its snapshot handle
  immediately after its final identity check so publication works on those volumes too.
* **Identity.** Identities use the volume serial number and the 128-bit file ID, so `ReFS`
  (including Dev Drive) is supported.
* **Durability.** Directory synchronization is a no-op because NTFS and `ReFS` journal entry
  metadata.

`bcode_confined_fs::persist` replaces `tempfile::NamedTempFile::persist` for atomic
publication. tempfile uses `MoveFileExW` alone, which fails while a reader holds the
destination open; `persist` falls back to POSIX replacement semantics.

## Paths at process boundaries

Canonical paths on Windows carry the verbatim `\\?\` prefix. Bcode keeps that form internally
so confinement comparisons stay consistent, and removes it only where a path leaves the
process: `cmd.exe` rejects verbatim current directories and silently falls back to the Windows
directory, and native libraries such as Tesseract do not normalize `/` inside verbatim paths.

Workspace-relative paths in path search results and resource grants use `/` on every platform.
They are joined onto absolute roots one component at a time, because `/` is not a separator
inside verbatim paths.

## Shell commands

The shell tool runs `cmd /C`, and the turn environment reports `%COMSPEC%` as the default
shell even when Bcode is launched from a POSIX-like shell. ConPTY is created with
`PSEUDOCONSOLE_INHERIT_CURSOR` and blocks its child until the terminal answers a leading
cursor-position query; the shell plugin answers that query with the origin of the new
terminal and omits it from captured output.

## Validation

Native Windows CI runs per-package tests for packages whose `clippier.toml` lists Windows,
plus the Windows contract and release-artifact checks described in [CI coverage](ci.md).

For local cross-checking from Linux, `cargo xwin check --target x86_64-pc-windows-msvc`
compiles the workspace against the MSVC SDK. Wine can run many of the resulting test binaries
but does not implement several behaviors Bcode relies on, so a Wine failure in these areas is
not evidence of a Windows defect:

* ACL inheritance on file creation, so inherited privacy checks fail.
* POSIX rename semantics, so replacing a file held open fails.
* ConPTY output, so terminal-mode shell commands produce no output.
* A `git` executable, so Git-index tests cannot create fixtures.
