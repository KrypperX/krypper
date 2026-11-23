#!/usr/bin/env bash
# Source this file to export Cloudflare Workers AI credentials in the current shell.
# The token is read without echo and is never written to disk by this helper.

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  printf 'Source this helper in Bash: source %s\n' "$0" >&2
  exit 2
fi

_krypper_set_cloudflare_env() {
  local account_id="${CLOUDFLARE_ACCOUNT_ID:-}"
  local api_token="${CLOUDFLARE_API_TOKEN:-}"
  local entered_account_id=""
  local entered_api_token=""

  printf 'Cloudflare Account ID'
  [[ -n "$account_id" ]] && printf ' [press Enter to keep current]'
  printf ': '
  IFS= read -r entered_account_id || {
    printf '\nCould not read the Account ID.\n' >&2
    return 1
  }
  [[ -n "$entered_account_id" ]] && account_id="$entered_account_id"

  printf 'Workers AI API token (input hidden)'
  [[ -n "$api_token" ]] && printf ' [press Enter to keep current]'
  printf ': '
  IFS= read -r -s entered_api_token || {
    printf '\nCould not read the API token.\n' >&2
    return 1
  }
  printf '\n'
  [[ -n "$entered_api_token" ]] && api_token="$entered_api_token"

  if [[ ! "$account_id" =~ ^[A-Za-z0-9_-]{1,128}$ ]]; then
    printf 'Account ID must be 1-128 ASCII letters, digits, hyphens, or underscores.\n' >&2
    return 1
  fi
  if [[ -z "$api_token" ]]; then
    printf 'A nonempty Workers AI API token is required.\n' >&2
    return 1
  fi

  export CLOUDFLARE_ACCOUNT_ID="$account_id"
  export CLOUDFLARE_API_TOKEN="$api_token"
  unset account_id api_token entered_account_id entered_api_token
}

if _krypper_set_cloudflare_env; then
  printf 'Cloudflare Workers AI credentials are exported in this shell.\n'
  printf 'Unset CLOUDFLARE_API_TOKEN and CLOUDFLARE_ACCOUNT_ID when finished.\n'
  unset -f _krypper_set_cloudflare_env
else
  _krypper_status=$?
  unset -f _krypper_set_cloudflare_env
  return "$_krypper_status"
fi
