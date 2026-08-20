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
#   CLEAN=1   ./scripts/setup-engines-linux.sh  # discard cached CMake trees
#
# The image's CUDA toolkit can be wrong in either direction. Too new and CCCL
# 3.0 (CUDA 13) has dropped Thrust/CUB API the sources still use; too old and
# it cannot emit code for the card at all -- SM 8.6 needs CUDA 11.1 or later,
# for instance. Either way the fix is another toolkit rather than another
# machine, because the driver is backward compatible:
#
#   apt-get install -y cuda-toolkit-12-4
#   export CUDACXX=/usr/local/cuda-12.4/bin/nvcc
#   CLEAN=1 CUDA_ARCH=8.6 ./scripts/setup-engines-linux.sh colmap
#
# CLEAN=1 matters there: CMake caches the compiler it configured with, so a
# changed CUDACXX is ignored until the build tree is discarded.
#
set -euo pipefail

# Bump deliberately. The pipeline parses engine stdout for progress, so a
# major version change can silently break the progress bar without failing
# the run. COLMAP in particular renamed its CLI options between 3.x and 4.x
# and the Rust code passes the 4.x names.
COLMAP_TAG="${COLMAP_TAG:-4.0.4}"
BRUSH_TAG="${BRUSH_TAG:-v0.3.0}"
# Ubuntu 22.04 packages Ceres 2.0, which is older than COLMAP 4.x and GLOMAP
# accept. Building it is cheap next to COLMAP itself.
CERES_TAG="${CERES_TAG:-2.2.0}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENGINES="$ROOT/engines"
# Source checkouts and CMake build trees, which are large and only needed while
# building. Override CACHE to keep them off the disk that holds the installed
# engines -- on hosts that snapshot one disk but not the other, the finished
# binaries belong on the snapshotted one and these do not.
CACHE="${CACHE:-$ROOT/.cache/engines}"
mkdir -p "$CACHE"

