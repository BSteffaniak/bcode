#!/usr/bin/env bash
# Coordinator-built artifacts only. Never builds or resolves a PATH Bcode.
set -euo pipefail
mode="${1:-offline}"
if [[ "$mode" != offline && "$mode" != live && "$mode" != report ]]; then
    echo 'usage: check-judgement-eval.sh [offline|live|report]' >&2
    exit 2
fi
: "${BCODE_JUDGEMENT_TEST_BINARY:?absolute coordinator-built test artifact required}"
: "${BCODE_JUDGEMENT_TEST_SHA256:?expected test artifact SHA256 required}"
[[ "$BCODE_JUDGEMENT_TEST_BINARY" = /* && -f "$BCODE_JUDGEMENT_TEST_BINARY" && -x "$BCODE_JUDGEMENT_TEST_BINARY" ]] || exit 2
actual="$(shasum -a 256 "$BCODE_JUDGEMENT_TEST_BINARY")"
[[ "${actual%% *}" = "$BCODE_JUDGEMENT_TEST_SHA256" ]] || { echo 'test artifact identity mismatch' >&2; exit 2; }
export TMPDIR="${TMPDIR:-/Volumes/ehdd/bcode-eval-artifacts/tmp}"
if [[ "$mode" = report ]]; then
    : "${BCODE_JUDGEMENT_PLAN:?bounded v1 plan JSON required}"
    : "${BCODE_JUDGEMENT_CAPTURES:?bounded capture array JSON required}"
    : "${BCODE_JUDGEMENT_REPORT_OUTPUT:?new report output file required}"
    env -i PATH="${PATH}" TMPDIR="$TMPDIR" \
        BCODE_JUDGEMENT_PLAN="$BCODE_JUDGEMENT_PLAN" \
        BCODE_JUDGEMENT_CAPTURES="$BCODE_JUDGEMENT_CAPTURES" \
        BCODE_JUDGEMENT_REPORT_OUTPUT="$BCODE_JUDGEMENT_REPORT_OUTPUT" \
        BCODE_JUDGEMENT_REPORT_MODE="${BCODE_JUDGEMENT_REPORT_MODE:-full_readiness}" \
        "$BCODE_JUDGEMENT_TEST_BINARY" --ignored --exact report_captured_benchmark --test-threads=1
    exit
fi
if [[ "$mode" = offline ]]; then
    env -i PATH="${PATH}" TMPDIR="$TMPDIR" "$BCODE_JUDGEMENT_TEST_BINARY" --test-threads=1
    exit
fi
[[ "${BCODE_JUDGEMENT_ALLOW_LIVE:-}" = 1 ]] || { echo 'live requires explicit opt-in; unknown pricing blocks spend' >&2; exit 2; }
: "${BCODE_JUDGEMENT_JOB:?bounded CaptureJob JSON required}"
: "${BCODE_JUDGEMENT_SUBJECT_BINARY:?explicit absolute selected subject CLI required}"
: "${BCODE_JUDGEMENT_CAPTURE_OUTPUT:?new output file required}"
: "${BCODE_CONFIG:?explicit selected subject configuration required}"
: "${BCODE_STATE_DIR:?explicit selected subject state root required}"
python3 - "$BCODE_JUDGEMENT_JOB" "$BCODE_JUDGEMENT_SUBJECT_BINARY" <<'PY'
import hashlib, json, pathlib, sys
with pathlib.Path(sys.argv[1]).open('rb') as stream:
    raw = stream.read(128 * 1024 + 1)
if len(raw) > 128 * 1024:
    raise SystemExit('job exceeds capture bound')
job = json.loads(raw)
if job['schema_version'] != 1 or not job['allow_live'] or job['request']['source'] != 'live_consumer':
    raise SystemExit('explicit v1 live job required')
subject = job['request']['subject']
binary = pathlib.Path(subject['executable'])
if str(binary) != sys.argv[2] or not binary.is_absolute() or not binary.is_file():
    raise SystemExit('exact selected absolute subject required')
with binary.open('rb') as stream:
    digest = hashlib.file_digest(stream, 'sha256').hexdigest()
if digest != subject['executable_sha256']:
    raise SystemExit('subject executable identity mismatch')
# Never dump/hash credential files. Hosting coordinator verifies non-secret resolved semantics.
if not subject['configuration_complete'] or not all(subject.get(k) for k in
        ('configuration_sha256', 'artifact_id', 'provider', 'model', 'state_id')):
    raise SystemExit('verified subject/config/model/state provenance required')
PY
# Serial one-job dispatch; budget admission is owned by CLI/coordinator, not guessed here.
env -i PATH="${PATH}" TMPDIR="$TMPDIR" HOME="${HOME}" \
    BCODE_CONFIG="$BCODE_CONFIG" BCODE_STATE_DIR="$BCODE_STATE_DIR" \
    BCODE_JUDGEMENT_ALLOW_LIVE=1 BCODE_JUDGEMENT_JOB="$BCODE_JUDGEMENT_JOB" \
    BCODE_JUDGEMENT_SUBJECT_BINARY="$BCODE_JUDGEMENT_SUBJECT_BINARY" \
    BCODE_JUDGEMENT_CAPTURE_OUTPUT="$BCODE_JUDGEMENT_CAPTURE_OUTPUT" \
    "$BCODE_JUDGEMENT_TEST_BINARY" --ignored --exact live_public_consumer_capture --test-threads=1
