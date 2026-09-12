#!/bin/sh
set -eu

PROXY_VERSION="v0.2.14"
PROXY_SHA256="1b30b8e3fb4212b30774e9a277c19d42e1ff058d76f444d05afacd2e8f747a70"
PROXY_URL="https://github.com/jito-labs/shredstream-proxy/releases/download/${PROXY_VERSION}/jito-shredstream-proxy-x86_64-unknown-linux-gnu"
BENCHMARK_BINARY_NAME="solana-shred-tx-benchmark-x86_64-unknown-linux-gnu"
BENCHMARK_REPOSITORY_URL="https://github.com/Shred-One/solana-shred-tx-benchmark"

source_1_address=""
source_1_name=""
source_2_address=""
source_2_name=""
duration="60"
grpc_port_1="19091"
grpc_port_2="19092"
latest_update=false

usage() {
    cat <<'EOF'
Usage: start_benchmark.sh [OPTIONS]

Options:
  --source-1-address IP:PORT  Local UDP address for the first shred source
  --source-1-name NAME        Display name for the first source
  --source-2-address IP:PORT  Local UDP address for the second shred source
  --source-2-name NAME        Display name for the second source
  --duration SECONDS          Benchmark duration (default: 60)
  --grpc-port-1 PORT          First local proxy gRPC port (default: 19091)
  --grpc-port-2 PORT          Second local proxy gRPC port (default: 19092)
  --latest-update             Download and use the latest benchmark release
  -h, --help                  Show this help
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --source-1-address | --source-1-name | --source-2-address | --source-2-name | --duration | --grpc-port-1 | --grpc-port-2)
            [ "$#" -ge 2 ] || {
                printf 'Missing value for %s\n' "$1" >&2
                exit 2
            }
            option=$1
            value=$2
            shift 2
            case "$option" in
                --source-1-address) source_1_address=$value ;;
                --source-1-name) source_1_name=$value ;;
                --source-2-address) source_2_address=$value ;;
                --source-2-name) source_2_name=$value ;;
                --duration) duration=$value ;;
                --grpc-port-1) grpc_port_1=$value ;;
                --grpc-port-2) grpc_port_2=$value ;;
            esac
            ;;
        --latest-update)
            latest_update=true
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            printf 'Unknown option: %s\n\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

prompt() {
    label=$1
    if [ ! -r /dev/tty ]; then
        printf 'No terminal is available. Pass %s on the command line.\n' "$label" >&2
        exit 2
    fi
    printf '%s: ' "$label" >/dev/tty
    IFS= read -r answer </dev/tty
    printf '%s' "$answer"
}

[ -n "$source_1_address" ] || source_1_address=$(prompt "First source UDP address (IP:PORT)")
[ -n "$source_1_name" ] || source_1_name=$(prompt "First source name")
[ -n "$source_2_address" ] || source_2_address=$(prompt "Second source UDP address (IP:PORT)")
[ -n "$source_2_name" ] || source_2_name=$(prompt "Second source name")

split_address() {
    address=$1
    case "$address" in
        \[*\]:*)
            split_ip=${address#\[}
            split_ip=${split_ip%%\]*}
            split_port=${address##*:}
            ;;
        *:*)
            split_ip=${address%:*}
            split_port=${address##*:}
            ;;
        *)
            printf 'Invalid UDP address: %s (expected IP:PORT)\n' "$address" >&2
            exit 2
            ;;
    esac
    case "$split_port" in
        '' | *[!0-9]*)
            printf 'Invalid port in UDP address: %s\n' "$address" >&2
            exit 2
            ;;
    esac
    if [ "$split_port" -lt 1 ] || [ "$split_port" -gt 65535 ]; then
        printf 'Port out of range in UDP address: %s\n' "$address" >&2
        exit 2
    fi
}

split_address "$source_1_address"
source_1_ip=$split_ip
source_1_port=$split_port
split_address "$source_2_address"
source_2_ip=$split_ip
source_2_port=$split_port

for number in "$duration" "$grpc_port_1" "$grpc_port_2"; do
    case "$number" in
        '' | *[!0-9]*)
            printf 'Duration and gRPC ports must be positive integers.\n' >&2
            exit 2
            ;;
    esac
