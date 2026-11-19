#!/usr/bin/env bash
set -euo pipefail
: "${CF_ACCOUNT_ID:?Set CF_ACCOUNT_ID}"
: "${CF_API_TOKEN:?Set CF_API_TOKEN (Account / Account Analytics / Read)}"
if [[ ! "$CF_ACCOUNT_ID" =~ ^[a-fA-F0-9]{32}$ ]]; then
  echo 'CF_ACCOUNT_ID must be a 32-character hexadecimal account ID' >&2
  exit 1
fi
query_name="${1:-all}"
case "$query_name" in
  all) query_names=(daily weekly versions channels platforms sampling registry_daily registry_installs) ;;
  daily|weekly|versions|channels|platforms|sampling|registry_daily|registry_installs) query_names=("$query_name") ;;
  *) echo 'Usage: stats.sh [all|daily|weekly|versions|channels|platforms|sampling|registry_daily|registry_installs]' >&2; exit 1 ;;
esac
script_directory="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
for query_name in "${query_names[@]}"; do
  query="$(awk -v name="$query_name" '
    /^-- name: / { selected = ($3 == name); next }
    selected { print }
  ' "$script_directory/queries.sql")"
  echo "=== $query_name ==="
  curl --fail-with-body --silent --show-error \
    "https://api.cloudflare.com/client/v4/accounts/${CF_ACCOUNT_ID}/analytics_engine/sql" \
    --header "Authorization: Bearer ${CF_API_TOKEN}" \
    --header 'Content-Type: text/plain' \
    --data-binary "$query"
  echo
done
