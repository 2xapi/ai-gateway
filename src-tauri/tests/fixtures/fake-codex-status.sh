#!/bin/sh
# Test-only fake CLI. Set FAKE_CODEX_STATUS to control output and exit code.
case "${1:-}" in
  login)
    case "${FAKE_CODEX_STATUS:-chatgpt}" in
      chatgpt) printf '%s\n' 'Logged in using ChatGPT'; exit 0 ;;
      api_key) printf '%s\n' 'Authenticated with API key'; exit 0 ;;
      signed_out) printf '%s\n' 'Not logged in'; exit 1 ;;
      timeout) sleep 30 ;;
      *) printf '%s\n' 'unrecognized status'; exit 0 ;;
    esac
    ;;
  logout) printf '%s\n' 'Logged out'; exit 0 ;;
  *) printf '%s\n' 'unsupported fake command'; exit 2 ;;
esac
