#!/usr/bin/env bash
#
# Provision the native engines OOOSplat drives, in the layout that
# EnginePaths::from_root expects:
#
#   engines/ffmpeg/ffmpeg      engines/ffmpeg/ffprobe
#   engines/colmap/bin/colmap  engines/brush/brush_app
#   engines/glomap/bin/glomap  (optional, --mapper glomap)
#
# This is deliberately not a port of setup-engines.ps1. That script restores
# pinned archives verified against manifest.json, which works on Windows
# because every engine publishes a prebuilt zip. On Linux, COLMAP publishes
# no binary release and its CUDA build has to be compiled against the local
# toolkit, so COLMAP and Brush are built from source instead.
#
# Usage:
#   ./scripts/setup-engines-linux.sh            # ffmpeg, colmap, brush
#   ./scripts/setup-engines-linux.sh colmap     # one component
#   ./scripts/setup-engines-linux.sh glomap     # optional mapper backend
#   SKIP_APT=1 ./scripts/setup-engines-linux.sh # no package installs
#
set -euo pipefail

# Bump deliberately. The pipeline parses engine stdout for progress, so a
# major version change can silently break the progress bar without failing
# the run. COLMAP in particular renamed its CLI options between 3.x and 4.x
# and the Rust code passes the 4.x names.
COLMAP_TAG="${COLMAP_TAG:-4.0.4}"
BRUSH_TAG="${BRUSH_TAG:-v0.3.0}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENGINES="$ROOT/engines"
CACHE="$ROOT/.cache/engines"
mkdir -p "$CACHE"

