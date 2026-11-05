#!/bin/sh
if [ "$1" = "--version" ]; then printf 'fake-claude 1\n'; exit 0; fi
if [ "$1" = "auth" ]; then
  printf '{"loggedIn":true,"authMethod":"claude.ai","configDirectory":"%s"}\n' "$CLAUDE_CONFIG_DIR"
  exit 0
fi
native_id="native-auto"
for arg in "$@"; do
  case "$arg" in
    --session-id=*) native_id="${arg#--session-id=}" ;;
    --resume=*) native_id="${arg#--resume=}" ;;
  esac
done
if [ "$FAKE_CASE" = "resume" ]; then
  case "$*" in *--resume=native-123*) ;; *) exit 3 ;; esac
fi
while IFS= read -r frame; do
  case "$frame" in
    *'"subtype":"initialize"'*)
      case "$FAKE_CASE" in
        identity_mismatch) native_id="other-native" ;;
        identity_missing) native_id="" ;;
      esac
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"rust_1","response":{"commands":[],"models":[],"session_state":"idle"}}}\n'
      ;;
    *'"subtype":"interrupt"'*)
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"rust_2","response":{"interrupted":true}}}\n'
      ;;
    *'"type":"user"'*)
      case "$FAKE_CASE" in
        identity_confirm|identity_mismatch|resume)
          printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$native_id"
          ;;
      esac
      case "$FAKE_CASE" in
        callbacks)
          printf '%s\n' '{"type":"control_request","request_id":"a","request":{"subtype":"can_use_tool","tool_name":"AskUserQuestion","input":{"questions":[]}}}'
          printf '%s\n' '{"type":"control_request","request_id":"b","request":{"subtype":"hook_callback","callback_id":"hook_0","input":{}}}'
          printf '%s\n' '{"type":"control_cancel_request","request_id":"a"}'
          ;;
        background)
          printf '%s\n' '{"type":"system","subtype":"task_started","task_id":"task-1","task_type":"local_agent"}'
          printf '%s\n' '{"type":"result","is_error":false}'
          printf '%s\n' '{"type":"system","subtype":"session_state_changed","state":"idle"}'
          ;;
        followup)
          printf '%s\n' '{"type":"system","subtype":"task_started","task_id":"task-1","task_type":"local_agent"}'
          printf '%s\n' '{"type":"result"}'
          printf '%s\n' '{"type":"system","subtype":"task_notification","task_id":"task-1","status":"completed"}'
          printf '%s\n' '{"type":"assistant","message":{"content":[]}}'
          printf '%s\n' '{"type":"result"}'
          printf '%s\n' '{"type":"system","subtype":"session_state_changed","state":"idle"}'
          ;;
        idle_first)
          printf '%s\n' '{"type":"system","subtype":"session_state_changed","state":"idle","sdk_host_only":true}'
          printf '%s\n' '{"type":"result"}'
          ;;
        unknown)
          printf '%s\n' '{"type":"system","subtype":"session_state_changed","state":"future_mode"}'
          ;;
        overflow)
          i=0
          while [ "$i" -lt 500 ]; do printf '%s\n' '{"type":"assistant","message":{}}'; i=$((i + 1)); done
          ;;
        oversized)
          i=0
          while [ "$i" -lt 500 ]; do printf 'xxxxxxxx'; i=$((i + 1)); done
          printf '\n'
          ;;
        stderr)
          i=0
          while [ "$i" -lt 5000 ]; do printf '%s\n' 'secret diagnostic not retained' >&2; i=$((i + 1)); done
          printf '%s\n' '{"type":"result"}' '{"type":"system","subtype":"session_state_changed","state":"idle"}'
          ;;
        eof)
          printf '%s\n' '{"type":"result"}'
          exit 0
          ;;
        *)
          printf '%s\n' '{"type":"result","is_error":false}'
          printf '%s\n' '{"type":"system","subtype":"session_state_changed","state":"idle"}'
          ;;
      esac
      ;;
  esac
done
