#!/bin/sh
# WrongMemory installer (Linux x64, macOS arm64).
#
#   curl -fsSL https://raw.githubusercontent.com/WrongStack/wrongmemory/main/install.sh | sh
#
# Environment:
#   WRONGMEMORY_VERSION         release to install, e.g. 0.1.9 (default: latest)
#   WRONGMEMORY_INSTALL_DIR     install directory (default: $XDG_DATA_HOME/wrongmemory/bin,
#                               else ~/.local/share/wrongmemory/bin)
#   WRONGMEMORY_DOWNLOAD_BASE   mirror serving the release assets (overrides the version URL)
#   WRONGMEMORY_STOP_DAEMON=1   stop a running WrongMemory daemon so its binaries can be replaced
#   WRONGMEMORY_AUTOSTART=1     start the daemon at sign-in (systemd user unit or LaunchAgent)
#   WRONGMEMORY_NO_MODIFY_PATH=1  leave shell startup files untouched
#   WRONGMEMORY_VERIFY_ATTESTATION=1
#                               also verify GitHub build provenance (needs a signed-in `gh`)
#
# Downloads `wrongmemory` and `wrongmemoryd` for this platform, verifies both
# against the release's SHA256SUMS, then runs `wrongmemory install`, which
# copies them into the install directory, writes install.json and keeps the
# previous binaries as *.old. It then puts the install directory on PATH: a
# marked block in ~/.profile and in the current shell's startup file
# (~/.bashrc, ~/.zshrc, or a fish conf.d file), the same block
# `wrongmemory install` writes and `wrongmemory uninstall` removes. It does
# not start the daemon.
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
need chmod
need awk

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

for exe in wrongmemory wrongmemoryd; do
  asset="$exe-$platform"
  printf 'Downloading %s...\n' "$asset"
  fetch "$base/$asset" "$tmp/$asset" || fail "download failed: $base/$asset"
  if ! expected=$(awk -v name="$asset" '
    $2 == name || $2 == "*"name { count++; hash=$1 }
    END { if (count == 1) print hash; else exit 1 }
  ' "$tmp/SHA256SUMS"); then
    fail "SHA256SUMS must contain exactly one entry for $asset"
  fi
  actual=$(sha256 "$tmp/$asset")
  [ "$expected" = "$actual" ] || fail "checksum mismatch for $asset"
  if [ "${WRONGMEMORY_VERIFY_ATTESTATION:-}" = 1 ]; then
    need gh
    gh attestation verify "$tmp/$asset" --repo "$REPO" >/dev/null ||
      fail "build provenance attestation did not verify for $asset"
  fi
  # Only verified bytes become executable, under the name `wrongmemory install` expects.
  chmod 755 "$tmp/$asset"
  mv -f "$tmp/$asset" "$tmp/$exe"
  if [ "$os" = darwin ] && command -v xattr >/dev/null 2>&1; then
    xattr -d com.apple.quarantine "$tmp/$exe" >/dev/null 2>&1 || true
  fi
done
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

set -- install --prefix "$prefix"
[ "${WRONGMEMORY_STOP_DAEMON:-}" = 1 ] && set -- "$@" --stop-daemon
rc=0
"$tmp/wrongmemory" "$@" >"$tmp/install.out" 2>&1 || rc=$?
if [ "$rc" -eq 6 ]; then
  fail "a WrongMemory daemon is running; rerun with WRONGMEMORY_STOP_DAEMON=1 to stop it and replace the binaries"
elif [ "$rc" -ne 0 ]; then
  cat "$tmp/install.out" >&2
  fail "wrongmemory install exited $rc"
fi

version=$("$prefix/wrongmemory" --version 2>/dev/null) || fail "the installed binary could not run: $prefix/wrongmemory"
printf '\nInstalled %s -> %s\n' "$version" "$prefix"

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
printf '  wstack sage service use wrongmemory        # use it as WrongStack'\''s SAGE memory\n'
printf 'Update by running this installer again; remove with: wrongmemory uninstall\n'
