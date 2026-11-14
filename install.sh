#!/bin/sh
# WrongMemory installer (Linux x64, macOS arm64).
#
#   curl -fsSL https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.sh | sh
#
# Environment:
#   WRONGMEMORY_VERSION         release to install, e.g. 0.2.1 (default: latest)
#   WRONGMEMORY_INSTALL_DIR     install directory (default: $XDG_DATA_HOME/wrongmemory/bin,
#                               else ~/.local/share/wrongmemory/bin)
#   WRONGMEMORY_DOWNLOAD_BASE   mirror serving the release assets (overrides the version URL)
#   WRONGMEMORY_AUTOSTART=1     start the daemon at sign-in (systemd user unit or LaunchAgent)
#   WRONGMEMORY_NO_MODIFY_PATH=1  leave shell startup files untouched
#   WRONGMEMORY_VERIFY_ATTESTATION=1
#                               also verify GitHub build provenance (needs a signed-in `gh`)
#   WRONGMEMORY_DESKTOP=1       install the native GPUI desktop bundle as well (needs `tar`)
#
# Downloads `wrongmemory` and `wrongmemoryd` for this platform, verifies both
# against the release's SHA256SUMS, then runs `wrongmemory install`, which
# copies them into the install directory, writes install.json and keeps the
# previous binaries as *.old. It then puts the install directory on PATH: a
# marked block in ~/.profile and in the current shell's startup file
# (~/.bashrc, ~/.zshrc, or a fish conf.d file), the same block
# `wrongmemory install` writes and `wrongmemory uninstall` removes. A daemon
# that is running is drained and stopped for the install and started again
# from the new binaries; one that is not running is not started.
set -eu

REPO="WrongStack/wrongmemory"

fail() {
  printf 'wrongmemory install: %s\n' "$1" >&2
  exit 1
}

need() {
  command -v "$1" >/dev/null 2>&1 || fail "'$1' is required"
}

need uname
need mkdir
need mktemp
need chmod
need awk
need grep
need sed
need cut

os=$(uname -s)
arch=$(uname -m)
case "$arch" in
  x86_64 | amd64) arch=x64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *) fail "unsupported architecture: $arch" ;;
esac
case "$os" in
  Linux) os=linux ;;
  Darwin)
    os=darwin
    # Rosetta: an x64 shell on Apple silicon still gets the native build.
    if [ "$arch" = x64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || echo 0)" = 1 ]; then
      arch=arm64
    fi
    ;;
  *) fail "unsupported OS: $os (Windows: use install.ps1)" ;;
esac
platform="$os-$arch"
case "$platform" in
  linux-x64 | darwin-arm64) ;;
  *) fail "WrongMemory ships for Linux x64, macOS arm64 and Windows x64; this is $platform" ;;
esac

if [ "$os" = linux ]; then
  # The Linux build links against glibc 2.34 or newer.
  if ls /lib/ld-musl-* >/dev/null 2>&1 || (ldd --version 2>&1 | grep -qi musl); then
    fail "this Linux uses musl libc; the WrongMemory build needs glibc 2.34 or newer"
  fi
  glibc=$( (getconf GNU_LIBC_VERSION 2>/dev/null || true) | awk '{print $2}')
  if [ -n "$glibc" ]; then
    major=${glibc%%.*}
    minor=${glibc#*.}
    minor=${minor%%.*}
    if [ "$major" -lt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -lt 34 ]; }; then
      fail "glibc $glibc is too old; the WrongMemory build needs glibc 2.34 or newer"
    fi
  fi
fi

if [ -n "${WRONGMEMORY_DOWNLOAD_BASE:-}" ]; then
  base="${WRONGMEMORY_DOWNLOAD_BASE%/}"
elif [ -n "${WRONGMEMORY_VERSION:-}" ]; then
  base="https://github.com/${REPO}/releases/download/v${WRONGMEMORY_VERSION#v}"
else
  base="https://github.com/${REPO}/releases/latest/download"
fi

if command -v curl >/dev/null 2>&1; then
  fetch() { curl -fsSL --retry 3 -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  fetch() { wget -q -O "$2" "$1"; }
else
  fail "curl or wget is required"
fi

if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum "$1" | cut -d' ' -f1; }
elif command -v shasum >/dev/null 2>&1; then
  sha256() { shasum -a 256 "$1" | cut -d' ' -f1; }
else
  fail "sha256sum or shasum is required to verify the download"
fi

tmp=$(mktemp -d 2>/dev/null || mktemp -d -t wrongmemory)
trap 'rm -rf "$tmp"' EXIT INT TERM

fetch "$base/SHA256SUMS" "$tmp/SHA256SUMS" || fail "download failed: $base/SHA256SUMS"

