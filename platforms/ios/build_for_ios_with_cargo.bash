#!/usr/bin/env bash
set -euo pipefail

export PATH="/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:$PATH:$HOME/.cargo/bin"

case "${CONFIGURATION:-Release}" in
  Debug)
    cargo_profile=debug
    cargo_args=()
    ;;
  *)
    cargo_profile=release
    cargo_args=(--release)
    ;;
esac

target=aarch64-apple-ios
if [[ "${LLVM_TARGET_TRIPLE_SUFFIX:-}" == "-simulator" ]]; then
  target=aarch64-apple-ios-sim
fi

repo_root="${PROJECT_DIR}/../.."
cd "$repo_root"

cargo build \
  --locked \
  --package friendshub-ios \
  --target "$target" \
  "${cargo_args[@]}"

source_binary="${repo_root}/target/${target}/${cargo_profile}/friendshub-ios"
destination_binary="${TARGET_BUILD_DIR}/${EXECUTABLE_PATH}"
mkdir -p "$(dirname "$destination_binary")"
cp "$source_binary" "$destination_binary"
chmod 755 "$destination_binary"

if [[ -n "${DWARF_DSYM_FOLDER_PATH:-}" && -n "${DWARF_DSYM_FILE_NAME:-}" ]]; then
  mkdir -p "$DWARF_DSYM_FOLDER_PATH"
  dsymutil "$destination_binary" -o "$DWARF_DSYM_FOLDER_PATH/$DWARF_DSYM_FILE_NAME"
fi

if [[ "$target" == "aarch64-apple-ios" && "${CODE_SIGNING_ALLOWED:-NO}" != "NO" && -n "${EXPANDED_CODE_SIGN_IDENTITY:-}" ]]; then
  codesign --force --sign "$EXPANDED_CODE_SIGN_IDENTITY" "$destination_binary"
fi
