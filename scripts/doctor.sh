#!/usr/bin/env bash
# Downpour environment check.
#
# Reports what is present, what is missing, and what to run to fix it.
# Never installs anything — it prints commands for a human to review and run.
#
# Exit: 0 = every REQUIRED tool present, 1 = something required is missing.

set -uo pipefail

# rustup installs to ~/.cargo/bin and adds it to the interactive shell profile.
# Agent tool calls often run in a NON-interactive shell that never sources that
# profile, so cargo appears missing when it is installed. Put it on PATH here so
# every caller — human, Claude Code, Codex, CI — sees the same environment.
[ -d "$HOME/.cargo/bin" ] && case ":$PATH:" in
  *":$HOME/.cargo/bin:"*) ;;
  *) PATH="$HOME/.cargo/bin:$PATH"; export PATH ;;
esac

RED=$'\033[31m'; GRN=$'\033[32m'; YEL=$'\033[33m'; BLD=$'\033[1m'; RST=$'\033[0m'
[ -t 1 ] || { RED=''; GRN=''; YEL=''; BLD=''; RST=''; }

missing_required=0
missing_apt=()
missing_cargo=()

section() { printf '\n%s%s%s\n' "$BLD" "$1" "$RST"; }

# check <level> <command> <apt-package|-> <cargo-package|-> <why>
check() {
  local level="$1" cmd="$2" apt="$3" crate="$4" why="$5"
  if command -v "$cmd" >/dev/null 2>&1; then
    local ver; ver="$("$cmd" --version 2>/dev/null | head -1 | cut -c1-52)"
    printf '  %s✓%s %-18s %s\n' "$GRN" "$RST" "$cmd" "${ver:-present}"
  else
    if [ "$level" = "required" ]; then
      printf '  %s✗%s %-18s MISSING — %s\n' "$RED" "$RST" "$cmd" "$why"
      missing_required=$((missing_required + 1))
    else
      printf '  %s○%s %-18s optional — %s\n' "$YEL" "$RST" "$cmd" "$why"
    fi
    [ "$apt" != "-" ] && missing_apt+=("$apt")
    [ "$crate" != "-" ] && missing_cargo+=("$crate")
  fi
}

# check_lib <level> <dpkg-package> <why>
check_lib() {
  local level="$1" pkg="$2" why="$3"
  if dpkg -s "$pkg" >/dev/null 2>&1; then
    printf '  %s✓%s %-30s\n' "$GRN" "$RST" "$pkg"
  else
    if [ "$level" = "required" ]; then
      printf '  %s✗%s %-30s MISSING — %s\n' "$RED" "$RST" "$pkg" "$why"
      missing_required=$((missing_required + 1))
    else
      printf '  %s○%s %-30s optional — %s\n' "$YEL" "$RST" "$pkg" "$why"
    fi
    missing_apt+=("$pkg")
  fi
}

printf '%sDownpour environment check%s\n' "$BLD" "$RST"
printf '%s\n' "$(uname -srm) · $(. /etc/os-release 2>/dev/null && echo "$PRETTY_NAME" || echo unknown)"

section "Rust toolchain"
check required rustc      -           - "the entire project is Rust"
check required cargo      -           - "build and test driver"
check required rustup     -           - "toolchain management, MSRV pinning"

section "Build essentials"
check required cc         build-essential - "linking Rust binaries"
check required pkg-config pkg-config  - "locating system libraries at build time"
check required git        git         - "version control"
check optional cmake      cmake       - "some -sys crates build C dependencies"
check optional lld        lld         - "faster linking"
check optional mold       mold        - "much faster linking"

section "Cargo helpers"
check required cargo-nextest -        cargo-nextest "the test runner used by CI"
check optional cargo-deny   -         cargo-deny    "licence and advisory policy (docs/10 §5)"
check optional cargo-audit  -         cargo-audit   "RUSTSEC advisory scanning"
check optional cargo-fuzz   -         cargo-fuzz    "fuzz targets for the IPC and manifest parsers"
check optional cargo-llvm-cov -       cargo-llvm-cov "coverage as a diagnostic"
check optional cargo-machete -        cargo-machete "finds unused dependencies"
check optional cargo-dist   -         cargo-dist    "release matrix (Stage 10)"

