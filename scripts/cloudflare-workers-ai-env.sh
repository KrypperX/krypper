#!/usr/bin/env bash
# Source this file to export Cloudflare Workers AI / AI Gateway settings in the current shell.
# The token is read without echo and is never written to disk by this helper.

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  printf 'Source this helper in Bash: source %s\n' "$0" >&2
  exit 2
fi

_krypper_set_cloudflare_env() {
  local LC_ALL=C
  local account_id="${CLOUDFLARE_ACCOUNT_ID:-}"
  local api_token="${CLOUDFLARE_API_TOKEN:-}"
  local gateway_id="${CLOUDFLARE_AI_GATEWAY_ID:-default}"
  local entered_account_id=""
  local entered_api_token=""
  local entered_gateway_id=""

  printf 'Cloudflare Account ID'
  [[ -n "$account_id" ]] && printf ' [press Enter to keep current]'
  printf ': '
  IFS= read -r entered_account_id || {
    printf '\nCould not read the Account ID.\n' >&2
    return 1
  }
  [[ -n "$entered_account_id" ]] && account_id="$entered_account_id"

  printf 'Cloudflare API token (input hidden)'
  [[ -n "$api_token" ]] && printf ' [press Enter to keep current]'
  printf ': '
  IFS= read -r -s entered_api_token || {
    printf '\nCould not read the API token.\n' >&2
    return 1
  }
  printf '\n'
  [[ -n "$entered_api_token" ]] && api_token="$entered_api_token"

  printf 'AI Gateway ID for --provider cloudflare-ai-gateway [%s]: ' "$gateway_id"
  IFS= read -r entered_gateway_id || {
    printf '\nCould not read the AI Gateway ID.\n' >&2
    return 1
  }
  [[ -n "$entered_gateway_id" ]] && gateway_id="$entered_gateway_id"

  if [[ ! "$account_id" =~ ^[A-Za-z0-9_-]{1,128}$ ]]; then
    printf 'Account ID must be 1-128 ASCII letters, digits, hyphens, or underscores.\n' >&2
    return 1
  fi
  if [[ -z "$api_token" ]]; then
    printf 'A nonempty Cloudflare API token is required.\n' >&2
    return 1
  fi
  if [[ ! "$gateway_id" =~ ^[[:graph:]]{1,64}$ ]]; then
    printf 'AI Gateway ID must be 1-64 visible ASCII characters without spaces.\n' >&2
    return 1
  fi

  export CLOUDFLARE_ACCOUNT_ID="$account_id"
  export CLOUDFLARE_API_TOKEN="$api_token"
  export CLOUDFLARE_AI_GATEWAY_ID="$gateway_id"
  unset account_id api_token gateway_id entered_account_id entered_api_token entered_gateway_id
}

_krypper_cloudflare_xtrace_enabled=0
case $- in
  *x*)
    _krypper_cloudflare_xtrace_enabled=1
    set +x
    ;;
esac

_krypper_cloudflare_status=0
if _krypper_set_cloudflare_env; then
  printf 'Cloudflare credentials and AI Gateway selection are exported in this shell.\n'
  printf 'Use --provider cloudflare for Workers AI or --provider cloudflare-ai-gateway for Gateway routing.\n'
  printf 'Unset CLOUDFLARE_API_TOKEN, CLOUDFLARE_ACCOUNT_ID, and CLOUDFLARE_AI_GATEWAY_ID when finished.\n'
else
  _krypper_cloudflare_status=$?
fi
unset -f _krypper_set_cloudflare_env
if (( _krypper_cloudflare_xtrace_enabled )); then
  set -x
fi
unset _krypper_cloudflare_xtrace_enabled
if (( _krypper_cloudflare_status != 0 )); then
  unset _krypper_cloudflare_status
  return 1
fi
unset _krypper_cloudflare_status
