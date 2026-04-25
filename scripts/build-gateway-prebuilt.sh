#!/usr/bin/env bash
# Build native/prebuilt TensorZero gateway artifacts without compiling inside Docker.
#
# Common flows:
#   scripts/build-gateway-prebuilt.sh setup
#   scripts/build-gateway-prebuilt.sh mac
#   scripts/build-gateway-prebuilt.sh linux-arm64
#   scripts/build-gateway-prebuilt.sh linux-amd64
#   scripts/build-gateway-prebuilt.sh binaries
#   scripts/build-gateway-prebuilt.sh image-arm64 --tag lobsterpool/tensorzero-gateway:dynamic-api-base
#   scripts/build-gateway-prebuilt.sh image-amd64 --tag registry.example/lobsterpool/tensorzero-gateway:staging --push

set -euo pipefail

cd "$(dirname "$0")/.."

DIST_DIR="${DIST_DIR:-dist}"
GLIBC_VERSION="${GLIBC_VERSION:-2.36}"
PROFILE="${PROFILE:-performance}"
DEFAULT_LOCAL_TAG="${DEFAULT_LOCAL_TAG:-lobsterpool/tensorzero-gateway:dynamic-api-base}"
DEFAULT_AMD64_TAG="${DEFAULT_AMD64_TAG:-lobsterpool/tensorzero-gateway:dynamic-api-base-amd64}"

DARWIN_ARM64_TARGET="aarch64-apple-darwin"
LINUX_ARM64_TARGET="aarch64-unknown-linux-gnu.${GLIBC_VERSION}"
LINUX_AMD64_TARGET="x86_64-unknown-linux-gnu.${GLIBC_VERSION}"

usage() {
    cat <<EOF
Usage:
  $0 setup
  $0 mac
  $0 linux-arm64
  $0 linux-amd64
  $0 binaries
  $0 image-arm64 [--tag TAG] [--push]
  $0 image-amd64 [--tag TAG] [--push]
  $0 images [--tag-prefix PREFIX] [--push]

Commands:
  setup        Install Rust targets and cargo helpers used by this workflow.
  mac          Build native Apple Silicon gateway binary.
  linux-arm64  Build Linux arm64 gateway binary for Docker Desktop / arm64 hosts.
  linux-amd64  Build Linux amd64 gateway binary for production x86_64 hosts.
  binaries     Build mac, linux-arm64, and linux-amd64 binaries.
  image-arm64  Package dist/gateway-linux-arm64 with Dockerfile.prebuilt.
  image-amd64  Package dist/gateway-linux-amd64 with Dockerfile.prebuilt.
  images       Package both Linux images. Use --push for registry output.

Environment:
  PROFILE=${PROFILE}
  GLIBC_VERSION=${GLIBC_VERSION}
  DIST_DIR=${DIST_DIR}
EOF
}

log() {
    printf '\n==> %s\n' "$*"
}

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "$1 is required"
}

base_target() {
    printf '%s\n' "$1" | sed -E 's/\.[0-9]+(\.[0-9]+)?$//'
}

profile_dir() {
    if [ "${PROFILE}" = "dev" ]; then
        printf 'debug\n'
    else
        printf '%s\n' "${PROFILE}"
    fi
}

git_commit() {
    git rev-parse --short HEAD 2>/dev/null || printf 'unknown'
}

setup() {
    need_cmd rustup
    need_cmd cargo

    log "Installing Rust targets"
    rustup target add "${DARWIN_ARM64_TARGET}"
    rustup target add "$(base_target "${LINUX_ARM64_TARGET}")"
    rustup target add "$(base_target "${LINUX_AMD64_TARGET}")"

    log "Installing cargo helpers"
    if ! command -v cargo-zigbuild >/dev/null 2>&1; then
        cargo install cargo-zigbuild --locked
    fi

    if ! command -v zig >/dev/null 2>&1; then
        if command -v brew >/dev/null 2>&1; then
            log "Installing zig with Homebrew"
            brew install zig
        else
            die "zig is missing. Install Zig, or install Homebrew and rerun setup."
        fi
    fi
}

ensure_cargo_prereqs() {
    need_cmd rustup
    need_cmd cargo
}

ensure_zig_prereqs() {
    ensure_cargo_prereqs
    command -v cargo-zigbuild >/dev/null 2>&1 \
        || die "cargo-zigbuild is missing. Run: $0 setup"
    command -v zig >/dev/null 2>&1 \
        || die "zig is missing. Run: $0 setup"
}

