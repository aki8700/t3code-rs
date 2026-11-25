#!/usr/bin/env bash
set -euo pipefail

if [[ "$(uname -s)" != Darwin ]]; then
    echo 'build-macos-app.sh requires macOS' >&2
    exit 1
fi

cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.."
repo="$PWD"
dist="$repo/dist"
destination="$dist/T3 Code RS.app"
mkdir -p "$dist"
lock="$dist/.build.lock"
if ! mkdir "$lock"; then
    echo "Could not acquire build lock at $lock" >&2
    exit 1
fi
staging=
cleanup() {
    if [[ -n "$staging" ]]; then
        rm -rf -- "$staging" || :
    fi
    rmdir -- "$lock"
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
staging="$(mktemp -d "$dist/.t3-macos.XXXXXX")"

bundle="$staging/T3 Code RS.app"
contents="$bundle/Contents"
mkdir -p "$contents/MacOS" "$contents/Resources"

MACOSX_DEPLOYMENT_TARGET=13.0 cargo build --release -p t3-cli -p t3-desktop \
    --message-format=json-render-diagnostics > "$staging/cargo.jsonl"

python3 - "$staging/cargo.jsonl" "$contents/MacOS" <<'PYTHON'
import json
from pathlib import Path
import shutil
import sys

messages, destination = map(Path, sys.argv[1:])
binaries = {"t3code-rs-app", "t3d"}
with messages.open() as log:
    for line in log:
        artifact = json.loads(line)
        if artifact["reason"] != "compiler-artifact" or not artifact.get("executable"):
            continue
        name = artifact["target"]["name"]
        if name in binaries:
            shutil.copy2(artifact["executable"], destination / name)
            binaries.remove(name)
if binaries:
    raise SystemExit(f"Missing release binaries: {', '.join(sorted(binaries))}")
PYTHON

cat > "$contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>
    <string>T3 Code RS</string>
    <key>CFBundleDisplayName</key>
    <string>T3 Code RS</string>
    <key>CFBundleIdentifier</key>
    <string>com.local.t3code-rs.app</string>
    <key>CFBundleExecutable</key>
    <string>t3code-rs-app</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleVersion</key>
    <string>0.1.0</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>LSMinimumSystemVersion</key>
    <string>13.0</string>
    <key>NSAppTransportSecurity</key>
    <dict>
        <key>NSAllowsLocalNetworking</key>
        <true/>
        <key>NSExceptionDomains</key>
        <dict>
            <key>127.0.0.1</key>
            <dict>
                <key>NSExceptionAllowsInsecureHTTPLoads</key>
                <true/>
            </dict>
        </dict>
    </dict>
</dict>
</plist>
PLIST

iconset="$staging/AppIcon.iconset"
mkdir "$iconset"
source_png="$iconset/icon_512x512@2x.png"

python3 - "$source_png" <<'PYTHON'
import math
from pathlib import Path
import struct
import sys
import zlib

SIZE = 1024
CENTER = SIZE / 2
RADIUS = 428
SAMPLES = (0.125, 0.375, 0.625, 0.875)


def square(left, top, right, bottom):
    return [(left, top), (right, top), (right, bottom), (left, bottom)]


def squircle_spans(row, radius):
    spans = []
    for sample in SAMPLES:
        offset = abs(row + sample - CENTER)
        if offset < radius:
            half_width = radius * (1 - (offset / radius) ** 4) ** 0.25
            spans.append((CENTER - half_width, CENTER + half_width))
    return spans


def polygon_spans(row, points):
    spans = []
    for sample in SAMPLES:
        y = row + sample
        intersections = []
        for (x1, y1), (x2, y2) in zip(points, points[1:] + points[:1]):
            if min(y1, y2) <= y < max(y1, y2):
                intersections.append(x1 + (y - y1) * (x2 - x1) / (y2 - y1))
        intersections.sort()
        spans.extend(zip(intersections[::2], intersections[1::2]))
    return spans


def coverage(x, spans):
    return sum(max(0, min(x + 1, right) - max(x, left)) for left, right in spans) / len(SAMPLES)


def columns(spans):
    if not spans:
        return range(0)
    return range(math.floor(min(left for left, _ in spans)),
                 math.ceil(max(right for _, right in spans)))


def paint(row, spans, color):
    for x in columns(spans):
        alpha = coverage(x, spans)
        if alpha:
            for channel, target in enumerate(color):
                index = 4 * x + channel
                row[index] = round(row[index] * (1 - alpha) + target * alpha)


# T and 3 share a cap height and use chamfered corners on the bowls of the 3.
tee = [(246, 350), (492, 350), (492, 414), (402, 414), (402, 658),
       (336, 658), (336, 414), (246, 414)]
three = [(520, 350), (718, 350), (778, 404), (778, 490), (750, 512),
         (778, 536), (778, 604), (718, 658), (520, 658), (520, 594),
         (692, 594), (713, 575), (713, 555), (692, 536), (555, 536),
         (555, 472), (692, 472), (713, 453), (713, 434), (692, 414),
         (520, 414)]
accent = square(423, 709, 601, 728)
ink = [(tee, (249, 249, 251)), (three, (249, 249, 251)), (accent, (232, 93, 42))]

pixels = bytearray()
for y in range(SIZE):
    outer = squircle_spans(y, RADIUS)
    inner = squircle_spans(y, RADIUS - 5)
    highlight = max(0, 1 - (y - (CENTER - RADIUS)) / RADIUS) ** 2
    fill = (13 + 11 * highlight, 13 + 11 * highlight, 16 + 13 * highlight)
    rim = (30 + 17 * highlight, 30 + 17 * highlight, 36 + 19 * highlight)
    row = bytearray(SIZE * 4)
    for x in columns(outer):
        total = coverage(x, outer)
        if total:
            surface = coverage(x, inner) / total
            rgb = [round(a * (1 - surface) + b * surface) for a, b in zip(rim, fill)]
            row[4 * x:4 * x + 4] = bytes([*rgb, round(255 * total)])
    for shape, color in ink:
        paint(row, polygon_spans(y, shape), color)
    pixels.append(0)  # PNG filter type None
    pixels.extend(row)


def chunk(kind, data):
    return (struct.pack(">I", len(data)) + kind + data
            + struct.pack(">I", zlib.crc32(kind + data)))


header = struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0)
Path(sys.argv[1]).write_bytes(
    b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"sRGB", b"\x00")
    + chunk(b"IDAT", zlib.compress(pixels, 9)) + chunk(b"IEND", b"")
)
PYTHON

