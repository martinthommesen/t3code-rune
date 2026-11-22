#!/usr/bin/env bash
set -euo pipefail

if [[ $# != 3 ]]; then
  printf 'Usage: %s BINARY VERSION OUTPUT.deb\n' "$0" >&2
  exit 2
fi

binary=$(realpath -- "$1")
version=$2
output=$(realpath -m -- "$3")
if [[ ! -x "$binary" || -e "$output" ]]; then
  printf '%s\n' 'Provide a compiled Rune executable and a new package path.' >&2
  exit 2
fi
dpkg --validate-version "$version"

case $(LC_ALL=C readelf -h "$binary" | awk -F ': *' '/Machine:/ {print $2}') in
  'Advanced Micro Devices X86-64') architecture=amd64 ;;
  AArch64) architecture=arm64 ;;
  *) printf '%s\n' 'The binary must be a Linux x86-64 or ARM64 executable.' >&2; exit 2 ;;
esac

resources=$(cd -- "$(dirname -- "$0")" && pwd -P)
root=$(cd -- "$resources/../../../.." && pwd -P)
package_work=$(mktemp -d)
trap 'rm -rf -- "$package_work"' EXIT
umask 022
stage=$package_work/root

install -Dm755 "$binary" "$stage/usr/bin/t3code"
install -Dm644 "$resources/com.t3tools.T3Code.desktop" "$stage/usr/share/applications/com.t3tools.T3Code.desktop"
install -Dm644 "$root/assets/prod/black-universal-1024.png" "$stage/usr/share/icons/hicolor/1024x1024/apps/com.t3tools.T3Code.png"
mkdir -p -- "$stage/usr/share/metainfo"
sed '/<launchable /s/t3code\.desktop/com.t3tools.T3Code.desktop/' \
  "$root/apps/desktop/resources/linux/com.t3tools.t3code.metainfo.xml" \
  > "$stage/usr/share/metainfo/com.t3tools.t3code.metainfo.xml"
install -Dm644 "$root/LICENSE" "$stage/usr/share/doc/t3code/copyright"
install -Dm644 "$root/apps/rune/assets/fonts/Inter-LICENSE.txt" "$stage/usr/share/doc/t3code/Inter-LICENSE.txt"

glibc_version=$(LC_ALL=C readelf --version-info "$binary" \
  | awk 'match($0, /Name: GLIBC_[0-9.]+/) {print substr($0, RSTART + 12, RLENGTH - 12)}' \
  | sort -Vu | tail -n 1)
if [[ -z "$glibc_version" ]]; then
  printf '%s\n' 'Could not identify the executable’s glibc requirement.' >&2
  exit 2
fi

mkdir -- "$stage/DEBIAN"
cat > "$stage/DEBIAN/control" <<EOF
Package: t3code
Version: $version
Section: devel
Priority: optional
Architecture: $architecture
Maintainer: T3 Tools <hello@t3.codes>
Depends: libc6 (>= $glibc_version), libgcc-s1, libwayland-client0, libx11-6, libxcb1, libxcursor1, libxi6, libxrandr2, libxkbcommon0, libxkbcommon-x11-0, libvulkan1, git
Recommends: mesa-vulkan-drivers
Description: Desktop GUI for coding agents
 Native T3 Code desktop application with an embedded orchestration server.
EOF

dpkg-deb --build --root-owner-group "$stage" "$output"