build_mac() {
    ensure_cargo_prereqs
    log "Building native macOS arm64 TensorZero gateway"
    (
        cd crates
        SKIP_TSC_VALIDATION=1 cargo build --profile "${PROFILE}" -p gateway
    )
    mkdir -p "${DIST_DIR}"
    cp "crates/target/$(profile_dir)/gateway" "${DIST_DIR}/gateway-darwin-arm64"
    log "Wrote ${DIST_DIR}/gateway-darwin-arm64"
}

build_linux() {
    local arch="$1"
    local target target_base built_binary

    case "${arch}" in
        arm64) target="${LINUX_ARM64_TARGET}" ;;
        amd64) target="${LINUX_AMD64_TARGET}" ;;
        *) die "unknown Linux arch: ${arch}" ;;
    esac

    ensure_zig_prereqs
    log "Building Linux ${arch} TensorZero gateway (${target})"
    (
        cd crates
        SKIP_TSC_VALIDATION=1 cargo zigbuild --profile "${PROFILE}" -p gateway --target "${target}"
    )

    target_base="$(base_target "${target}")"
    built_binary="crates/target/${target_base}/$(profile_dir)/gateway"
    if [ ! -f "${built_binary}" ]; then
        built_binary="crates/target/${target}/$(profile_dir)/gateway"
    fi
    [ -f "${built_binary}" ] || die "built binary was not found for ${target}"
    mkdir -p "${DIST_DIR}"
    cp "${built_binary}" "${DIST_DIR}/gateway-linux-${arch}"
    log "Wrote ${DIST_DIR}/gateway-linux-${arch}"
}

build_image() {
    local arch="$1"
    local tag="$2"
    local output_mode="$3"
    local binary="${DIST_DIR}/gateway-linux-${arch}"

    [ -f "${binary}" ] || die "${binary} is missing. Run: $0 linux-${arch}"
    need_cmd docker

    log "Packaging linux/${arch} image as ${tag}"
    docker buildx build \
        --platform "linux/${arch}" \
        -f crates/gateway/Dockerfile.prebuilt \
        --build-arg "GIT_COMMIT=$(git_commit)" \
        -t "${tag}" \
        "${output_mode}" \
        .
}

parse_image_args() {
    PARSED_TAG="$1"
    PARSED_OUTPUT_MODE="--load"
    shift

    while [ "$#" -gt 0 ]; do
        case "$1" in
            --tag)
                [ "$#" -ge 2 ] || die "--tag requires a value"
                PARSED_TAG="$2"
                shift 2
                ;;
            --push)
                PARSED_OUTPUT_MODE="--push"
                shift
                ;;
            --load)
                PARSED_OUTPUT_MODE="--load"
                shift
                ;;
            *)
                die "unknown argument: $1"
                ;;
        esac
    done
}

parse_images_args() {
    PARSED_TAG_PREFIX="${DEFAULT_LOCAL_TAG%:*}"
    PARSED_OUTPUT_MODE="--load"

    while [ "$#" -gt 0 ]; do
        case "$1" in
            --tag-prefix)
                [ "$#" -ge 2 ] || die "--tag-prefix requires a value"
                PARSED_TAG_PREFIX="$2"
                shift 2
                ;;
            --push)
                PARSED_OUTPUT_MODE="--push"
                shift
                ;;
            --load)
                PARSED_OUTPUT_MODE="--load"
                shift
                ;;
            *)
                die "unknown argument: $1"
                ;;
        esac
    done
}

main() {
    local command="${1:-}"
    [ -n "${command}" ] || {
        usage
        exit 1
    }
    shift || true

    case "${command}" in
        setup)
            setup
            ;;
        mac)
            build_mac
            ;;
        linux-arm64)
            build_linux arm64
            ;;
        linux-amd64)
            build_linux amd64
            ;;
        binaries)
            build_mac
            build_linux arm64
            build_linux amd64
            ;;
        image-arm64)
            parse_image_args "${DEFAULT_LOCAL_TAG}" "$@"
            build_image arm64 "${PARSED_TAG}" "${PARSED_OUTPUT_MODE}"
            ;;
        image-amd64)
            parse_image_args "${DEFAULT_AMD64_TAG}" "$@"
            build_image amd64 "${PARSED_TAG}" "${PARSED_OUTPUT_MODE}"
            ;;
        images)
            parse_images_args "$@"
            build_image arm64 "${PARSED_TAG_PREFIX}:arm64" "${PARSED_OUTPUT_MODE}"
            build_image amd64 "${PARSED_TAG_PREFIX}:amd64" "${PARSED_OUTPUT_MODE}"
            ;;
        -h|--help|help)
            usage
            ;;
        *)
            usage
            die "unknown command: ${command}"
            ;;
    esac
}

main "$@"