# Download one release asset and verify it against SHA256SUMS (and, when
# requested, its build provenance) before anything uses its bytes.
verified() {
  printf 'Downloading %s...\n' "$1"
  fetch "$base/$1" "$2" || fail "download failed: $base/$1"
  if ! expected=$(awk -v name="$1" '
    $2 == name || $2 == "*"name { count++; hash=$1 }
    END { if (count == 1) print hash; else exit 1 }
  ' "$tmp/SHA256SUMS"); then
    fail "SHA256SUMS must contain exactly one entry for $1"
  fi
  actual=$(sha256 "$2")
  [ "$expected" = "$actual" ] || fail "checksum mismatch for $1"
  if [ "${WRONGMEMORY_VERIFY_ATTESTATION:-}" = 1 ]; then
    need gh
    gh attestation verify "$2" --repo "$REPO" >/dev/null ||
      fail "build provenance attestation did not verify for $1"
  fi
}

install_src="$tmp"
desktop_flag=""
if [ "${WRONGMEMORY_DESKTOP:-}" = 1 ]; then
  need tar
  case "$platform" in
    linux-x64) desktop_platform=linux-x64 ;;
    darwin-arm64) desktop_platform=macos-arm64 ;;
  esac
  desktop_asset=$(awk -v suffix="-$desktop_platform.tar.gz" '
    { name = $2; sub(/^\*/, "", name) }
    name ~ /^wrongmemory-desktop-[0-9]+\.[0-9]+\.[0-9]+-/ && substr(name, length(name) - length(suffix) + 1) == suffix { count++; found = name }
    END { if (count == 1) print found; else exit 1 }
  ' "$tmp/SHA256SUMS") ||
    fail "this release has no native desktop bundle for $desktop_platform; unset WRONGMEMORY_DESKTOP to install the CLI and daemon"
  verified "$desktop_asset" "$tmp/$desktop_asset"
  # Refuse absolute paths, parent references and links before extracting.
  tar -tzf "$tmp/$desktop_asset" >"$tmp/bundle.list" || fail "the desktop bundle is not a readable tar.gz"
  if grep -Eq '(^/|(^|/)\.\.(/|$))' "$tmp/bundle.list"; then
    fail "the desktop bundle contains unsafe paths"
  fi
  if tar -tvzf "$tmp/$desktop_asset" | grep -Eq '^[lh]'; then
    fail "the desktop bundle contains links"
  fi
  mkdir "$tmp/bundle"
  tar -xzf "$tmp/$desktop_asset" -C "$tmp/bundle" || fail "extracting the desktop bundle failed"
  # `wrongmemory install --desktop` re-verifies every file against MANIFEST.json.
  install_src="$tmp/bundle/bin"
  desktop_flag="--desktop"
  for exe in wrongmemory wrongmemoryd wrongmemory-desktop; do
    [ -f "$install_src/$exe" ] || fail "the desktop bundle is missing bin/$exe"
    if [ "$os" = darwin ] && command -v xattr >/dev/null 2>&1; then
      xattr -d com.apple.quarantine "$install_src/$exe" >/dev/null 2>&1 || true
    fi
  done
else
  for exe in wrongmemory wrongmemoryd; do
    asset="$exe-$platform"
    verified "$asset" "$tmp/$asset"
    # Only verified bytes become executable, under the name `wrongmemory install` expects.
    chmod 755 "$tmp/$asset"
    mv -f "$tmp/$asset" "$tmp/$exe"
    if [ "$os" = darwin ] && command -v xattr >/dev/null 2>&1; then
      xattr -d com.apple.quarantine "$tmp/$exe" >/dev/null 2>&1 || true
    fi
  done
fi
printf 'Checksums match SHA256SUMS.\n'
[ "${WRONGMEMORY_VERIFY_ATTESTATION:-}" = 1 ] && printf 'Build provenance attestations verified.\n'

if [ -n "${WRONGMEMORY_INSTALL_DIR:-}" ]; then
  prefix="$WRONGMEMORY_INSTALL_DIR"
elif [ -n "${XDG_DATA_HOME:-}" ] && [ "${XDG_DATA_HOME#/}" != "$XDG_DATA_HOME" ]; then
  prefix="$XDG_DATA_HOME/wrongmemory/bin"
else
  [ -n "${HOME:-}" ] || fail "HOME is not set; set WRONGMEMORY_INSTALL_DIR"
  prefix="$HOME/.local/share/wrongmemory/bin"
fi

# --stop-daemon: releases before 0.1.12 refuse to replace a running daemon
# without it; later ones always drain it and start it again themselves.
rc=0
"$install_src/wrongmemory" --json install --prefix "$prefix" --stop-daemon $desktop_flag >"$tmp/install.out" 2>&1 || rc=$?
if [ "$rc" -ne 0 ]; then
  cat "$tmp/install.out" >&2
  fail "wrongmemory install exited $rc"
fi

version=$("$prefix/wrongmemory" --version 2>/dev/null) || fail "the installed binary could not run: $prefix/wrongmemory"
printf '\nInstalled %s -> %s\n' "$version" "$prefix"

if grep -Eq '"daemonStopped": *true' "$tmp/install.out"; then
  # Releases before 0.1.12 stop the daemon without starting it again.
  if grep -Eq '"daemonRestarted": *true' "$tmp/install.out" ||
    "$prefix/wrongmemory" daemon start >/dev/null 2>&1; then
    printf 'The running daemon was stopped for the update and started again from the new binaries.\n'
  else
    printf 'warning: the daemon was stopped for the update and did not start again; run: %s daemon start\n' "$prefix/wrongmemory" >&2
  fi
fi

if [ "${WRONGMEMORY_AUTOSTART:-}" = 1 ]; then
  "$prefix/wrongmemory" autostart enable --prefix "$prefix" >/dev/null ||
    fail "autostart enable failed; run: $prefix/wrongmemory autostart enable"
  printf 'The daemon starts at sign-in (wrongmemory autostart status).\n'
fi

# Keep these blocks byte-identical to crates/wrongmemory-cli/src/user_path.rs,
# so `wrongmemory install` and `uninstall` recognise them.
sh_block() {
  q=$(printf '%s' "$1" | sed "s/'/'\\\\''/g")
  printf '%s\n' '# >>> wrongmemory >>>'
  # shellcheck disable=SC2016 # written literally into the startup file
  printf '%s\n' '# Added by the WrongMemory installer; `wrongmemory uninstall` removes this block.'
  printf "wm_bin='%s'\n" "$q"
  # shellcheck disable=SC2016 # written literally into the startup file
  printf '%s\n' 'case ":$PATH:" in *":$wm_bin:"*) ;; *) PATH="$wm_bin:$PATH"; export PATH ;; esac'
  printf '%s\n' 'unset wm_bin'
  printf '%s\n' '# <<< wrongmemory <<<'
}

fish_file() {
  q=$(printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e "s/'/\\\\'/g")
  # shellcheck disable=SC2016 # written literally into the fish file
  printf '%s\n' '# Added by the WrongMemory installer; `wrongmemory uninstall` removes this file.'
  printf "if not contains -- '%s' \$PATH\n" "$q"
  printf "    set -gx PATH '%s' \$PATH\n" "$q"
  printf '%s\n' 'end'
}

changed=""
add_block() {
  file=$1
  if [ -f "$file" ] && grep -qF '# >>> wrongmemory >>>' "$file"; then
    return 0
  fi
  if [ -s "$file" ]; then
    [ -n "$(tail -c 1 "$file")" ] && printf '\n' >>"$file"
    [ -n "$(tail -n 1 "$file")" ] && printf '\n' >>"$file"
  fi
  sh_block "$prefix" >>"$file" || fail "could not write $file"
  changed="$changed $file"
}

if [ "${WRONGMEMORY_NO_MODIFY_PATH:-}" = 1 ]; then
  case ":$PATH:" in
    *":$prefix:"*) ;;
    *) printf '\n%s is not on your PATH (WRONGMEMORY_NO_MODIFY_PATH=1); add it yourself.\n' "$prefix" ;;
  esac
