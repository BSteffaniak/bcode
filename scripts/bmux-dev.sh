#!/usr/bin/env bash
set -euo pipefail

# Toggle a local BMUX checkout for Bcode development.
#
# `on` rewrites only the bmux_* entries of the workspace `[patch.crates-io]` table to local path
# patches (each marked with a trailing `# bmux-dev:` comment) and records the original lines in
# git-ignored state under `.cargo/`. `off` puts the recorded lines back without touching any other
# manifest edits and restores Cargo.lock. `cargo xtask bmux-lock-check` (also part of
# `cargo xtask architecture`, which CI runs) rejects both path patches and path-patched locks.
#
# Why not an untracked Cargo config `[patch]`: config patches do take precedence over manifest
# patches, but Cargo discovers config from the working directory of every invocation, including
# nested builds under `target/`. `bmux_mermaid_render`'s build script runs a `--locked` guest
# build from its OUT_DIR, sees the patches as unused, needs a lockfile update, and fails.
# Manifest patches apply only to the Bcode workspace itself.
#
# Compatible with macOS bash 3.2 and BSD userland: no associative arrays, mapfile or GNU flags.

usage() {
    cat <<'EOF'
Usage: scripts/bmux-dev.sh <command> [options]

Commands:
  on [path]        Patch every bmux_* crate Bcode patches to the BMUX checkout at <path>
                   (default: $BMUX_DEV_PATH or ~/GitHub/wt-bmux/robust-themes).
  off [--force]    Remove the override and restore Cargo.lock. Refuses when Cargo.lock may hold
                   unrelated uncommitted changes unless --force (which restores it from HEAD).
  status           Show override state, BMUX path and HEAD, and whether Cargo.lock is patched.
  update [--force] After pushing BMUX: run `off`, then `cargo update -p bmux_tui`, and report the
                   newly locked BMUX revision.
EOF
}

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
cd "${root}"

manifest_file="${root}/Cargo.toml"
orig_file="${root}/.cargo/bmux-dev.manifest-lines"
state_file="${root}/.cargo/bmux-dev.state"
snapshot_file="${root}/.cargo/bmux-dev.Cargo.lock.orig"
discarded_file="${root}/.cargo/bmux-dev.Cargo.lock.discarded"
lock_file="${root}/Cargo.lock"
default_bmux="${BMUX_DEV_PATH:-${HOME}/GitHub/wt-bmux/robust-themes}"
marker="bmux-dev: local override; restore with scripts/bmux-dev.sh off"

die() {
    printf 'bmux-dev: error: %s\n' "$*" >&2
    exit 1
}

info() {
    printf 'bmux-dev: %s\n' "$*"
}

sha256_stdin() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | awk '{print $1}'
    else
        shasum -a 256 | awk '{print $1}'
    fi
}

state_get() {
    [[ -f "${state_file}" ]] || return 0
    awk -v key="$1" 'index($0, key "=") == 1 { print substr($0, length(key) + 2); exit }' \
        "${state_file}"
}

# Fingerprint of uncommitted manifest edits. A change between `on` and `off` means a dependency
# edit may be recorded in Cargo.lock, so `off` refuses to restore the lock.
manifest_fingerprint() {
    {
        git -C "${root}" diff HEAD -- '*Cargo.toml'
        git -C "${root}" ls-files --others --exclude-standard -- '*Cargo.toml' |
            while IFS= read -r manifest; do
                printf '%s\n' "${manifest}"
                cat "${root}/${manifest}"
            done
    } | sha256_stdin
}

# Fingerprint of Cargo.lock without its bmux_* package entries. The override only rewrites those
# entries, so a change here between `on` (after re-resolution) and `off` comes from something else
# (for example `cargo update -p <crate>`, or a dependency the local BMUX checkout added) and must
# not be discarded silently.
lock_fingerprint_without_bmux() {
    awk '
        /^\[\[package\]\]/ { flush(); block = $0 "\n"; in_pkg = 1; skip = 0; next }
        in_pkg && /^name = "bmux_/ { skip = 1 }
        in_pkg { block = block $0 "\n"; next }
        { print }
        function flush() { if (in_pkg && !skip) printf "%s", block; block = "" }
        END { flush() }
    ' "${1:-${lock_file}}" | sha256_stdin
}

# The fingerprint recorded after `on` resolved the lock; if `on` failed before recording it, the
# pre-`on` snapshot's fingerprint (resolution did not complete, so nothing else was expected).
lock_fp_on() {
    local recorded
    recorded="$(state_get lock_fp_on)"
    if [[ -n "${recorded}" ]]; then
        printf '%s' "${recorded}"
    elif [[ -f "${snapshot_file}" ]]; then
        lock_fingerprint_without_bmux "${snapshot_file}"
    fi
}

