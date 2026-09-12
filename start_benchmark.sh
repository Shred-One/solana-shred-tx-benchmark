#!/bin/sh
set -eu

PROXY_VERSION="v0.2.14"
PROXY_SHA256="1b30b8e3fb4212b30774e9a277c19d42e1ff058d76f444d05afacd2e8f747a70"
PROXY_URL="https://github.com/jito-labs/shredstream-proxy/releases/download/${PROXY_VERSION}/jito-shredstream-proxy-x86_64-unknown-linux-gnu"
BENCHMARK_BINARY_NAME="solana-shred-tx-benchmark-x86_64-unknown-linux-gnu"
BENCHMARK_RELEASE_URL="https://github.com/Shred-One/solana-shred-tx-benchmark/releases/latest/download"

source_1_address=""
source_1_name=""
source_2_address=""
source_2_name=""
duration="60"
grpc_port_1="19091"
grpc_port_2="19092"

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
for command in curl sha256sum; do
    command -v "$command" >/dev/null 2>&1 || {
        printf 'Required command not found: %s\n' "$command" >&2
        exit 1
    }
done
if [ -n "${SOLANA_SHRED_TX_BENCHMARK_BIN:-}" ] && [ ! -x "$SOLANA_SHRED_TX_BENCHMARK_BIN" ]; then
    printf 'Benchmark override is not executable: %s\n' "$SOLANA_SHRED_TX_BENCHMARK_BIN" >&2
    exit 1
fi

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

proxy_bin="$work_dir/jito-shredstream-proxy"
printf 'Downloading Jito ShredStream proxy %s...\n' "$PROXY_VERSION"
curl -fsSL --retry 3 "$PROXY_URL" -o "$proxy_bin"
printf '%s  %s\n' "$PROXY_SHA256" "$proxy_bin" | sha256sum -c -
chmod +x "$proxy_bin"

if [ -n "${SOLANA_SHRED_TX_BENCHMARK_BIN:-}" ]; then
    benchmark_bin=$SOLANA_SHRED_TX_BENCHMARK_BIN
else
    benchmark_bin="$work_dir/$BENCHMARK_BINARY_NAME"
    checksum_file="$benchmark_bin.sha256"
    printf 'Downloading the latest solana-shred-tx-benchmark release...\n'
    curl -fsSL --retry 3 "$BENCHMARK_RELEASE_URL/$BENCHMARK_BINARY_NAME" -o "$benchmark_bin"
    curl -fsSL --retry 3 "$BENCHMARK_RELEASE_URL/$BENCHMARK_BINARY_NAME.sha256" -o "$checksum_file"
    (cd "$work_dir" && sha256sum -c "$BENCHMARK_BINARY_NAME.sha256")
    chmod +x "$benchmark_bin"
fi

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
