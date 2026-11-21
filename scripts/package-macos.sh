#!/bin/sh
set -eu

if [ "$(uname -s)" != Darwin ]; then
    printf '%s\n' '请在装有 Xcode Command Line Tools 或 Xcode 的 Mac 上生成 .app。' >&2
    exit 1
fi

cd "$(dirname "$0")/.."
sdk_version=$(xcrun --sdk macosx --show-sdk-version)
printf '使用 macOS %s SDK 构建，最低运行版本为 macOS 27.0。\n' "$sdk_version"

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
