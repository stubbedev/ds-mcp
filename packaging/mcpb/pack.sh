#!/usr/bin/env bash
# Pack one .mcpb bundle (MCP Bundle — https://github.com/modelcontextprotocol/mcpb):
# a zip holding manifest.json plus the prebuilt binary for one platform. Claude
# Desktop installs these in one click, so no client JSON and no PATH fixing.
#
# Usage: packaging/mcpb/pack.sh <binary> <darwin|win32|linux> <out.mcpb>
set -euo pipefail

bin="${1:?binary path}"
platform="${2:?platform: darwin|win32|linux}"
out="${3:?output .mcpb path}"
root="$(cd "$(dirname "$0")/../.." && pwd)"

version="$(grep -m1 '^version = ' "$root/Cargo.toml" | cut -d'"' -f2)"
name="ds-mcp"
[ "$platform" = win32 ] && name="ds-mcp.exe"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

sed -e "s/__VERSION__/${version}/" \
    -e "s/__BIN__/${name}/g" \
    -e "s/__PLATFORM__/${platform}/" \
    "$root/packaging/mcpb/manifest.template.json" > "$tmp/manifest.json"
cp "$bin" "$tmp/$name"
chmod +x "$tmp/$name"
cp "$root/packaging/mcpb/icon.png" "$tmp/icon.png"

# manifest.json must sit at the zip root.
(cd "$tmp" && zip -q -X -r bundle.mcpb manifest.json icon.png "$name")
mkdir -p "$(dirname "$out")"
mv "$tmp/bundle.mcpb" "$out"
echo "$out ($(du -h "$out" | cut -f1), v${version}, ${platform})"
