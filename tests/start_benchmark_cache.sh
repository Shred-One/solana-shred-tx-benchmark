#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
test_dir=$(mktemp -d)
trap 'rm -rf "$test_dir"' EXIT INT TERM
mkdir -p "$test_dir/bin" "$test_dir/tmp"
export TEST_DOWNLOAD_LOG="$test_dir/downloads"

cat >"$test_dir/bin/curl" <<'EOF'
#!/bin/sh
set -eu
output=""
url=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        -o) output=$2; shift 2 ;;
        -w | --retry) shift 2 ;;
        -*) shift ;;
        *) url=$1; shift ;;
    esac
done
printf '%s\n' "$url" >>"$TEST_DOWNLOAD_LOG"
case "$url" in
    */releases/latest)
        printf 'https://github.com/Shred-One/solana-shred-tx-benchmark/releases/tag/%s' "$LATEST_TAG"
        ;;
    *jito-shredstream-proxy*)
        printf '%s\n' '#!/bin/sh' 'trap "exit 0" TERM INT' 'while :; do sleep 1; done' >"$output"
        ;;
    *.sha256)
        tag=${url%/*}; tag=${tag##*/}
        printf '%s  %s\n' "$tag" solana-shred-tx-benchmark-x86_64-unknown-linux-gnu >"$output"
        ;;
    */solana-shred-tx-benchmark-x86_64-unknown-linux-gnu)
        tag=${url%/*}; tag=${tag##*/}
        [ "${FAIL_TAG:-}" != "$tag" ] || exit 22
        printf '%s\n' '#!/bin/sh' "# VERSION: $tag" 'exit 0' >"$output"
        ;;
esac
EOF

cat >"$test_dir/bin/sha256sum" <<'EOF'
#!/bin/sh
set -eu
[ "$1" = -c ]
if [ "$2" = - ]; then
    read -r expected file
    grep -q '^#!/bin/sh$' "$file"
else
    read -r expected file <"$2"
    grep -q "^# VERSION: $expected$" "$file"
fi
printf '%s: OK\n' "$file"
EOF
chmod +x "$test_dir/bin/curl" "$test_dir/bin/sha256sum"

run_launcher() {
    LATEST_TAG=$1
    shift
    export LATEST_TAG
    PATH="$test_dir/bin:$PATH" TMPDIR="$test_dir/tmp" "$root/start_benchmark.sh" \
        --source-1-address 127.0.0.1:21001 --source-1-name one \
        --source-2-address 127.0.0.1:21002 --source-2-name two --duration 1 "$@"
}

cache="$test_dir/tmp/solana-shred-tx-benchmark-cache"
metadata="$cache/benchmark-version"
cached_tag() {
    awk '{ print $1 }' "$metadata"
}
cached_binary() {
    directory=$(awk '{ print $2 }' "$metadata")
    printf '%s/%s/%s\n' "$cache" "$directory" solana-shred-tx-benchmark-x86_64-unknown-linux-gnu
}

: >"$TEST_DOWNLOAD_LOG"
run_launcher 0.1.1 >/dev/null
[ "$(cached_tag)" = 0.1.1 ]
grep -q 'jito-shredstream-proxy' "$TEST_DOWNLOAD_LOG"
grep -q '/releases/download/0.1.1/' "$TEST_DOWNLOAD_LOG"

: >"$TEST_DOWNLOAD_LOG"
output=$(run_launcher 0.1.2)
printf '%s\n' "$output" | grep -q 'Benchmark update available: 0.1.2 (cached: 0.1.1).'
if grep -q '/releases/download/' "$TEST_DOWNLOAD_LOG"; then
    exit 1
fi

run_launcher 0.1.2 --latest-update >/dev/null
[ "$(cached_tag)" = 0.1.2 ]

binary=$(cached_binary)
printf 'corrupt\n' >"$binary"
: >"$TEST_DOWNLOAD_LOG"
output=$(run_launcher 0.1.3)
printf '%s\n' "$output" | grep -q 'Restoring cached solana-shred-tx-benchmark 0.1.2'
grep -q '/releases/download/0.1.2/' "$TEST_DOWNLOAD_LOG"
if grep -q '/releases/download/0.1.3/' "$TEST_DOWNLOAD_LOG"; then
    exit 1
fi

: >"$TEST_DOWNLOAD_LOG"
if FAIL_TAG=0.1.3 run_launcher 0.1.3 --latest-update >/dev/null 2>&1; then
    echo 'Expected the failed update to return an error' >&2
    exit 1
fi
[ "$(cached_tag)" = 0.1.2 ]
binary=$(cached_binary)
grep -q '^# VERSION: 0.1.2$' "$binary"

cp "$binary" "$test_dir/override"
chmod +x "$test_dir/override"
: >"$TEST_DOWNLOAD_LOG"
SOLANA_SHRED_TX_BENCHMARK_BIN="$test_dir/override" run_launcher 0.1.3 >/dev/null
[ ! -s "$TEST_DOWNLOAD_LOG" ]

printf 'Launcher cache tests passed.\n'
