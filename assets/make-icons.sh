#!/usr/bin/env bash
# Render the program icon from its SVG sources, after editing them:
#   icon.ico  every Windows size; build.rs puts it in the .exe
#   icon.png  256 px; the window and taskbar icon, set in src/gui.rs
#
# icon-small.svg is drawn for 16 to 24 px and icon.svg for the rest.
# Needs rsvg-convert (librsvg), ImageMagick 7 and python3.

set -euo pipefail
cd "$(dirname "$0")"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for size in 16 20 24; do
    rsvg-convert -w $size -h $size icon-small.svg -o "$tmp/$size.png"
done
for size in 32 40 48 64 256; do
    rsvg-convert -w $size -h $size icon.svg -o "$tmp/$size.png"
done

magick "$tmp"/{16,20,24,32,40,48,64}.png "$tmp/small.ico"
cp "$tmp/256.png" icon.png

# ImageMagick stores every size as an uncompressed bitmap. Add the 256 px
# size as PNG instead, as Windows allows, which keeps 250 KB out of the .exe.
python3 -I - "$tmp/small.ico" icon.png icon.ico <<'EOF'
import struct, sys

ico, png = (open(path, "rb").read() for path in sys.argv[1:3])
count = struct.unpack_from("<H", ico, 4)[0]
images = []
for i in range(count):
    *head, size, offset = struct.unpack_from("<BBBBHHII", ico, 6 + 16 * i)
    images.append((head, ico[offset:offset + size]))
images.append(([0, 0, 0, 0, 1, 32], png))  # 0 means 256

directory, data = struct.pack("<HHH", 0, 1, len(images)), b""
offset = 6 + 16 * len(images)
for head, image in images:
    directory += struct.pack("<BBBBHHII", *head, len(image), offset + len(data))
    data += image
open(sys.argv[3], "wb").write(directory + data)
EOF