log()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[33mwarning: %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[31merror: %s\033[0m\n' "$*" >&2; exit 1; }

# Container images often run as root with no sudo installed at all.
SUDO=""
[ "$(id -u)" -ne 0 ] && SUDO="sudo"

apt_install() {
  if [ "${SKIP_APT:-0}" = "1" ]; then
    warn "SKIP_APT=1, assuming these are present: $*"
    return
  fi
  $SUDO apt-get install -y --no-install-recommends "$@"
}

# Every source build below drives the Ninja generator, and cargo needs a
# linker as well. Installing these from setup_colmap alone was a bug, since
# Ceres builds first and fails at configure time without them.
require_build_tools() {
  apt_install build-essential ninja-build
}

# Clone if absent, and fetch only when the wanted ref is genuinely missing.
# Re-fetching every run makes the script unusable behind a proxy that has to
# be switched on for GitHub and off for the distribution mirrors, since the
# sources are almost always already at the pinned ref.
sync_source() {
  local src="$1" url="$2" ref="${3:-}"
  if [ ! -d "$src/.git" ]; then
    git clone --quiet "$url" "$src"
  elif [ -n "$ref" ] &&
       ! git -C "$src" rev-parse --verify --quiet "${ref}^{commit}" >/dev/null; then
    git -C "$src" fetch --tags --quiet
  fi
  [ -n "$ref" ] && git -C "$src" checkout --quiet "$ref"
  return 0
}

preflight() {
  log "Preflight"
  command -v git   >/dev/null || die "git is required"
  command -v cmake >/dev/null || apt_install cmake
  [ "${SKIP_APT:-0}" = "1" ] || $SUDO apt-get update

  # CMAKE_CUDA_ARCHITECTURES=native needs 3.24. Older CMake still works, but
  # only when CUDA_ARCH names the card explicitly (8.6 for Ampere GA102).
  local cmake_version
  cmake_version="$(cmake --version | head -1 | awk '{print $3}')"
  if [ "${CUDA_ARCH:-native}" = "native" ] &&
     [ "$(printf '%s\n3.24.0\n' "$cmake_version" | sort -V | head -1)" != "3.24.0" ]; then
    die "cmake $cmake_version cannot resolve CUDA_ARCH=native; pass CUDA_ARCH=8.6"
  fi

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

setup_ceres() {
  log "Ceres Solver ${CERES_TAG}"
  # Installed to /usr/local so it lands on the default library search path and
  # nothing downstream needs RPATH handling. Distribution packages of Eigen and
  # glog are fine; only Ceres itself is too old.
  require_build_tools
  apt_install libeigen3-dev libgoogle-glog-dev libgflags-dev libsuitesparse-dev

  local src="$CACHE/ceres"
  sync_source "$src" https://github.com/ceres-solver/ceres-solver.git "$CERES_TAG"
  [ "${CLEAN:-0}" = "1" ] && rm -rf "$src/build"

  cmake -S "$src" -B "$src/build" -GNinja \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_TESTING=OFF \
    -DBUILD_EXAMPLES=OFF \
    -DBUILD_BENCHMARKS=OFF \
    -DCMAKE_INSTALL_PREFIX=/usr/local
  cmake --build "$src/build" --target install
  $SUDO ldconfig
}

setup_colmap() {
  log "COLMAP ${COLMAP_TAG} (CUDA)"
  # No libceres-dev here on purpose: setup_ceres installs a newer Ceres into
  # /usr/local, and pulling the distribution package in as well only invites
  # CMake to resolve against the older one.
  # COLMAP 3.12 moved image IO from FreeImage to OpenImageIO. Both are listed
  # so COLMAP_TAG can be rolled back to a 3.11 release without editing this.
  # openimageio-tools looks redundant next to the -dev package but is not: the
  # -dev CMake targets point at /usr/bin/oiiotool and friends, which ship in
  # the tools package that --no-install-recommends would otherwise skip.
  require_build_tools
  apt_install \
    libboost-program-options-dev libboost-graph-dev libboost-system-dev \
    libeigen3-dev libflann-dev libfreeimage-dev libmetis-dev \
    libopenimageio-dev openimageio-tools \
    libgoogle-glog-dev libgtest-dev libsqlite3-dev libglew-dev \
    qtbase5-dev libqt5opengl5-dev libcgal-dev \
    libcurl4-openssl-dev
  [ -f /usr/local/lib/cmake/Ceres/CeresConfig.cmake ] ||
    die "Ceres not installed yet: run '$0 ceres' first"

  local src="$CACHE/colmap"
  sync_source "$src" https://github.com/colmap/colmap.git "$COLMAP_TAG"
  [ "${CLEAN:-0}" = "1" ] && rm -rf "$src/build"

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
  sync_source "$src" https://github.com/ArthurBrussee/brush.git "$BRUSH_TAG"

  require_build_tools
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
  # Unpinned by default: pin GLOMAP_TAG once a revision is known good on your
  # footage, the same way COLMAP_TAG and BRUSH_TAG are pinned above.
  sync_source "$src" https://github.com/colmap/glomap.git "${GLOMAP_TAG:-}"
  [ "${CLEAN:-0}" = "1" ] && rm -rf "$src/build"

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
  [ ${#targets[@]} -eq 0 ] && targets=(ffmpeg ceres colmap brush)
  preflight
  for target in "${targets[@]}"; do
    case "$target" in
      ffmpeg) setup_ffmpeg ;;
      ceres)  setup_ceres  ;;
      colmap) setup_colmap ;;
      brush)  setup_brush  ;;
      glomap) setup_glomap ;;
      *) die "unknown component: $target (expected ffmpeg, ceres, colmap, brush or glomap)" ;;
    esac
  done
  record_hashes
  log "Done. Verify with:"
  echo "  cargo run --release --bin splatstudio --no-default-features \\"
  echo "    --manifest-path src-tauri/Cargo.toml -- health"
}

main "$@"