done
[ "$duration" -gt 0 ] || {
    printf 'Duration must be greater than zero.\n' >&2
    exit 2
}
for port in "$grpc_port_1" "$grpc_port_2"; do
    if [ "$port" -lt 1 ] || [ "$port" -gt 65535 ]; then
        printf 'gRPC port out of range: %s\n' "$port" >&2
        exit 2
    fi
done
[ "$grpc_port_1" != "$grpc_port_2" ] || {
    printf 'gRPC ports must be different.\n' >&2
    exit 2
}

[ "$(uname -m)" = "x86_64" ] || {
    printf 'The pinned ShredStream proxy binary supports x86_64 Linux only.\n' >&2
    exit 1
}
for command in curl flock id sha256sum stat; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'Required command not found: %s\n' "$command" >&2
        exit 1
    }
done
if [ -n "${SOLANA_SHRED_TX_BENCHMARK_BIN:-}" ] && [ ! -x "$SOLANA_SHRED_TX_BENCHMARK_BIN" ]; then
    printf 'Benchmark override is not executable: %s\n' "$SOLANA_SHRED_TX_BENCHMARK_BIN" >&2
    exit 1
fi

cache_uid=$(id -u)
cache_dir="${TMPDIR:-/tmp}/solana-shred-tx-benchmark-cache-$cache_uid"
cache_dir_valid() {
    [ ! -L "$cache_dir" ] && [ -d "$cache_dir" ] &&
        [ "$(stat -c %u -- "$cache_dir" 2>/dev/null)" = "$cache_uid" ]
}
if [ ! -e "$cache_dir" ] && [ ! -L "$cache_dir" ]; then
    old_umask=$(umask)
    umask 077
    mkdir "$cache_dir" 2>/dev/null || true
    umask "$old_umask"
fi
if ! cache_dir_valid; then
    printf 'Unsafe benchmark cache directory: %s\n' "$cache_dir" >&2
    exit 1
fi
chmod 700 "$cache_dir"

lock_file="$cache_dir/cache.lock"
if [ ! -e "$lock_file" ] && [ ! -L "$lock_file" ]; then
    (umask 077 && set -C && : >"$lock_file") 2>/dev/null || true
fi
if [ -L "$lock_file" ] || [ ! -f "$lock_file" ] ||
    [ "$(stat -c %u -- "$lock_file" 2>/dev/null)" != "$cache_uid" ]; then
    printf 'Unsafe benchmark cache lock: %s\n' "$lock_file" >&2
    exit 1
fi
exec 9>>"$lock_file"
flock -x 9

work_dir=$(mktemp -d "${TMPDIR:-/tmp}/solana-shred-tx-benchmark.XXXXXX")
proxy_pid_1=""
proxy_pid_2=""
cleanup() {
    trap - EXIT INT TERM
    for pid in "$proxy_pid_1" "$proxy_pid_2"; do
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            kill "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    rm -rf "$work_dir"
}
trap cleanup EXIT INT TERM

valid_release_tag() {
    case "$1" in
        0.1.*)
            release_patch=${1#0.1.}
            case "$release_patch" in
                '' | *[!0-9]* | 0[0-9]*) return 1 ;;
            esac
            ;;
        *) return 1 ;;
    esac
}

