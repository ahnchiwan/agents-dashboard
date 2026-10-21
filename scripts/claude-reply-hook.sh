#!/usr/bin/env bash
# Claude Code hook helper that records "a permission prompt is waiting" for
# abtop. Interface: `claude-reply-hook.sh mark|clear|clear_all`, hook payload
# JSON on stdin, marker at
# ${CLAUDE_CONFIG_DIR:-$HOME/.claude}/abtop-reply-state/<session_id>.
#
# Marker format (at most 128 bytes, one field per line; line 1 is the tool name
# and may be empty, which keeps old readers and old `clear` logic working):
#   <tool_name>
#   ev=permission|notification|unknown   hook event kind
#   at=<epoch_ms>                        when the prompt was marked
#   id=<tool_use_id>                     only when the payload carries one
#   nt=<epoch_ms>                        latest identity-less Notification seen
#                                        while a named marker was present
# Claude Code documents tool_use_id for PostToolUse, PostToolUseFailure and
# PermissionDenied but not for PermissionRequest or Notification, so `id` is
# optional and matching falls back to the tool name.
set -euo pipefail

# Keep every marker within abtop's 128-byte limit:
# 33 (name+LF) + 16 (ev) + 17 (at) + 44 (id) + 17 (nt) = 127.
# MAX_TOOL_NAME must match PERMISSION_MARKER_TOOL_CAP in src/collector/claude.rs.
MAX_TOOL_NAME=32
MAX_TOOL_ID=40

action=${1:-}
case "$action" in
  mark|clear|clear_all) ;;
  *) exit 0 ;;
esac

# The separator is U+001F, not a tab: tab is IFS whitespace, so adjacent empty
# fields would collapse and shift the remaining values. Values are sanitized
# here so they cannot contain a newline or the separator.
fields=$(jq -r --argjson name_cap "$MAX_TOOL_NAME" --argjson id_cap "$MAX_TOOL_ID" '
  def clean(cap): (. // "" | tostring | gsub("[^A-Za-z0-9_.:-]"; "") | .[0:cap]);
  [ (.session_id // "" | tostring | if test("^[A-Za-z0-9_-]{1,128}$") then . else "" end),
    (.tool_name | clean($name_cap)),
    (.tool_use_id | if (. // "" | tostring | length) > $id_cap then "" else clean($id_cap) end),
    (.hook_event_name | clean(32))
  ] | join("\u001f")' 2>/dev/null) || exit 0
IFS=$'\x1f' read -r session_id tool_name tool_use_id event_name <<< "$fields"
[[ $session_id =~ ^[A-Za-z0-9_-]{1,128}$ ]] || exit 0

profile_dir=${CLAUDE_CONFIG_DIR:-"$HOME/.claude"}
state_dir="$profile_dir/abtop-reply-state"
marker="$state_dir/$session_id"

now_ms() {
  local value
  if [[ -n ${EPOCHREALTIME:-} ]]; then
    value=${EPOCHREALTIME/[.,]/}
    printf '%s' "${value:0:13}"
    return
  fi
  value=$(date +%s%3N 2>/dev/null) || value=
  if [[ $value =~ ^[0-9]{13}$ ]]; then
    printf '%s' "$value"
  else
    printf '%s000' "$(date +%s)"
  fi
}

# Load the current marker into cur_* (all empty when absent or not a regular
# file). Values are validated again so a hand-edited marker cannot break writes.
load_marker() {
  cur_tool= cur_ev= cur_at= cur_id= cur_nt=
  [[ -f $marker && ! -L $marker ]] || return 0
  local line first=1
  while IFS= read -r line || [[ -n $line ]]; do
    if (( first )); then
      cur_tool=${line//[^A-Za-z0-9_.:-]/}
      cur_tool=${cur_tool:0:MAX_TOOL_NAME}
      first=0
      continue
    fi
    case "$line" in
      ev=*) cur_ev=${line#ev=} ;;
      at=*) cur_at=${line#at=} ;;
      id=*) cur_id=${line#id=} ;;
      nt=*) cur_nt=${line#nt=} ;;
    esac
  done < "$marker" || true
  [[ $cur_ev =~ ^(permission|notification|unknown)$ ]] || cur_ev=unknown
  [[ $cur_at =~ ^[0-9]{13}$ ]] || cur_at=
  [[ $cur_nt =~ ^[0-9]{13}$ ]] || cur_nt=
  [[ $cur_id =~ ^[A-Za-z0-9_-]{1,40}$ ]] || cur_id=
}

# write_marker <tool> <event> <at_ms> <tool_use_id> <notification_at_ms>
write_marker() {
  local temporary_marker
  temporary_marker=$(mktemp "$state_dir/.reply.XXXXXXXX")
  {
    printf '%s\n' "$1"
    printf 'ev=%s\n' "$2"
    printf 'at=%s\n' "$3"
    if [[ -n $4 ]]; then printf 'id=%s\n' "$4"; fi
    if [[ -n $5 ]]; then printf 'nt=%s\n' "$5"; fi
  } > "$temporary_marker"
  mv -f -- "$temporary_marker" "$marker"
}

case "$action" in
  mark)
    umask 077
    mkdir -p -- "$state_dir"
    case "$event_name" in
      PermissionRequest) event=permission ;;
      Notification) event=notification ;;
      *) event=unknown ;;
    esac
    now=$(now_ms)
    load_marker
    if [[ -z $tool_name && ( -n $cur_tool || -n $cur_id ) ]]; then
      # An identity-less Notification must not erase the identified marker
      # written by the PermissionRequest of the same prompt. Keep that identity
      # and record the notification time separately so a later, unrelated
      # prompt is still visible to abtop.
      write_marker "$cur_tool" "$cur_ev" "${cur_at:-$now}" "$cur_id" "$now"
    else
      write_marker "$tool_name" "$event" "$now" "$tool_use_id" ""
    fi
    ;;
  clear)
    [[ -f $marker ]] || exit 0
    load_marker
    if [[ -z $cur_tool && -z $cur_id ]]; then
      # Identity-less marker: any completed tool clears it.
      rm -f -- "$marker"
    elif [[ -n $cur_id && -n $tool_use_id ]]; then
      # Both sides know the call id: it decides, so a sibling call with the
      # same tool name does not clear this prompt.
      if [[ $cur_id == "$tool_use_id" ]]; then rm -f -- "$marker"; fi
    elif [[ $cur_tool == "$tool_name" ]]; then
      rm -f -- "$marker"
    fi
    ;;
  clear_all)
    rm -f -- "$marker"
    ;;
esac
