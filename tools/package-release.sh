#!/usr/bin/env bash
# Build, version-name, and ZIP ScriptMCP release binaries for every
# target this machine can actually produce.
#
# One Linux zip (native), one Windows zip (MSVC on Windows, mingw
# otherwise), and macOS zips when running on a Mac (or with osxcross).
#
# Usage: tools/package-release.sh [--native-only]

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DIST="$ROOT/dist"
NATIVE_ONLY=0

usage() {
  cat <<'EOF'
Usage: tools/package-release.sh [--native-only]

Build release binaries for every target this host can link, then write
versioned zip files to dist/:

  dist/scriptmcp-<version>-x86_64-linux.zip
  dist/scriptmcp-<version>-x86_64-windows.zip
  dist/scriptmcp-<version>-aarch64-macos.zip

Each zip contains the executable plus LICENSE.md and README.md.

  --native-only   Build only the host OS (skip cross-compilation)
  -h, --help      Show this help
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --native-only) NATIVE_ONLY=1 ;;
    -h|--help) usage; exit 0 ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

cd "$ROOT"

if ! command -v rustc >/dev/null || ! command -v cargo >/dev/null; then
  echo "error: rustc and cargo are required" >&2
  exit 1
fi

VERSION="$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)"
if [ -z "$VERSION" ]; then
  echo "error: could not read version from Cargo.toml" >&2
  exit 1
fi

HOST="$(rustc -vV | awk '/^host:/{print $2}')"
UNAME_S="$(uname -s)"
UNAME_M="$(uname -m)"

echo "ScriptMCP $VERSION"
echo "host: $HOST ($UNAME_S $UNAME_M)"
echo

binary_name() {
  case "$1" in
    *windows*) echo scriptmcp.exe ;;
    *) echo scriptmcp ;;
  esac
}

# Short label used in zip filenames (not the rustc triple).
zip_label() {
  case "$1" in
    x86_64-unknown-linux-gnu) echo x86_64-linux ;;
    aarch64-unknown-linux-gnu) echo aarch64-linux ;;
    x86_64-pc-windows-gnu|x86_64-pc-windows-msvc) echo x86_64-windows ;;
    aarch64-pc-windows-gnu|aarch64-pc-windows-msvc) echo aarch64-windows ;;
    x86_64-apple-darwin) echo x86_64-macos ;;
    aarch64-apple-darwin) echo aarch64-macos ;;
    *) echo "$1" ;;
  esac
}

has_cmd() {
  command -v "$1" >/dev/null 2>&1
}

can_build_apple() {
  if [ "$UNAME_S" = Darwin ]; then
    return 0
  fi
  case "$1" in
    aarch64-apple-darwin) has_cmd oa64-clang || has_cmd aarch64-apple-darwin-clang ;;
    x86_64-apple-darwin) has_cmd o64-clang || has_cmd x86_64-apple-darwin-clang ;;
    *) return 1 ;;
  esac
}

# One Windows target: native MSVC on Windows, otherwise mingw if present.
windows_target() {
  case "$HOST" in
    *-pc-windows-msvc|*-pc-windows-gnu)
      echo "$HOST"
      return 0
      ;;
  esac
  if has_cmd x86_64-w64-mingw32-gcc; then
    echo x86_64-pc-windows-gnu
    return 0
  fi
  return 1
}

linux_target() {
  case "$HOST" in
    *-unknown-linux-gnu)
      echo "$HOST"
      return 0
      ;;
  esac
  return 1
}

target_supported() {
  local target="$1"
  if [ "$target" = "$HOST" ]; then
    return 0
  fi
  case "$target" in
    *-apple-darwin) can_build_apple "$target" ;;
    x86_64-pc-windows-gnu) has_cmd x86_64-w64-mingw32-gcc ;;
    *-pc-windows-msvc) [ "$HOST" = "$target" ] ;;
    *-unknown-linux-gnu) [ "$HOST" = "$target" ] ;;
    *) return 1 ;;
  esac
}

skip_reason() {
  local target="$1"
  case "$target" in
    *-apple-darwin)
      echo "needs macOS or osxcross (o64-clang / oa64-clang)"
      ;;
    *-pc-windows-gnu)
      echo "needs mingw-w64 (e.g. x86_64-w64-mingw32-gcc)"
      ;;
    *-pc-windows-msvc)
      echo "needs Windows with the MSVC toolchain"
      ;;
    *-unknown-linux-gnu)
      echo "Linux builds are native-only for now"
      ;;
    *)
      echo "no linker support on this host"
      ;;
  esac
}

cross_linker() {
  local target="$1"
  if [ "$target" = "$HOST" ]; then
    return 0
  fi
  case "$target" in
    x86_64-pc-windows-gnu) echo x86_64-w64-mingw32-gcc ;;
    x86_64-apple-darwin)
      if has_cmd o64-clang; then echo o64-clang
      elif has_cmd x86_64-apple-darwin-clang; then echo x86_64-apple-darwin-clang
      fi
      ;;
    aarch64-apple-darwin)
      if has_cmd oa64-clang; then echo oa64-clang
      elif has_cmd aarch64-apple-darwin-clang; then echo aarch64-apple-darwin-clang
      fi
      ;;
  esac
}

