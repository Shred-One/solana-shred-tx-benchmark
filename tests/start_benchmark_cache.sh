#!/bin/sh
set -eu

root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
test_dir=$(mktemp -d)
trap 'rm -rf "$test_dir"' EXIT INT TERM
mkdir -p "$test_dir/bin" "$test_dir/tmp"
export TEST_DOWNLOAD_LOG="$test_dir/downloads"
REAL_STAT=$(command -v stat)
export REAL_STAT

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
        if [ "${DELAY_TAG:-}" = "$tag" ]; then
            sleep 2
        fi
        printf '%s\n' '#!/bin/sh' "# VERSION: $tag" 'exit 0' >"$output"
        ;;
esac
EOF

cat >"$test_dir/bin/stat" <<'EOF'
#!/bin/sh
last=""
for argument in "$@"; do
    last=$argument
done
if [ -n "${WRONG_OWNER_PATH:-}" ] && [ "$last" = "$WRONG_OWNER_PATH" ]; then
    printf '999999\n'
    exit 0
fi
exec "$REAL_STAT" "$@"
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

export TEST_ROOT="$root"
export TEST_BENCHMARK_LOG="$test_dir/benchmark-arguments"
cat >"$test_dir/bin/config-benchmark" <<'EOF'
#!/bin/sh
printf '%s\n' "$@" >"$TEST_BENCHMARK_LOG"
EOF
cat >"$test_dir/bin/run-config-launcher" <<'EOF'
#!/bin/sh
exec "$TEST_ROOT/start_benchmark.sh" --duration 1 "$@"
EOF
chmod +x "$test_dir/bin/curl" "$test_dir/bin/sha256sum" "$test_dir/bin/stat" \
    "$test_dir/bin/config-benchmark" "$test_dir/bin/run-config-launcher"

run_launcher() {
    LATEST_TAG=$1
    shift
    export LATEST_TAG
    launcher_tmp=${TEST_TMPDIR:-$test_dir/tmp}
    PATH="$test_dir/bin:$PATH" TMPDIR="$launcher_tmp" "$root/start_benchmark.sh" \
        --source-1-address 127.0.0.1:21001 --source-1-name one \
        --source-2-address 127.0.0.1:21002 --source-2-name two --duration 1 "$@"
}

uid=$(id -u)
cache="$test_dir/tmp/solana-shred-tx-benchmark-cache-$uid"
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