log()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33mwarning: %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[31merror: %s\033[0m\n' "$*" >&2; exit 1; }

apt_install() {
  if [ "${SKIP_APT:-0}" = "1" ]; then
    warn "SKIP_APT=1, assuming these are present: $*"
    return
  fi
  sudo apt-get install -y --no-install-recommends "$@"
}

preflight() {
  log "Preflight"
  command -v git   >/dev/null || die "git is required"
  command -v cmake >/dev/null || apt_install cmake
  [ "${SKIP_APT:-0}" = "1" ] || sudo apt-get update

  if ! command -v nvidia-smi >/dev/null; then
    warn "nvidia-smi not found. COLMAP will still build, but --compute gpu"
    warn "will fail at runtime and Brush has no GPU backend to train on."
  else
    nvidia-smi --query-gpu=name,driver_version,memory.total --format=csv,noheader
  fi

  if ! command -v nvcc >/dev/null; then
    warn "nvcc not found. Install the CUDA toolkit before building COLMAP,"
    warn "e.g. 'sudo apt-get install nvidia-cuda-toolkit', or use NVIDIA's"
    warn "repository for a newer release. Without it COLMAP builds CPU-only"
    warn "and the whole point of the port is lost."
  else
    nvcc --version | tail -2
  fi
}

setup_ffmpeg() {
  log "FFmpeg / FFprobe"
  mkdir -p "$ENGINES/ffmpeg"
  # The distribution build is enough here: the pipeline only calls ffmpeg for
  # uniform frame extraction and parses "frame=N" from -progress, which has
  # been stable for many releases. Swap in a pinned build if you ever need
  # byte-identical output across machines.
  command -v ffmpeg >/dev/null || apt_install ffmpeg
  ln -sf "$(command -v ffmpeg)"  "$ENGINES/ffmpeg/ffmpeg"
  ln -sf "$(command -v ffprobe)" "$ENGINES/ffmpeg/ffprobe"
  "$ENGINES/ffmpeg/ffmpeg" -version | head -1
}

setup_colmap() {
  log "COLMAP ${COLMAP_TAG} (CUDA)"
  apt_install \
    build-essential ninja-build \
    libboost-program-options-dev libboost-graph-dev libboost-system-dev \
    libeigen3-dev libflann-dev libfreeimage-dev libmetis-dev \
    libgoogle-glog-dev libgtest-dev libsqlite3-dev libglew-dev \
    qtbase5-dev libqt5opengl5-dev libcgal-dev libceres-dev \
    libcurl4-openssl-dev

  local src="$CACHE/colmap"
  if [ -d "$src/.git" ]; then
    git -C "$src" fetch --tags --quiet
  else
    git clone --quiet https://github.com/colmap/colmap.git "$src"
  fi
  git -C "$src" checkout --quiet "$COLMAP_TAG"

  # CMAKE_CUDA_ARCHITECTURES=native compiles only for the GPU in this machine,
  # which keeps the build short. Use "all-major" instead if the binary has to
  # run on other cards.
  cmake -S "$src" -B "$src/build" -GNinja \
    -DCMAKE_BUILD_TYPE=Release \
    -DCUDA_ENABLED=ON \
    -DCMAKE_CUDA_ARCHITECTURES="${CUDA_ARCH:-native}" \
    -DGUI_ENABLED=OFF \
    -DCMAKE_INSTALL_PREFIX="$ENGINES/colmap"
  cmake --build "$src/build" --target install

  # health.rs reads this line, and require_colmap_policy rejects the build for
  # --compute gpu if it reports "without CUDA".
  "$ENGINES/colmap/bin/colmap" -h 2>&1 | head -1
}

setup_brush() {
  log "Brush ${BRUSH_TAG}"
  command -v cargo >/dev/null || die "cargo is required; install rustup first"
  mkdir -p "$ENGINES/brush"

  local src="$CACHE/brush"
  if [ -d "$src/.git" ]; then
    git -C "$src" fetch --tags --quiet
  else
    git clone --quiet https://github.com/ArthurBrussee/brush.git "$src"
  fi
  git -C "$src" checkout --quiet "$BRUSH_TAG"

  # Brush renders through wgpu, which uses Vulkan on Linux.
  apt_install libvulkan1 vulkan-tools mesa-vulkan-drivers
  ( cd "$src" && cargo build --release --bin brush_app )
  install -m 755 "$src/target/release/brush_app" "$ENGINES/brush/brush_app"
  "$ENGINES/brush/brush_app" --help | head -3
}

setup_glomap() {
  log "GLOMAP (optional mapper backend)"
  # GLOMAP links against the COLMAP libraries, so that has to exist first.
  [ -x "$ENGINES/colmap/bin/colmap" ] || die "build COLMAP first: $0 colmap"

  local src="$CACHE/glomap"
  if [ -d "$src/.git" ]; then
    git -C "$src" fetch --tags --quiet
  else
    git clone --quiet https://github.com/colmap/glomap.git "$src"
  fi
  # Unpinned by default: pin GLOMAP_TAG once a revision is known good on your
  # footage, the same way COLMAP_TAG and BRUSH_TAG are pinned above.
  [ -n "${GLOMAP_TAG:-}" ] && git -C "$src" checkout --quiet "$GLOMAP_TAG"

  cmake -S "$src" -B "$src/build" -GNinja \
    -DCMAKE_BUILD_TYPE=Release \
    -DCMAKE_PREFIX_PATH="$ENGINES/colmap" \
    -DCMAKE_CUDA_ARCHITECTURES="${CUDA_ARCH:-native}" \
    -DCMAKE_INSTALL_PREFIX="$ENGINES/glomap"
  cmake --build "$src/build" --target install

  "$ENGINES/glomap/bin/glomap" mapper -h 2>&1 | head -3
}

record_hashes() {
  log "Recorded hashes"
  # The Windows flow pins these in manifest.json before packaging. Here the
  # binaries are built locally, so just record what was produced -- rerun this
  # after any rebuild to see whether an engine actually changed.
  ( cd "$ROOT" && sha256sum \
      engines/colmap/bin/colmap \
      engines/brush/brush_app \
      engines/glomap/bin/glomap 2>/dev/null ) || true
}

main() {
  local targets=("$@")
  # GLOMAP is not in the default set: it is optional, and building it means
  # building COLMAP first.
  [ ${#targets[@]} -eq 0 ] && targets=(ffmpeg colmap brush)
  preflight
  for target in "${targets[@]}"; do
    case "$target" in
      ffmpeg) setup_ffmpeg ;;
      colmap) setup_colmap ;;
      brush)  setup_brush  ;;
      glomap) setup_glomap ;;
      *) die "unknown component: $target (expected ffmpeg, colmap, brush or glomap)" ;;
    esac
  done
  record_hashes
  log "Done. Verify with:"
  echo "  cargo run --release --bin splatstudio --no-default-features \\"
  echo "    --manifest-path src-tauri/Cargo.toml -- health"
}

main "$@"
