#!/bin/sh
if [ "$1" = "--version" ]; then printf '%s\n' "${FAKE_VERSION:-2.1.308 (Claude Code)}"; exit 0; fi
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
  # The child environment has no PATH: use shell expansion only, never external tools.
  request_id=${frame#*\"request_id\":\"}
  request_id=${request_id%%\"*}
  case "$frame" in
    *'"subtype":"initialize"'*)
      case "$FAKE_CASE" in
        identity_mismatch) native_id="other-native" ;;
        identity_missing) native_id="" ;;
      esac
      models='[]'
      if [ "$FAKE_CASE" = "configure" ]; then
        models='[{"value":"sonnet","displayName":"Sonnet"},{"value":"claude-opus-4-1","displayName":"Opus"}]'
      fi
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"commands":[],"models":%s,"session_state":"idle"}}}\n' "$request_id" "$models"
      ;;
    *'"subtype":"interrupt"'*)
      printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"interrupted":true}}}\n' "$request_id"
      ;;
    *'"subtype":"set_model"'*)
      case "$frame" in
        *'"model":"sonnet"'*)
          printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{"model":"claude-sonnet-4-6"}}}\n' "$request_id"
          ;;
        *)
          printf '{"type":"control_response","response":{"subtype":"error","request_id":"%s","error":"unknown model"}}\n' "$request_id"
          ;;
      esac
      ;;
    *'"subtype":"set_permission_mode"'*)
      case "$frame" in
        *'"mode":"default"'*|*'"mode":"plan"'*|*'"mode":"acceptEdits"'*)
          printf '{"type":"control_response","response":{"subtype":"success","request_id":"%s","response":{}}}\n' "$request_id"
          ;;
        *)
          printf '{"type":"control_response","response":{"subtype":"error","request_id":"%s","error":"bad mode"}}\n' "$request_id"
          ;;
      esac
      ;;
    *'"type":"user"'*)
      case "$FAKE_CASE" in
        identity_confirm|identity_mismatch|resume)
          printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$native_id"
          ;;
      esac
      case "$FAKE_CASE" in
        attribution)
          case "$frame" in
            *'"uuid":"00000000-0000-4000-8000-000000000001"'*) ;;
            *) exit 4 ;;
          esac
          printf '%s\n' '{"type":"command_lifecycle","state":"started","command_uuid":"00000000-0000-4000-8000-000000000001","session_id":"native-auto"}'
          printf '%s\n' '{"type":"assistant","uuid":"00000000-0000-4000-8000-000000000002","user_message_uuid":"00000000-0000-4000-8000-000000000001","session_id":"native-auto","message":{"content":[]}}'
          printf '%s\n' '{"type":"rate_limit_event","session_id":"native-auto"}'
          printf '%s\n' '{"type":"result","uuid":"00000000-0000-4000-8000-000000000003","user_message_uuid":"00000000-0000-4000-8000-000000000001","session_id":"native-auto","is_error":false,"queued_turn_count":0}'
          printf '%s\n' '{"type":"system","subtype":"session_state_changed","state":"idle","session_id":"native-auto","sdk_host_only":true}'
          ;;
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
