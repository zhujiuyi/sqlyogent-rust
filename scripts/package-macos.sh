#!/bin/sh
set -eu

if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' '请在装有 Xcode 27 或更高版本的 Mac 上生成 .app。' >&2
    exit 1
fi

cd "$(dirname "$0")/.."
sdk_version=$(xcrun --sdk macosx --show-sdk-version)
sdk_major=$(printf '%s' "$sdk_version" | cut -d. -f1)
if [ "$sdk_major" -lt 27 ]; then
    printf '%s\n' '请选择包含 macOS 27 SDK 的 Xcode。' >&2
    exit 1
fi

target=aarch64-apple-darwin
export MACOSX_DEPLOYMENT_TARGET=27.0
cargo build --locked --release --bins --target "$target"

target_root=$(printenv CARGO_TARGET_DIR || printf '%s' target)
[ -n "$target_root" ] || target_root=target
app='dist/小海豚MysqlGui.app'
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp scripts/HarborSQL-Info.plist "$app/Contents/Info.plist"
install -m 644 assets/dolphin.icns "$app/Contents/Resources/dolphin.icns"
install -m 755 "$target_root/$target/release/harbor-sql" "$app/Contents/MacOS/harbor-sql"
install -m 755 "$target_root/$target/release/harbor-sql-job" "$app/Contents/MacOS/harbor-sql-job"
codesign --force --sign - "$app/Contents/MacOS/harbor-sql-job"
codesign --force --sign - "$app"
printf '%s\n' "$PWD/$app"
