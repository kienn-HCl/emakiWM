#!/usr/bin/env bash
# emakiwm.svg から emakiwm.ico を生成する。
#   nix shell nixpkgs#imagemagick nixpkgs#librsvg -c ./assets/icon/build-ico.sh
# 16〜64px は BMP、256px は PNG で格納する (256px を BMP にすると ICO が 6 倍以上に膨らむ。
# PNG 格納は Vista 以降の Windows・windres・rc.exe のいずれも扱える)。
set -euo pipefail
cd "$(dirname "$0")"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
sizes=(16 20 24 32 40 48 64 256)
for s in "${sizes[@]}"; do rsvg-convert -w "$s" -h "$s" emakiwm.svg -o "$tmp/$s.png"; done
pngs=(); for s in "${sizes[@]}"; do pngs+=("$tmp/$s.png"); done
magick "${pngs[@]}" "$tmp/bmp.ico"

# bmp.ico のエントリを並べ直し、256px (幅 0) のデータだけ PNG に差し替える
rd() { od -A n -t "u$2" -j "$1" -N "$2" "$tmp/bmp.ico" | tr -d ' '; }
le32() { printf "$(printf '\\x%02x\\x%02x\\x%02x\\x%02x' $(($1 & 255)) $((($1 >> 8) & 255)) $((($1 >> 16) & 255)) $((($1 >> 24) & 255)))"; }
n=$(rd 4 2); off=$((6 + 16 * n)); : > "$tmp/dir"; : > "$tmp/data"
for i in $(seq 0 $((n - 1))); do
  e=$((6 + 16 * i)); w=$(rd "$e" 1); sz=$(rd $((e + 8)) 4); o=$(rd $((e + 12)) 4)
  if [ "$w" = 0 ]; then cp "$tmp/256.png" "$tmp/img"; else dd if="$tmp/bmp.ico" of="$tmp/img" bs=1 skip="$o" count="$sz" status=none; fi
  len=$(stat -c %s "$tmp/img")
  dd if="$tmp/bmp.ico" bs=1 skip="$e" count=8 status=none >> "$tmp/dir"
  le32 "$len" >> "$tmp/dir"; le32 "$off" >> "$tmp/dir"
  cat "$tmp/img" >> "$tmp/data"; off=$((off + len))
done
{ dd if="$tmp/bmp.ico" bs=1 count=6 status=none; cat "$tmp/dir" "$tmp/data"; } > emakiwm.ico
echo "emakiwm.ico: $(stat -c %s emakiwm.ico) bytes"