for size in 16 32 128 256 512; do
    sips -z "$size" "$size" "$source_png" --out "$iconset/icon_${size}x${size}.png" > /dev/null
    if [[ "$size" != 512 ]]; then
        retina=$((size * 2))
        sips -z "$retina" "$retina" "$source_png" --out "$iconset/icon_${size}x${size}@2x.png" > /dev/null
    fi
done
iconutil -c icns "$iconset" -o "$contents/Resources/AppIcon.icns"

/usr/bin/plutil -lint "$contents/Info.plist"
/usr/bin/codesign --force --deep --sign - "$bundle"
/usr/bin/codesign --verify --deep --strict "$bundle"

python3 - "$bundle" "$destination" <<'PYTHON'
import ctypes
import os
import sys

source, destination = sys.argv[1:]
rename = ctypes.CDLL(None, use_errno=True).renamex_np
rename.argtypes = (ctypes.c_char_p, ctypes.c_char_p, ctypes.c_uint)
rename.restype = ctypes.c_int

# macOS <sys/stdio.h>. An atomic exchange keeps the previous app installed
# if the swap fails and moves it into staging for cleanup on success.
RENAME_SWAP = 0x00000002
RENAME_EXCL = 0x00000004
flags = RENAME_SWAP if os.path.lexists(destination) else RENAME_EXCL
if rename(os.fsencode(source), os.fsencode(destination), flags) != 0:
    errno = ctypes.get_errno()
    raise SystemExit(f"Could not install {destination}: {os.strerror(errno)}")
PYTHON
echo "$destination"