lock_matches_head() {
    git -C "${root}" diff --quiet HEAD -- Cargo.lock
}

lock_staged() {
    ! git -C "${root}" diff --cached --quiet -- Cargo.lock
}

# Prints `<name> <source>` for every locked bmux_* package; `-` marks a path (sourceless) entry.
lock_bmux_sources() {
    awk '
        function flush() { if (name ~ /^bmux_/) print name, src }
        /^\[\[package\]\]/ { flush(); name = ""; src = "-"; next }
        /^name = / { name = $3; gsub(/"/, "", name) }
        /^source = / { src = $3; gsub(/"/, "", src) }
        END { flush() }
    ' "${lock_file}"
}

lock_path_patched_count() {
    lock_bmux_sources | awk '$2 == "-" { n++ } END { print n + 0 }'
}

locked_bmux_tui_rev() {
    lock_bmux_sources | awk '$1 == "bmux_tui" { n = split($2, part, "#"); if (n > 1) print part[n]; exit }'
}

# True when every changed Cargo.lock line is a removed or added BMUX git `source` line.
lock_diff_is_only_bmux_sources() {
    git -C "${root}" diff --unified=0 HEAD -- Cargo.lock |
        awk '
            /^(\+\+\+|---) / { next }
            /^[+-]/ {
                if ($0 !~ /^[+-]source = "git\+https:\/\/github\.com\/BSteffaniak\/bmux[?#]/) bad = 1
            }
            END { exit bad }
        '
}

# Crate names patched under [patch.crates-io] in the workspace manifest.
patched_bmux_crates() {
    awk '
        /^[[:space:]]*\[/ { in_patch = ($0 ~ /^[[:space:]]*\[patch\.crates-io\][[:space:]]*$/); next }
        in_patch && /^[[:space:]]*bmux_[A-Za-z0-9_-]*[[:space:]]*=/ {
            sub(/^[[:space:]]*/, ""); sub(/[[:space:]]*=.*/, ""); print
        }
    ' "${root}/Cargo.toml"
}

# The `[package] name` of one manifest, or nothing for virtual/unparseable manifests.
package_name() {
    awk '
        /^[[:space:]]*\[/ { in_pkg = ($0 ~ /^[[:space:]]*\[package\][[:space:]]*(#.*)?$/); next }
        in_pkg && /^[[:space:]]*name[[:space:]]*=/ {
            sub(/^[^=]*=[[:space:]]*/, "")
            if ($0 ~ /^"/) { sub(/^"/, ""); sub(/".*/, "") }
            else if ($0 ~ /^\047/) { sub(/^\047/, ""); sub(/\047.*/, "") }
            else next
            print
            exit
        }
    ' "$1"
}

bmux_manifests() {
    local bmux="$1"
    if git -C "${bmux}" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        git -C "${bmux}" ls-files --cached --others --exclude-standard -- '*Cargo.toml'
    else
        (cd "${bmux}" && find . \( -name target -o -name node_modules -o -name .git \) -prune \
            -o -name Cargo.toml -type f -print | sed 's|^\./||')
    fi
}

# Prints `<crate> <absolute dir>` for every patched crate, failing on missing or duplicate names.
resolve_crate_dirs() {
    local bmux="$1" index crates crate matches count
    index="$(
        bmux_manifests "${bmux}" | while IFS= read -r manifest; do
            [[ -f "${bmux}/${manifest}" ]] || continue
            name="$(package_name "${bmux}/${manifest}")"
            [[ "${name}" == bmux_* ]] || continue
            dir="$(dirname "${bmux}/${manifest}")"
            printf '%s %s\n' "${name}" "$(cd "${dir}" && pwd -P)"
        done
    )"
    crates="$(patched_bmux_crates)"
    [[ -n "${crates}" ]] || die "Cargo.toml [patch.crates-io] declares no bmux_* crates"
    for crate in ${crates}; do
        matches="$(printf '%s\n' "${index}" | awk -v c="${crate}" '$1 == c { print substr($0, length(c) + 2) }')"
        count="$(printf '%s' "${matches}" | awk 'END { print NR }')"
        if [[ -z "${matches}" ]]; then
            die "BMUX checkout ${bmux} has no package named ${crate}"
        elif [[ "${count}" -ne 1 ]]; then
            die "BMUX checkout ${bmux} has ${count} packages named ${crate}: $(printf '%s' "${matches}" | tr '\n' ' ')"
        fi
        printf '%s %s\n' "${crate}" "${matches}"
    done
}

bmux_head() {
    local bmux="$1" head dirty=""
    head="$(git -C "${bmux}" log -1 --format='%H %s' 2>/dev/null)" || {
        printf 'unknown (not a git checkout)'
        return
    }
    if [[ -n "$(git -C "${bmux}" status --porcelain --untracked-files=no 2>/dev/null)" ]]; then
        dirty=" [uncommitted changes]"
    fi
    printf '%s%s' "${head}" "${dirty}"
}

# Re-resolves Cargo.lock under the current Cargo configuration, quietly unless it fails.
resolve_lock() {
    local log
    log="$(mktemp "${TMPDIR:-/tmp}/bmux-dev-cargo.XXXXXX")"
    if ! cargo metadata --format-version 1 >/dev/null 2>"${log}"; then
        cat "${log}" >&2
        rm -f "${log}"
        return 1
    fi
    rm -f "${log}"
}

# Prints `<crate>\t<line>` for each bmux_* entry in a manifest's [patch.crates-io] table.
patch_lines() {
    awk '
        /^[[:space:]]*\[/ { in_patch = ($0 ~ /^[[:space:]]*\[patch\.crates-io\][[:space:]]*$/) }
        in_patch && match($0, /^[[:space:]]*bmux_[A-Za-z0-9_-]*/) {
            name = substr($0, RSTART, RLENGTH); gsub(/[[:space:]]/, "", name)
            print name "\t" $0
        }
    '
}

override_count() {
    patch_lines <"${manifest_file}" | awk -v m="# ${marker}" 'index($0, m) { n++ } END { print n + 0 }'
}

override_path() {
    patch_lines <"${manifest_file}" | awk -v m="# ${marker}" -F"'" 'index($0, m) { print $2; exit }' |
        sed 's|/packages/[^/]*$||'
}

# Rewrites Cargo.toml in place (keeping its inode and mode) through an awk program whose first
# input is the given `<crate>\t<replacement line>` file.
rewrite_manifest() {
    local replacements="$1" tmp
    tmp="$(mktemp "${TMPDIR:-/tmp}/bmux-dev-manifest.XXXXXX")"
    awk -F'\t' -v only_marked="$2" -v m="# ${marker}" '
        NR == FNR { line[$1] = substr($0, length($1) + 2); next }
        /^[[:space:]]*\[/ { in_patch = ($0 ~ /^[[:space:]]*\[patch\.crates-io\][[:space:]]*$/) }
        in_patch && match($0, /^[[:space:]]*bmux_[A-Za-z0-9_-]*/) {
            name = substr($0, RSTART, RLENGTH); gsub(/[[:space:]]/, "", name)
            if ((name in line) && (only_marked == "0" || index($0, m))) { print line[name]; next }
        }
        { print }
    ' "${replacements}" "${manifest_file}" >"${tmp}"
    cat "${tmp}" >"${manifest_file}"
    rm -f "${tmp}"
}

cmd_on() {
    local requested="${1:-${default_bmux}}" bmux dirs unpatched replacements
    [[ -d "${requested}" ]] || die "BMUX path does not exist: ${requested}"
    bmux="$(cd "${requested}" && pwd -P)"
    case "${bmux}" in
        *"'"* | *$'\n'* | *$'\t'*) die "BMUX path must not contain quotes, tabs or newlines: ${bmux}" ;;
    esac
    [[ "${bmux}" != "${root}" ]] || die "BMUX path is the Bcode checkout itself"

    dirs="$(resolve_crate_dirs "${bmux}")"

    if [[ -f "${state_file}" ]]; then
        info "override already on ($(state_get path)); regenerating for ${bmux}"
    else
        [[ "$(override_count)" -eq 0 ]] ||
            die "Cargo.toml has bmux-dev path patches but no state; run \`scripts/bmux-dev.sh off\` first"
        [[ "$(lock_path_patched_count)" -eq 0 ]] ||
            die "Cargo.lock is already path-patched without an override record; run \`scripts/bmux-dev.sh off\` first"
        ! lock_staged || die "Cargo.lock has staged changes; commit or unstage them before enabling the override"
        patch_lines <"${manifest_file}" >"${orig_file}"
        cp "${lock_file}" "${snapshot_file}"
        {
            if lock_matches_head; then printf 'lock_was_clean=1\n'; else printf 'lock_was_clean=0\n'; fi
            printf 'manifest_fp=%s\n' "$(manifest_fingerprint)"
            printf 'path=%s\n' "${bmux}"
        } >"${state_file}"
    fi
    awk -v path="${bmux}" 'index($0, "path=") == 1 { print "path=" path; next } { print }' \
        "${state_file}" >"${state_file}.tmp"
    mv "${state_file}.tmp" "${state_file}"

    replacements="$(mktemp "${TMPDIR:-/tmp}/bmux-dev-lines.XXXXXX")"
    printf '%s\n' "${dirs}" | awk -v m="# ${marker}" '{
        dir = substr($0, length($1) + 2)
        printf "%s\t%s = { path = \047%s\047 } %s\n", $1, $1, dir, m
    }' >"${replacements}"
    rewrite_manifest "${replacements}" 0
    rm -f "${replacements}"

    info "patched $(override_count) bmux_* entries in Cargo.toml [patch.crates-io] -> ${bmux}"
    resolve_lock || die "cargo could not resolve the workspace against ${bmux}; run \`scripts/bmux-dev.sh off\` to undo"
    # Recorded once, after the first resolution, so later lock edits are detected by `off`.
    [[ -n "$(state_get lock_fp_on)" ]] ||
        printf 'lock_fp_on=%s\n' "$(lock_fingerprint_without_bmux)" >>"${state_file}"

    unpatched="$(lock_bmux_sources | awk '$2 != "-" { print $1 }' | tr '\n' ' ')"
    [[ -z "${unpatched}" ]] || die "Cargo.lock still resolves these from git (version mismatch?): ${unpatched}"
    info "Cargo.lock now resolves $(lock_path_patched_count) bmux_* packages from ${bmux}"
    info "BMUX HEAD: $(bmux_head "${bmux}")"
    info "run \`scripts/bmux-dev.sh off\` before committing"
}

# Restores the recorded original patch lines (falling back to HEAD's) for marked entries only.
restore_manifest() {
    local originals
    [[ "$(override_count)" -gt 0 ]] || return 0
    originals="$(mktemp "${TMPDIR:-/tmp}/bmux-dev-orig.XXXXXX")"
    # Recorded lines win over HEAD's because they keep uncommitted pre-`on` patch edits.
    {
        git -C "${root}" show HEAD:Cargo.toml | patch_lines
        [[ ! -f "${orig_file}" ]] || cat "${orig_file}"
    } >"${originals}"
    rewrite_manifest "${originals}" 1
    rm -f "${originals}"
    [[ "$(override_count)" -eq 0 ]] ||
        die "could not restore every bmux-dev entry in Cargo.toml; fix the remaining \`# bmux-dev:\` lines by hand"
    info "restored the bmux_* git patches in Cargo.toml"
}

cmd_off() {
    local force=0 was_clean recorded_fp current_fp reason="" restore="head" patched
    while [[ $# -gt 0 ]]; do
        case "$1" in
            --force) force=1 ;;
            *) die "unknown option for off: $1" ;;
        esac
        shift
    done

    if [[ "$(override_count)" -eq 0 && ! -f "${state_file}" ]] && lock_matches_head && ! lock_staged; then
        info "override already off; Cargo.lock matches HEAD"
        return 0
    fi

    # Removing the override is always safe; only the lockfile decision below can refuse.
    restore_manifest

    if ! lock_matches_head || lock_staged; then
        if lock_staged; then
            reason="Cargo.lock has staged changes"
        elif [[ -f "${state_file}" ]]; then
            was_clean="$(state_get lock_was_clean)"
            recorded_fp="$(state_get manifest_fp)"
            current_fp="$(manifest_fingerprint)"
            if [[ "${recorded_fp}" != "${current_fp}" ]]; then
                reason="Cargo.toml manifests changed while the override was on"
            elif [[ "$(lock_fp_on)" != "$(lock_fingerprint_without_bmux)" ]]; then
                reason="Cargo.lock changed outside bmux_* packages while the override was on (for example \`cargo update\`, or a dependency the local BMUX checkout added)"
            elif [[ "${was_clean}" != "1" ]]; then
                # Pre-existing lock edits are preserved by restoring the pre-`on` snapshot.
                restore="snapshot"
                [[ -f "${snapshot_file}" ]] || reason="Cargo.lock had uncommitted changes before \`on\` and its snapshot is missing"
            fi
        elif ! lock_diff_is_only_bmux_sources; then
            reason="no override record exists and Cargo.lock differs from HEAD beyond bmux_* sources"
        fi
    fi

    if [[ -n "${reason}" ]]; then
        if [[ "${force}" -ne 1 ]]; then
            [[ ! -f "${snapshot_file}" ]] || info "the pre-\`on\` Cargo.lock is saved at ${snapshot_file#"${root}"/}"
            die "Cargo.toml override removed, but refusing to restore Cargo.lock: ${reason}. Inspect \`git diff Cargo.lock\`; rerun \`off --force\` to restore it from HEAD"
        fi
        cp "${lock_file}" "${discarded_file}"
        info "--force: ${reason}; restoring Cargo.lock from HEAD (the discarded lock is saved at ${discarded_file#"${root}"/})"
        restore="head"
    fi

    if [[ "${restore}" == "snapshot" ]]; then
        cp "${snapshot_file}" "${lock_file}"
        info "restored Cargo.lock from the pre-\`on\` snapshot (keeps its earlier uncommitted changes)"
    elif ! lock_matches_head || lock_staged; then
        git -C "${root}" restore --source=HEAD --staged --worktree -- Cargo.lock
        info "restored Cargo.lock from HEAD"
    fi
    rm -f "${state_file}" "${snapshot_file}" "${orig_file}"

    patched="$(lock_path_patched_count)"
    [[ "${patched}" -eq 0 ]] || die "Cargo.lock still has ${patched} path-patched bmux_* packages"
    info "override off; Cargo.lock resolves bmux_* from git (bmux_tui rev $(locked_bmux_tui_rev))"
}

cmd_status() {
    local path patched count
    count="$(override_count)"
    if [[ "${count}" -gt 0 ]]; then
        path="$(state_get path)"
        path="${path:-$(override_path)}"
        printf 'override:      on (%s bmux_* path patches in Cargo.toml; do not commit)\n' "${count}"
        printf 'BMUX path:     %s\n' "${path:-unknown}"
        printf 'BMUX HEAD:     %s\n' "$(bmux_head "${path:-/nonexistent}")"
        [[ -f "${state_file}" ]] || printf 'warning:       no state recorded; `off` will restore patch lines from HEAD\n'
    else
        printf 'override:      off\n'
        [[ ! -f "${state_file}" ]] || printf 'warning:       stale %s; run `scripts/bmux-dev.sh off`\n' "${state_file#"${root}"/}"
    fi
    patched="$(lock_path_patched_count)"
    if [[ "${patched}" -gt 0 ]]; then
        printf 'Cargo.lock:    patched (%s bmux_* packages from local paths; do not commit)\n' "${patched}"
        [[ "${count}" -gt 0 ]] || printf 'warning:       lock is patched but no override exists; run `scripts/bmux-dev.sh off`\n'
    else
        printf 'Cargo.lock:    git (bmux_tui rev %s)\n' "$(locked_bmux_tui_rev)"
    fi
    if lock_matches_head; then
        printf 'lock vs HEAD:  identical\n'
    else
        printf 'lock vs HEAD:  modified\n'
    fi
}

cmd_update() {
    local before after revs bmux local_head
    bmux="$(state_get path)"
    bmux="${bmux:-${default_bmux}}"
    cmd_off "$@"
    before="$(locked_bmux_tui_rev)"
    cargo update -p bmux_tui
    after="$(locked_bmux_tui_rev)"
    revs="$(lock_bmux_sources | awk '{ n = split($2, part, "#"); print part[n] }' | sort -u)"
    if [[ "$(printf '%s\n' "${revs}" | awk 'END { print NR }')" -ne 1 ]]; then
        info "warning: bmux_* packages lock different revisions: $(printf '%s' "${revs}" | tr '\n' ' ')"
    fi
    if [[ "${before}" == "${after}" ]]; then
        info "BMUX rev unchanged: ${after}"
    else
        info "BMUX rev updated: ${before} -> ${after}"
    fi
    if [[ -d "${bmux}" ]] && local_head="$(git -C "${bmux}" rev-parse HEAD 2>/dev/null)"; then
        if [[ "${local_head}" != "${after}" ]]; then
            info "note: local BMUX HEAD ${local_head} (${bmux}) differs from the locked rev; push it first if it should be included"
        fi
    fi
}

command="${1:-}"
[[ $# -eq 0 ]] || shift
case "${command}" in
    on)
        [[ $# -le 1 ]] || die "on takes at most one path"
        cmd_on "$@"
        ;;
    off) cmd_off "$@" ;;
    status)
        [[ $# -eq 0 ]] || die "status takes no arguments"
        cmd_status
        ;;
    update) cmd_update "$@" ;;
    -h | --help | help) usage ;;
    "")
        usage >&2
        exit 2
        ;;
    *)
        usage >&2
        die "unknown command: ${command}"
        ;;
esac