ensure_rust_target() {
  local target="$1"
  if rustup target list --installed 2>/dev/null | grep -qx "$target"; then
    return 0
  fi
  if has_cmd rustup; then
    echo "  rustup target add $target"
    rustup target add "$target"
  else
    echo "  rustc does not include $target and rustup is not available"
    return 1
  fi
}

cargo_target_dir() {
  if [ -n "${CARGO_TARGET_DIR:-}" ]; then
    case "$CARGO_TARGET_DIR" in
      /*) echo "$CARGO_TARGET_DIR" ;;
      *) echo "$ROOT/$CARGO_TARGET_DIR" ;;
    esac
  else
    echo "$ROOT/target"
  fi
}

locate_binary() {
  local target="$1"
  local bin="$2"
  local dir
  dir="$(cargo_target_dir)"
  if [ -f "$dir/$target/release/$bin" ]; then
    echo "$dir/$target/release/$bin"
    return 0
  fi
  # Host builds sometimes land in target/release when --target is omitted.
  if [ "$target" = "$HOST" ] && [ -f "$dir/release/$bin" ]; then
    echo "$dir/release/$bin"
    return 0
  fi
  return 1
}

write_zip() {
  local src_dir="$1"
  local dest="$2"
  rm -f "$dest"
  if has_cmd zip; then
    (cd "$src_dir" && zip -9 -q "$dest" ./*)
  else
    python3 - "$src_dir" "$dest" <<'PY'
import os, sys, zipfile
src, dest = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(dest, "w", zipfile.ZIP_DEFLATED) as zf:
    for name in sorted(os.listdir(src)):
        zf.write(os.path.join(src, name), name)
PY
  fi
}

package_target() {
  local target="$1"
  local bin
  local built
  local stage
  local zip_path
  local linker
  local label
  bin="$(binary_name "$target")"
  label="$(zip_label "$target")"
  zip_path="$DIST/scriptmcp-${VERSION}-${label}.zip"

  echo "==> $label ($target)"
  if ! ensure_rust_target "$target"; then
    return 1
  fi

  set -- cargo build --release --locked --target "$target"
  linker="$(cross_linker "$target" || true)"
  if [ -n "${linker:-}" ]; then
    set -- "$@" --config "target.${target}.linker=\"${linker}\""
  fi

  echo "  $*"
  if ! "$@"; then
    echo "  build failed"
    return 1
  fi

  if ! built="$(locate_binary "$target" "$bin")"; then
    echo "  error: built binary $bin not found under $(cargo_target_dir)"
    return 1
  fi

  stage="$(mktemp -d "${TMPDIR:-/tmp}/scriptmcp-release.XXXXXX")"
  cp "$built" "$stage/$bin"
  cp "$ROOT/LICENSE.md" "$stage/LICENSE.md"
  cp "$ROOT/README.md" "$stage/README.md"
  write_zip "$stage" "$zip_path"
  rm -rf "$stage"

  echo "  wrote $zip_path ($(wc -c < "$zip_path" | tr -d ' ') bytes)"
}

append_unique() {
  local list="$1"
  local item="$2"
  local existing
  for existing in $list; do
    if [ "$existing" = "$item" ]; then
      echo "$list"
      return 0
    fi
  done
  echo "$list $item"
}

TARGETS=""
if [ "$NATIVE_ONLY" -eq 1 ]; then
  TARGETS="$HOST"
else
  if linux="$(linux_target)"; then
    TARGETS="$(append_unique "$TARGETS" "$linux")"
  fi
  if windows="$(windows_target)"; then
    TARGETS="$(append_unique "$TARGETS" "$windows")"
  fi
  if can_build_apple aarch64-apple-darwin; then
    TARGETS="$(append_unique "$TARGETS" aarch64-apple-darwin)"
  fi
  if can_build_apple x86_64-apple-darwin; then
    TARGETS="$(append_unique "$TARGETS" x86_64-apple-darwin)"
  fi
fi

mkdir -p "$DIST"
rm -f "$DIST"/scriptmcp-"$VERSION"-*.zip

built_list=""
skipped_list=""
failed_list=""

for target in $TARGETS; do
  if ! target_supported "$target"; then
    reason="$(skip_reason "$target")"
    echo "==> $(zip_label "$target") ($target)"
    echo "  skip: $reason"
    echo
    skipped_list="$skipped_list $target"
    continue
  fi
  if package_target "$target"; then
    built_list="$built_list $target"
  else
    failed_list="$failed_list $target"
  fi
  echo
done

echo "----"
if [ -n "$built_list" ]; then
  echo "built:"
  for target in $built_list; do
    echo "  dist/scriptmcp-${VERSION}-$(zip_label "$target").zip"
  done
fi
if [ -n "$skipped_list" ]; then
  echo "skipped:"
  for target in $skipped_list; do
    echo "  $(zip_label "$target")"
  done
fi
if [ -n "$failed_list" ]; then
  echo "failed:"
  for target in $failed_list; do
    echo "  $(zip_label "$target")"
  done
fi

if [ -z "$built_list" ]; then
  echo "error: no release packages were produced" >&2
  exit 1
fi
if [ -n "$failed_list" ]; then
  exit 1
fi