resolve_latest_release() {
    latest_release_url=$(curl -fsSL --retry 3 -o /dev/null -w '%{url_effective}' \
        "$BENCHMARK_REPOSITORY_URL/releases/latest") || return 1
    latest_release_url=${latest_release_url%/}
    resolved_tag=${latest_release_url##*/}
    if ! valid_release_tag "$resolved_tag"; then
        printf 'Invalid latest release tag: %s\n' "$resolved_tag" >&2
        return 1
    fi
    printf '%s\n' "$resolved_tag"
}

proxy_bin="$cache_dir/jito-shredstream-proxy-$PROXY_VERSION"
if [ -e "$proxy_bin" ] || [ -L "$proxy_bin" ]; then
    if [ -L "$proxy_bin" ] || [ ! -f "$proxy_bin" ] ||
        [ "$(stat -c %u -- "$proxy_bin" 2>/dev/null)" != "$cache_uid" ]; then
        printf 'Unsafe cached Jito ShredStream proxy: %s\n' "$proxy_bin" >&2
        exit 1
    fi
fi
if [ ! -f "$proxy_bin" ] ||
    ! printf '%s  %s\n' "$PROXY_SHA256" "$proxy_bin" | sha256sum -c - >/dev/null 2>&1; then
    proxy_temp=$(mktemp "$cache_dir/.jito-shredstream-proxy.XXXXXX")
    printf 'Downloading Jito ShredStream proxy %s...\n' "$PROXY_VERSION"
    if ! curl -fsSL --retry 3 "$PROXY_URL" -o "$proxy_temp" ||
        ! printf '%s  %s\n' "$PROXY_SHA256" "$proxy_temp" | sha256sum -c -; then
        rm -f "$proxy_temp"
        exit 1
    fi
    chmod +x "$proxy_temp"
    mv -f "$proxy_temp" "$proxy_bin"
else
    printf 'Using cached Jito ShredStream proxy %s.\n' "$PROXY_VERSION"
fi
chmod +x "$proxy_bin"

metadata_file="$cache_dir/benchmark-version"
benchmark_dir=""
benchmark_bin=""
checksum_file=""

benchmark_cache_valid() {
    [ -f "$benchmark_bin" ] && [ -f "$checksum_file" ] &&
        (cd "$benchmark_dir" && sha256sum -c "$BENCHMARK_BINARY_NAME.sha256") >/dev/null 2>&1
}

benchmark_cache_safe() {
    for cached_file in "$benchmark_bin" "$checksum_file"; do
        if [ -e "$cached_file" ] || [ -L "$cached_file" ]; then
            if [ -L "$cached_file" ] || [ ! -f "$cached_file" ] ||
                [ "$(stat -c %u -- "$cached_file" 2>/dev/null)" != "$cache_uid" ]; then
                printf 'Unsafe cached benchmark file: %s\n' "$cached_file" >&2
                return 1
            fi
        fi
    done
    return 0
}

download_benchmark() {
    download_tag=$1
    download_dir=$(mktemp -d "$cache_dir/benchmark.$download_tag.XXXXXX")
    download_url="$BENCHMARK_REPOSITORY_URL/releases/download/$download_tag"
    printf 'Downloading solana-shred-tx-benchmark %s...\n' "$download_tag"
    if ! curl -fsSL --retry 3 "$download_url/$BENCHMARK_BINARY_NAME" \
        -o "$download_dir/$BENCHMARK_BINARY_NAME" ||
        ! curl -fsSL --retry 3 "$download_url/$BENCHMARK_BINARY_NAME.sha256" \
            -o "$download_dir/$BENCHMARK_BINARY_NAME.sha256" ||
        ! (cd "$download_dir" && sha256sum -c "$BENCHMARK_BINARY_NAME.sha256"); then
        rm -rf "$download_dir"
        return 1
    fi
    chmod +x "$download_dir/$BENCHMARK_BINARY_NAME"
    metadata_temp=$(mktemp "$cache_dir/.benchmark-version.XXXXXX")
    printf '%s %s\n' "$download_tag" "${download_dir##*/}" >"$metadata_temp"
    mv -f "$metadata_temp" "$metadata_file"
    benchmark_dir=$download_dir
    benchmark_bin="$benchmark_dir/$BENCHMARK_BINARY_NAME"
    checksum_file="$benchmark_bin.sha256"
}

if [ -n "${SOLANA_SHRED_TX_BENCHMARK_BIN:-}" ]; then
    benchmark_bin=$SOLANA_SHRED_TX_BENCHMARK_BIN
else
    cached_tag=""
    if [ -e "$metadata_file" ] || [ -L "$metadata_file" ]; then
        if [ -L "$metadata_file" ] || [ ! -f "$metadata_file" ] ||
            [ "$(stat -c %u -- "$metadata_file" 2>/dev/null)" != "$cache_uid" ]; then
            printf 'Unsafe cached benchmark metadata.\n' >&2
            exit 1
        fi
        cached_dir=""
        extra=""
        read -r cached_tag cached_dir extra <"$metadata_file" || true
        if ! valid_release_tag "$cached_tag" || [ -n "$extra" ]; then
            printf 'Invalid cached benchmark version: %s\n' "$cached_tag" >&2
            exit 1
        else
            case "$cached_dir" in
                *"/"* | *".."* | *[!A-Za-z0-9._-]*)
                    printf 'Invalid cached benchmark metadata.\n' >&2
                    exit 1
                    ;;
                benchmark."$cached_tag".??????)
                    benchmark_dir="$cache_dir/$cached_dir"
                    benchmark_bin="$benchmark_dir/$BENCHMARK_BINARY_NAME"
                    checksum_file="$benchmark_bin.sha256"
                    if [ -e "$benchmark_dir" ] || [ -L "$benchmark_dir" ]; then
                        if [ -L "$benchmark_dir" ] || [ ! -d "$benchmark_dir" ] ||
                            [ "$(stat -c %u -- "$benchmark_dir" 2>/dev/null)" != "$cache_uid" ]; then
                            printf 'Unsafe cached benchmark directory.\n' >&2
                            exit 1
                        fi
                    fi
                    benchmark_cache_safe || exit 1
                    ;;
                *)
                    printf 'Invalid cached benchmark metadata.\n' >&2
                    exit 1
                    ;;
            esac
        fi
    fi

    latest_tag=""
    if [ -n "$cached_tag" ]; then
        if benchmark_cache_valid; then
            printf 'Using cached solana-shred-tx-benchmark %s.\n' "$cached_tag"
        else
            printf 'Restoring cached solana-shred-tx-benchmark %s...\n' "$cached_tag"
            download_benchmark "$cached_tag"
        fi
    else
        latest_tag=$(resolve_latest_release) || {
            printf 'Unable to resolve the latest benchmark release.\n' >&2
            exit 1
        }
        download_benchmark "$latest_tag"
        cached_tag=$latest_tag
    fi

    if [ -z "$latest_tag" ]; then
        if ! latest_tag=$(resolve_latest_release); then
            printf 'Unable to check for benchmark updates; using cached %s.\n' "$cached_tag" >&2
            latest_tag=""
        fi
    fi
    if [ -n "$latest_tag" ]; then
        cached_patch=${cached_tag#0.1.}
        latest_patch=${latest_tag#0.1.}
        if [ "$latest_patch" -gt "$cached_patch" ]; then
            if [ "$latest_update" = true ]; then
                download_benchmark "$latest_tag"
                cached_tag=$latest_tag
            else
                printf 'Benchmark update available: %s (cached: %s). Run with --latest-update to install it.\n' \
                    "$latest_tag" "$cached_tag"
            fi
        fi
    fi
    chmod +x "$benchmark_bin"
fi

flock -u 9
exec 9>&-

printf 'Starting proxy for %s on %s...\n' "$source_1_name" "$source_1_address"
"$proxy_bin" forward-only \
    --src-bind-addr "$source_1_ip" \
    --src-bind-port "$source_1_port" \
    --dest-ip-ports 127.0.0.1:44444 \
    --grpc-service-port "$grpc_port_1" \
    >"$work_dir/proxy-1.log" 2>&1 &
proxy_pid_1=$!

printf 'Starting proxy for %s on %s...\n' "$source_2_name" "$source_2_address"
"$proxy_bin" forward-only \
    --src-bind-addr "$source_2_ip" \
    --src-bind-port "$source_2_port" \
    --dest-ip-ports 127.0.0.1:44444 \
    --grpc-service-port "$grpc_port_2" \
    >"$work_dir/proxy-2.log" 2>&1 &
proxy_pid_2=$!

sleep 1
for item in "1:$proxy_pid_1" "2:$proxy_pid_2"; do
    proxy_number=${item%%:*}
    proxy_pid=${item#*:}
    if ! kill -0 "$proxy_pid" 2>/dev/null; then
        printf 'Proxy %s failed to start:\n' "$proxy_number" >&2
        cat "$work_dir/proxy-${proxy_number}.log" >&2
        exit 1
    fi
done

"$benchmark_bin" \
    --source-1-url "http://127.0.0.1:$grpc_port_1" \
    --source-1-name "$source_1_name" \
    --source-2-url "http://127.0.0.1:$grpc_port_2" \
    --source-2-name "$source_2_name" \
    --duration "$duration"