else
  add_block "$HOME/.profile"
  case "$(basename "${SHELL:-sh}")" in
    bash)
      add_block "$HOME/.bashrc"
      # Login bash reads only the first of these that exists.
      [ -f "$HOME/.bash_profile" ] && add_block "$HOME/.bash_profile"
      ;;
    zsh) add_block "${ZDOTDIR:-$HOME}/.zshrc" ;;
    fish)
      config=${XDG_CONFIG_HOME:-$HOME/.config}
      case "$config" in /*) ;; *) config="$HOME/.config" ;; esac
      fish_conf="$config/fish/conf.d/wrongmemory.fish"
      mkdir -p "$(dirname "$fish_conf")"
      fish_file "$prefix" >"$fish_conf" || fail "could not write $fish_conf"
      changed="$changed $fish_conf"
      ;;
  esac
  if [ -n "$changed" ]; then
    printf '\nAdded %s to PATH in:%s\n' "$prefix" "$changed"
  fi
  case ":$PATH:" in
    *":$prefix:"*) ;;
    *)
      printf 'Open a new terminal, or use it in this one now with:\n'
      # shellcheck disable=SC2016 # $PATH is printed literally for the user to paste.
      printf '  export PATH="%s:$PATH"\n' "$prefix"
      ;;
  esac
fi

printf '\nNext:\n'
printf '  wrongmemory doctor                         # check the install and the daemon\n'
printf '  wrongmemory ui                             # dashboard in the browser\n'
if [ -n "$desktop_flag" ]; then
  printf '  wrongmemory-desktop                        # native desktop (no automatic launch)\n'
fi
printf '  wstack sage service use wrongmemory        # use it as WrongStack'\''s SAGE memory\n'
printf 'Update by running this installer again; remove with: wrongmemory uninstall\n'
