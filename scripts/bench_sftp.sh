#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 3 ]]; then
  echo "usage: $0 <remote_spec> <destination_dir> <runs> [jobs]" >&2
  echo "example: $0 user@host:/src /tmp/prsync-bench 5 16" >&2
  exit 1
fi

REMOTE_SPEC="$1"
DEST_DIR="$2"
RUNS="$3"
JOBS="${4:-16}"

mkdir -p "$DEST_DIR"

need_cmd() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "missing required command: $1" >&2
    exit 1
  }
}

need_cmd prsync
need_cmd jq

if ! command -v rclone >/dev/null 2>&1; then
  echo "warning: rclone not found; it will be skipped" >&2
fi
if ! command -v rsync >/dev/null 2>&1; then
  echo "warning: rsync not found; it will be skipped" >&2
fi

stamp=$(date +%Y%m%d-%H%M%S)
out_json="$DEST_DIR/bench-$stamp.json"
out_md="$DEST_DIR/bench-$stamp.md"

tmp_json=$(mktemp)
printf '{"runs":[]}' > "$tmp_json"

run_one() {
  local tool="$1"
  local run_id="$2"
  local dst="$DEST_DIR/${tool}-${run_id}"
  rm -rf "$dst"
  mkdir -p "$dst"

  local start_ns end_ns elapsed_ns elapsed_s
  start_ns=$(date +%s%N)

  case "$tool" in
    prsync)
      prsync -vrPlu --jobs "$JOBS" "$REMOTE_SPEC" "$dst" >/dev/null 2>&1
      ;;
    rclone)
      rclone copy --transfers "$JOBS" --sftp-concurrency "$JOBS" "$REMOTE_SPEC" "$dst" >/dev/null 2>&1
      ;;
    rsync)
      rsync -vrPlu "$REMOTE_SPEC" "$dst" >/dev/null 2>&1
      ;;
    *)
      echo "unknown tool: $tool" >&2
      return 1
      ;;
  esac

  end_ns=$(date +%s%N)
  elapsed_ns=$((end_ns - start_ns))
  elapsed_s=$(awk -v n="$elapsed_ns" 'BEGIN { printf "%.6f", n/1000000000 }')

  jq --arg tool "$tool" --argjson run "$run_id" --argjson sec "$elapsed_s" \
    '.runs += [{tool:$tool,run:$run,seconds:$sec}]' "$tmp_json" > "$tmp_json.next"
  mv "$tmp_json.next" "$tmp_json"
}

for i in $(seq 1 "$RUNS"); do
  run_one prsync "$i"
  if command -v rclone >/dev/null 2>&1; then
    run_one rclone "$i"
  fi
  if command -v rsync >/dev/null 2>&1; then
    run_one rsync "$i"
  fi
done

jq '
  def median(xs): (xs|sort) as $s | if ($s|length)==0 then null else if ($s|length)%2==1 then $s[(($s|length)/2|floor)] else (($s[($s|length/2)-1]+$s[($s|length/2)])/2) end end;
  .summary = (
    .runs
    | group_by(.tool)
    | map({tool: .[0].tool, count: length, median_s: median(map(.seconds)), mean_s: (map(.seconds)|add/length)})
  )
' "$tmp_json" > "$out_json"

{
  echo "# SFTP Benchmark"
  echo
  echo "- remote: \\`$REMOTE_SPEC\\`"
  echo "- runs: $RUNS"
  echo "- jobs: $JOBS"
  echo
  echo "| Tool | Runs | Median (s) | Mean (s) |"
  echo "|---|---:|---:|---:|"
  jq -r '.summary[] | "| \(.tool) | \(.count) | \(.median_s|tostring) | \(.mean_s|tostring) |"' "$out_json"
} > "$out_md"

rm -f "$tmp_json"

echo "wrote: $out_json"
echo "wrote: $out_md"