section "Project tooling"
check required node       -           - "runs scripts/check-progress.mjs"
check required jq         jq          - "shell-side JSON inspection"
check optional just       just        - "task runner (just corpus, just sim)"
check optional shellcheck shellcheck  - "linting the hook and helper scripts"
check optional hyperfine  hyperfine   - "benchmark timing (docs/09 §5)"
check optional watchexec  -           watchexec-cli "rebuild on change"

section "Test and benchmark infrastructure"
check required openssl    openssl     - "generating test certificates for the corpus server"
check optional curl       curl        - "manual protocol probing"
check optional nghttp     nghttp2-client - "HTTP/2 debugging"
check optional h2load     nghttp2-client - "HTTP/2 and HTTP/3 load generation"
check optional tshark     tshark      - "packet-level protocol debugging"
check optional tc         iproute2    - "network shaping with netem (docs/09 §5.3)"
check optional ffmpeg     ffmpeg      - "HLS/DASH remux and reference comparison (Stage 9)"
check optional podman     podman      - "isolated corpus servers"

section "Browser extension (Stage 7)"
check optional npm        -           - "extension build"
check optional pnpm       -           - "preferred package manager for the extension"

section "System libraries"
check_lib required build-essential "compiler and linker"
check_lib optional libssl-dev      "some crates still prefer system OpenSSL"
check_lib optional libsqlite3-dev  "if rusqlite is not built with the bundled feature"
check_lib optional libwayland-dev  "GUI on Wayland (Stage 10)"
check_lib optional libxkbcommon-dev "keyboard handling for the GUI (Stage 10)"
check_lib optional libx11-dev      "GUI on X11 (Stage 10)"
check_lib optional mingw-w64       "cross-compiling to Windows from Linux"

section "Kernel features"
if modinfo sch_netem >/dev/null 2>&1; then
  printf '  %s✓%s %-18s network impairment available for the benchmark suite\n' "$GRN" "$RST" "sch_netem"
else
  printf '  %s○%s %-18s missing — tc netem shaping will not work\n' "$YEL" "$RST" "sch_netem"
fi

section "Repository state"
if [ -d "$(git rev-parse --show-toplevel 2>/dev/null)/.git" ] 2>/dev/null; then
  printf '  %s✓%s git repository initialised\n' "$GRN" "$RST"
else
  printf '  %s○%s not a git repository yet — run: git init\n' "$YEL" "$RST"
fi
for f in state/progress.json docs/agent/HARNESS.md CLAUDE.md AGENTS.md; do
  if [ -f "$f" ]; then printf '  %s✓%s %s\n' "$GRN" "$RST" "$f"
  else printf '  %s✗%s %s MISSING\n' "$RED" "$RST" "$f"; missing_required=$((missing_required + 1)); fi
done
if command -v node >/dev/null 2>&1 && [ -f scripts/check-progress.mjs ]; then
  if node scripts/check-progress.mjs >/dev/null 2>&1; then
    printf '  %s✓%s state/progress.json validates\n' "$GRN" "$RST"
  else
    printf '  %s✗%s state/progress.json does NOT validate — run: node scripts/check-progress.mjs\n' "$RED" "$RST"
    missing_required=$((missing_required + 1))
  fi
fi

# ---------------------------------------------------------------- remedies

dedupe() { printf '%s\n' "$@" | awk 'NF' | sort -u | tr '\n' ' '; }

if [ ${#missing_apt[@]} -gt 0 ] || [ ${#missing_cargo[@]} -gt 0 ]; then
  section "Suggested install commands"
  if [ ${#missing_apt[@]} -gt 0 ]; then
    printf '  sudo apt update && sudo apt install -y %s\n' "$(dedupe "${missing_apt[@]}")"
  fi
  if ! command -v rustup >/dev/null 2>&1; then
    printf '  curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y\n'
  fi
  if [ ${#missing_cargo[@]} -gt 0 ]; then
    printf '  cargo install %s\n' "$(dedupe "${missing_cargo[@]}")"
  fi
  printf '\n  Review before running. Nothing here is installed automatically.\n'
fi

section "Result"
if [ "$missing_required" -eq 0 ]; then
  printf '  %s✓ every required tool is present%s\n\n' "$GRN" "$RST"
  exit 0
else
  printf '  %s✗ %d required item(s) missing%s\n\n' "$RED" "$missing_required" "$RST"
  exit 1
fi