safe_metadata=$(cat "$metadata")
safe_directory=${safe_metadata#* }
for unsafe_metadata in '0.1.2 ../outside' "0.1.02 $safe_directory"; do
    printf '%s\n' "$unsafe_metadata" >"$metadata"
    : >"$TEST_DOWNLOAD_LOG"
    if run_launcher 0.1.3 >/dev/null 2>&1; then
        echo 'Expected unsafe metadata to be rejected' >&2
        exit 1
    fi
    [ ! -s "$TEST_DOWNLOAD_LOG" ]
done
printf '%s\n' "$safe_metadata" >"$metadata"

security_tmp="$test_dir/security"
mkdir -p "$security_tmp/target"
ln -s "$security_tmp/target" "$security_tmp/solana-shred-tx-benchmark-cache-$uid"
target_mode=$(stat -c %a "$security_tmp/target")
if TEST_TMPDIR="$security_tmp" run_launcher 0.1.3 >/dev/null 2>&1; then
    echo 'Expected a cache symlink to be rejected' >&2
    exit 1
fi
[ "$(stat -c %a "$security_tmp/target")" = "$target_mode" ]

non_directory_tmp="$test_dir/non-directory"
mkdir -p "$non_directory_tmp"
: >"$non_directory_tmp/solana-shred-tx-benchmark-cache-$uid"
if TEST_TMPDIR="$non_directory_tmp" run_launcher 0.1.3 >/dev/null 2>&1; then
    echo 'Expected a non-directory cache path to be rejected' >&2
    exit 1
fi

wrong_owner_tmp="$test_dir/wrong-owner"
wrong_owner_cache="$wrong_owner_tmp/solana-shred-tx-benchmark-cache-$uid"
mkdir -p "$wrong_owner_cache"
wrong_owner_mode=$(stat -c %a "$wrong_owner_cache")
if WRONG_OWNER_PATH="$wrong_owner_cache" TEST_TMPDIR="$wrong_owner_tmp" run_launcher 0.1.3 >/dev/null 2>&1; then
    echo 'Expected a wrong-owner cache to be rejected' >&2
    exit 1
fi
[ "$(stat -c %a "$wrong_owner_cache")" = "$wrong_owner_mode" ]

binary=$(cached_binary)
printf 'corrupt\n' >"$binary"
: >"$TEST_DOWNLOAD_LOG"
DELAY_TAG=0.1.2 run_launcher 0.1.2 >"$test_dir/older.log" 2>&1 &
older_pid=$!
attempt=0
until grep -q '/releases/download/0.1.2/solana-shred-tx-benchmark-x86_64' "$TEST_DOWNLOAD_LOG"; do
    if ! kill -0 "$older_pid" 2>/dev/null || [ "$attempt" -ge 40 ]; then
        wait "$older_pid" || true
        echo 'Older launcher did not enter its delayed download' >&2
        exit 1
    fi
    attempt=$((attempt + 1))
    sleep 0.05
done
run_launcher 0.1.3 --latest-update >"$test_dir/newer.log" 2>&1 &
newer_pid=$!
wait "$older_pid"
wait "$newer_pid"
[ "$(cached_tag)" = 0.1.3 ]

binary=$(cached_binary)
cp "$binary" "$test_dir/override"
chmod +x "$test_dir/override"
: >"$TEST_DOWNLOAD_LOG"
SOLANA_SHRED_TX_BENCHMARK_BIN="$test_dir/override" run_launcher 0.1.3 >/dev/null
[ ! -s "$TEST_DOWNLOAD_LOG" ]

config_tmp="$test_dir/config-tmp"
mkdir -p "$config_tmp"
config_cache="$config_tmp/solana-shred-tx-benchmark-cache-$uid"
source_config="$config_cache/source-config"
run_config_interactive() {
    input=$1
    shift
    : >"$TEST_BENCHMARK_LOG"
    printf '%b' "$input" | PATH="$test_dir/bin:$PATH" TMPDIR="$config_tmp" \
        SOLANA_SHRED_TX_BENCHMARK_BIN="$test_dir/bin/config-benchmark" \
        script -qefc "run-config-launcher $*" /dev/null
}

output=$(run_config_interactive '127.0.0.1:22001\nfirst\n127.0.0.1:22002\nsecond\n')
[ -f "$source_config" ] && [ ! -L "$source_config" ]
[ "$(stat -c %a "$source_config")" = 600 ]
[ "$(stat -c %u "$source_config")" = "$uid" ]
printf '%s\n' 127.0.0.1:22001 first 127.0.0.1:22002 second >"$test_dir/expected-config"
cmp "$test_dir/expected-config" "$source_config"
printf '%s\n' "$output" | grep -q 'First source UDP address (IP:PORT)'

output=$(run_config_interactive '\n')
printf '%s\n' "$output" | grep -q 'Saved source configuration:'
printf '%s\n' "$output" | grep -q 'Use this configuration? \[Y/n\]'
grep -q '^first$' "$TEST_BENCHMARK_LOG"

old_inode=$(stat -c %i "$source_config")
run_config_interactive 'n\n127.0.0.1:22101\nchanged-one\n127.0.0.1:22102\nchanged-two\n' >/dev/null
[ "$(stat -c %i "$source_config")" != "$old_inode" ]
grep -q '^changed-one$' "$TEST_BENCHMARK_LOG"
if find "$config_cache" -name '.source-config.*' | grep -q .; then
    echo 'Expected no partial source configuration files' >&2
    exit 1
fi

output=$(run_config_interactive '127.0.0.1:22201\n127.0.0.1:22202\ncli-two\n' --source-1-name cli-one)
if printf '%s\n' "$output" | grep -q 'Saved source configuration:'; then
    echo 'Expected a source CLI option to bypass saved configuration reuse' >&2
    exit 1
fi
printf '%s\n' 127.0.0.1:22201 cli-one 127.0.0.1:22202 cli-two >"$test_dir/expected-config"
cmp "$test_dir/expected-config" "$source_config"

printf '%s\n' invalid corrupt 127.0.0.1:22002 second extra >"$source_config"
chmod 600 "$source_config"
output=$(run_config_interactive '127.0.0.1:22301\nrepaired-one\n127.0.0.1:22302\nrepaired-two\n')
printf '%s\n' "$output" | grep -q 'Ignoring invalid saved source configuration.'
grep -q '^repaired-one$' "$TEST_BENCHMARK_LOG"

marker="$test_dir/config-was-sourced"
printf '%s\n' 127.0.0.1:22401 "\$(touch $marker)" 127.0.0.1:22402 safe >"$source_config"
chmod 600 "$source_config"
run_config_interactive 'y\n' >/dev/null
[ ! -e "$marker" ]
grep -F -q "\$(touch $marker)" "$TEST_BENCHMARK_LOG"

chmod 644 "$source_config"
if TEST_TMPDIR="$config_tmp" run_launcher 0.1.3 >/dev/null 2>&1; then
    echo 'Expected an overly permissive source configuration to be rejected' >&2
    exit 1
fi
chmod 600 "$source_config"
if WRONG_OWNER_PATH="$source_config" TEST_TMPDIR="$config_tmp" run_launcher 0.1.3 >/dev/null 2>&1; then
    echo 'Expected a wrong-owner source configuration to be rejected' >&2
    exit 1
fi
mv "$source_config" "$source_config.safe"
ln -s "$source_config.safe" "$source_config"
if TEST_TMPDIR="$config_tmp" run_launcher 0.1.3 >/dev/null 2>&1; then
    echo 'Expected a source configuration symlink to be rejected' >&2
    exit 1
fi
rm "$source_config"
mkdir "$source_config"
if TEST_TMPDIR="$config_tmp" run_launcher 0.1.3 >/dev/null 2>&1; then
    echo 'Expected a source configuration directory to be rejected' >&2
    exit 1
fi

printf 'Launcher cache and configuration tests passed.\n'
