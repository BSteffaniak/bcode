#!/bin/bash
# PID 1 is tini; it forwards signals to this supervisor, not directly to the daemon.
# No tracing: Fly's identity secret must never reach argv, children, or diagnostics.
set +x
set -euo pipefail
broker_identity=${BCODE_BROKER_AGE_IDENTITY:-}
unset BCODE_BROKER_AGE_IDENTITY
umask 077
export RUST_LOG=off
readonly config=/data/broker.toml identity=/run/bcode/age-identity
readonly uid=10001 gid=10001
stopping=0
daemon_pid= client_pid=
trap 'stopping=1' TERM INT
fail() { printf '%s\n' 'Broker stopped; operator configuration/custody review required.' >&2; exit 1; }

private_dir() {
    local path=$1
    [[ ! -L "$path" && ( ! -e "$path" || -d "$path" ) ]] || fail
    mkdir -p -- "$path"
    [[ $(stat -c %u "$path") == 0 || $(stat -c %u "$path") == "$uid" ]] || fail
    chown "$uid:$gid" "$path"
    chmod 700 "$path"
}

[[ $(id -u) == 0 ]] || fail
private_dir /data
private_dir /data/state
private_dir /run/bcode
# printf is a shell builtin; no secret is passed to an external process argv.
# The temporary file is private, on ephemeral /run, and never part of an image.
if [[ -n "$broker_identity" ]]; then
    [[ ${#broker_identity} -le 4096 && "$broker_identity" == AGE-SECRET-KEY-1* && "$broker_identity" != *$'\n'* ]] || fail
    staged=$(mktemp /run/bcode/identity.XXXXXX)
    printf '%s\n' "$broker_identity" > "$staged"
    unset broker_identity
    if [[ -e "$identity" || -L "$identity" ]]; then
        [[ -f "$identity" && ! -L "$identity" && $(stat -c %u "$identity") == "$uid" && $(stat -c %a "$identity") == 600 ]] || fail
        cmp -s "$staged" "$identity" || fail
        rm -- "$staged"
    else
        chown "$uid:$gid" "$staged"
        chmod 600 "$staged"
        mv -T -- "$staged" "$identity"
    fi
fi
unset broker_identity

operation=${1:-}
[[ $# -ge 1 ]] || fail
shift
# Explicit enrollment bootstrap: no listener, daemon, remote exchange, or repair.
if [[ "$operation" == serve && ${BCODE_BROKER_MODE:-} == maintenance ]]; then
    while (( ! stopping )); do sleep 1 & wait $! || :; done
    exit 0
fi
[[ -f "$config" && ! -L "$config" && $(stat -c %u "$config") == 0 && $(stat -c %a "$config") == 644 ]] || fail
! grep -q UNSET "$config" || fail
[[ -f "$identity" && ! -L "$identity" && $(stat -c %u "$identity") == "$uid" && $(stat -c %a "$identity") == 600 ]] || fail
[[ ! -L /data/operator.lock ]] || fail
exec 9>/data/operator.lock
flock -n 9 || fail

bcode() {
    setpriv --reuid="$uid" --regid="$gid" --clear-groups \
        /usr/local/bin/bcode --config-only "$config" --profile broker --state-root /data/state "$@"
}
setup() {
    bcode auth broker-setup "$1" --provider openai --profile broker-owner \
        --directory /data/custody --identity-file "$identity" "${@:2}"
}
ready() {
    # Local readiness is exit-status based: only Ready succeeds. Never inspect
    # presentation text or expose custody diagnostics in service logs.
    setup readiness >/dev/null 2>&1
}
case "$operation" in
    initialize)
        [[ $# == 2 && "$1" == --recipient && "$2" == age1* ]] || fail
        setup initialize --recipient "$2"
        exit $? ;;
    reconnect)
        [[ $# == 0 ]] || fail
        setup reconnect
        exit $? ;;
    readiness)
        [[ $# == 0 ]] || fail
        ready || fail
        printf '%s\n' 'Custody locally ready (not remote verification).'
        exit 0 ;;
    serve) [[ $# == 0 ]] || fail ;;
    *) fail ;;
esac
ready || fail
(( ! stopping )) || exit 0

shutdown() {
    trap '' TERM INT
    if [[ -n "$daemon_pid" ]] && kill -0 "$daemon_pid" 2>/dev/null; then
        # Existing public API requests cancellation and cleanup. Do not signal or
        # kill the daemon: its foreground path currently has no OS signal hook.
        bcode --request-timeout-secs 240 server stop >/dev/null 2>&1 || :
    fi
    if [[ -n "$daemon_pid" ]]; then wait "$daemon_pid" || :; fi
    if [[ -n "$client_pid" ]]; then wait "$client_pid" || :; fi
}
trap shutdown EXIT
bcode server run >/dev/null 2>&1 & daemon_pid=$!
available=0
for ((attempt=0; attempt<60 && !stopping; attempt++)); do
    kill -0 "$daemon_pid" 2>/dev/null || fail
    # Source startup-probe requires the existing daemon; no replacement launch.
    if bcode --request-timeout-secs 2 server startup-probe >/dev/null 2>&1; then
        available=1
        break
    fi
    sleep 1 & wait $! || :
done
(( available && !stopping )) || fail
# Explicit bounded IPC lifetime, not a keepalive/retry loop. After 30 days the
# invocation ends and this service stops for operator review. Never reconnect.
bcode --request-timeout-secs 2592000 plugin call bcode.auth-broker/v1 serve --daemon >/dev/null 2>&1 & client_pid=$!
while (( !stopping )); do
    kill -0 "$daemon_pid" 2>/dev/null && kill -0 "$client_pid" 2>/dev/null || fail
    sleep 1 & wait $! || :
done
exit 0
