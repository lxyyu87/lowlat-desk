#!/bin/bash
# Build a universal (arm64 + x86_64) rd-client.app plus 使用说明.txt.
# OpenH264 is statically linked by the openh264 crate. System frameworks
# (VideoToolbox, Metal, libc++) stay dynamic because they ship with macOS.
set -euo pipefail

cd "$(dirname "$0")/../.."

rustup target add aarch64-apple-darwin x86_64-apple-darwin
cargo build --release --locked --target aarch64-apple-darwin -p rd-client
cargo build --release --locked --target x86_64-apple-darwin -p rd-client

stage="dist/lowlat-desk-client-macos"
app="$stage/rd-client.app"
rm -rf "$stage"
mkdir -p "$app/Contents/MacOS"

lipo -create \
    target/aarch64-apple-darwin/release/rd-client \
    target/x86_64-apple-darwin/release/rd-client \
    -output "$app/Contents/MacOS/rd-client"
chmod +x "$app/Contents/MacOS/rd-client"

echo "universal binary:"
lipo -info "$app/Contents/MacOS/rd-client"
lipo -info "$app/Contents/MacOS/rd-client" | grep -q "arm64"
lipo -info "$app/Contents/MacOS/rd-client" | grep -q "x86_64"

cat > "$app/Contents/Info.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>rd-client</string>
    <key>CFBundleDisplayName</key>
    <string>rd-client</string>
    <key>CFBundleIdentifier</key>
    <string>dev.lowlat.rd-client</string>
    <key>CFBundleVersion</key>
    <string>0.1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleExecutable</key>
    <string>rd-client</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

cp .github/instructions/client-macos.txt "$stage/使用说明.txt"

check_deps() {
    local arch="$1"
    local deps dep
    echo "dylibs ($arch):"
    deps="$(otool -arch "$arch" -L "$app/Contents/MacOS/rd-client" | tail -n +2 | awk '{print $1}')"
    while IFS= read -r dep; do
        [ -z "$dep" ] && continue
        echo "  $dep"
        case "$dep" in
            /usr/lib/*|/System/*) ;;
            *)
                echo "unexpected non-system library: $dep" >&2
                echo "static-link it or copy it into the app bundle" >&2
                exit 1
                ;;
        esac
    done <<< "$deps"
}

check_deps arm64
check_deps x86_64
echo "packaged $stage"
