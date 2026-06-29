#!/usr/bin/env bash
# PULL-BASED gitops agent. Runs ON each fleet host on a timer.
# 1) git-pull the gitops repo  2) install the latest published binary for this
# host's class+target  3) run apply.sh. Idempotent; only restarts the daemon
# when the installed binary actually changed.
#
# Config: ~/.config/zoder/release.env  (RELEASE_BASE_URL, CHANNEL, CLASS)
#
# BUILDER hosts (roles include `builder`, e.g. cerberus/ultra/jperlow-mlt) may set
# BUILD_FROM_SOURCE=1 to compile the binary set locally from source instead of
# fetching a published artifact. ZC_SRC must point at the zeroclaw fork checkout
# that provides zerocode + zeroclaw (defaults to ~/src/zeroclaw).
set -uo pipefail
# launchd/systemd timers run with a minimal PATH that excludes cargo/rustup and
# Homebrew. Prepend the usual toolchain locations so BUILD_FROM_SOURCE works.
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
REPO="${ZODER_REPO:-$HOME/src/nvzoder}"
GITOPS="$REPO/gitops"
REL="$HOME/.config/zoder/release.env"
LOG="$HOME/.cache/zoder/pull.log"; mkdir -p "$(dirname "$LOG")"
ts(){ date +%Y-%m-%dT%H:%M:%S; }
log(){ echo "[$(ts)] $*" >>"$LOG"; }

[ -f "$REL" ] && . "$REL"
CLASS="${CLASS:-}"; RELEASE_BASE_URL="${RELEASE_BASE_URL:-}"; CHANNEL="${CHANNEL:-stable}"
BUILD_FROM_SOURCE="${BUILD_FROM_SOURCE:-0}"
ZC_SRC="${ZC_SRC:-$HOME/src/zeroclaw}"
BIN="$([ "$CLASS" = zoder ] && echo zoder || echo nvzoder)"
PREFIX="${INSTALL_PREFIX:-$HOME/.local/bin}"; mkdir -p "$PREFIX"

# class auto-detect if unset
if [ -z "$CLASS" ]; then
  if [ -d "$HOME/.zoder" ]; then CLASS=zoder; BIN=zoder; else CLASS=nvzoder; BIN=nvzoder; fi
fi

# Optional: sync deployment config overlays from a private config repo into
# $ZODER_HOME. Set CONFIG_REPO (in release.env) to a git URL to enable. The code
# repo stays config-free and the URL never lives in the public source — only in
# this host's release.env. Filled config.<org>.toml overlays are versioned in
# that repo, not here, so a code import never clobbers them.
ZODER_HOME="${ZODER_HOME:-$HOME/.zoder}"
if [ -n "${CONFIG_REPO:-}" ]; then
  cfgdir="$HOME/.cache/zoder/config-repo"
  if [ -d "$cfgdir/.git" ]; then
    git -C "$cfgdir" pull --ff-only >>"$LOG" 2>&1 || log "config-repo pull failed (using cached tree)"
  else
    git clone --depth 1 "$CONFIG_REPO" "$cfgdir" >>"$LOG" 2>&1 || log "config-repo clone failed"
  fi
  if ls "$cfgdir"/config.*.toml >/dev/null 2>&1; then
    mkdir -p "$ZODER_HOME"
    cp "$cfgdir"/config.*.toml "$ZODER_HOME"/ && log "synced config overlays from CONFIG_REPO -> $ZODER_HOME"
  fi
fi

# target triple for this host
host_target(){
  local os arch; os="$(uname -s)"; arch="$(uname -m)"
  case "$os:$arch" in
    Darwin:arm64)  echo aarch64-apple-darwin;;
    Darwin:x86_64) echo x86_64-apple-darwin;;
    Linux:x86_64)  echo x86_64-unknown-linux-musl;;     # musl = portable
    Linux:aarch64) echo aarch64-unknown-linux-musl;;
    *) echo unknown;;
  esac
}
TARGET="$(host_target)"

log "pull start class=$CLASS bin=$BIN target=$TARGET channel=$CHANNEL"

# 1) update gitops tree
if [ -d "$REPO/.git" ]; then
  git -C "$REPO" pull --ff-only >>"$LOG" 2>&1 || log "git pull failed (continuing with cached tree)"
fi

# install helper: copy into PREFIX only when content changed; set CHANGED on update
install_if_changed(){ # <src-binary> <name>
  local src="$1" name="$2"
  [ -f "$src" ] || { log "build missing artifact: $src"; return 1; }
  if ! cmp -s "$src" "$PREFIX/$name" 2>/dev/null; then
    install -m 0755 "$src" "$PREFIX/$name" && { log "installed new $name (built)"; CHANGED=1; }
  else
    log "$name already up to date"
  fi
}

# 2) acquire binaries — BUILD mode (self-compile) takes precedence on builder hosts
if [ "$BUILD_FROM_SOURCE" = 1 ]; then
  command -v cargo >/dev/null 2>&1 || { log "BUILD_FROM_SOURCE=1 but cargo not found; aborting build"; exit 3; }
  # pull zeroclaw fork (zerocode + zeroclaw) if present
  [ -d "$ZC_SRC/.git" ] && { git -C "$ZC_SRC" pull --ff-only >>"$LOG" 2>&1 || log "zc-src git pull failed (using cached tree)"; }
  log "build start: $BIN (REPO=$REPO) + zerocode/zeroclaw (ZC_SRC=$ZC_SRC)"
  if cargo build --release --manifest-path "$REPO/Cargo.toml" --bin "$BIN" >>"$LOG" 2>&1; then
    install_if_changed "$REPO/target/release/$BIN" "$BIN"
  else
    log "build FAILED: $BIN (keeping existing)"
  fi
  # zerocode lives in the `zerocode` package; the `zeroclaw` daemon bin lives in
  # the root `zeroclawlabs` package, so they need separate cargo invocations.
  # ZC_SRC must be a CLEAN dedicated fork checkout (a dirty working tree would
  # ship un-reviewed WIP). Skip gracefully when unset/missing.
  if [ -z "$ZC_SRC" ] || [ ! -d "$ZC_SRC" ]; then
    log "zerocode/zeroclaw build skipped (set ZC_SRC to a clean fork checkout to enable): '${ZC_SRC:-}'"
  elif [ -n "$(git -C "$ZC_SRC" status --porcelain 2>/dev/null)" ]; then
    log "zerocode/zeroclaw build skipped: ZC_SRC ($ZC_SRC) has a dirty working tree"
  elif cargo build --release --manifest-path "$ZC_SRC/Cargo.toml" -p zerocode >>"$LOG" 2>&1 \
     && cargo build --release --manifest-path "$ZC_SRC/Cargo.toml" --bin zeroclaw >>"$LOG" 2>&1; then
    install_if_changed "$ZC_SRC/target/release/zerocode" zerocode
    install_if_changed "$ZC_SRC/target/release/zeroclaw" zeroclaw
  else
    log "build FAILED: zerocode/zeroclaw (keeping existing)"
  fi
# install latest published binary (if a release base is configured)
elif [ -n "$RELEASE_BASE_URL" ] && [ "$TARGET" != unknown ]; then
  url="$RELEASE_BASE_URL/$CHANNEL/$BIN-$TARGET"
  tmp="$(mktemp)"
  if curl -fsSL "$url" -o "$tmp" 2>>"$LOG"; then
    if ! cmp -s "$tmp" "$PREFIX/$BIN" 2>/dev/null; then
      chmod +x "$tmp"; mv -f "$tmp" "$PREFIX/$BIN"
      log "installed new $BIN from $url"; CHANGED=1
    else
      rm -f "$tmp"; log "$BIN already up to date"
    fi
  else
    rm -f "$tmp"; log "download failed: $url (keeping existing $BIN)"
  fi
else
  log "no RELEASE_BASE_URL set or unknown target; skipping binary install"
fi

# 3) apply config + restart. Workstation/binaries-only hosts set APPLY_CONFIG=0
# to PRESERVE a hand-tuned live config (apply.sh would re-render it from the
# fleet template). In that mode we still restart the daemon when the binary
# changed, leaving the config untouched.
if [ "${APPLY_CONFIG:-1}" = 0 ]; then
  log "config apply skipped (APPLY_CONFIG=0; preserving live config)"
  if [ "${CHANGED:-0}" = 1 ]; then
    if [ "$(uname -s)" = Darwin ]; then
      lbl="${DAEMON_LABEL:-com.nclawzero.zeroclaw-gateway}"
      launchctl kickstart -k "gui/$(id -u)/$lbl" >>"$LOG" 2>&1 \
        && log "daemon restarted ($lbl, new binary)" || log "daemon restart failed ($lbl)"
    else
      svc="${DAEMON_SERVICE:-zoder-daemon.service}"
      systemctl --user restart "$svc" >>"$LOG" 2>&1 && log "daemon restarted ($svc)" || log "daemon restart failed ($svc)"
    fi
  else
    log "no binary change; daemon left running"
  fi
else
  restart_flag="--no-restart"; [ "${CHANGED:-0}" = 1 ] && restart_flag=""
  bash "$GITOPS/scripts/apply.sh" --class "$CLASS" $restart_flag >>"$LOG" 2>&1 \
    && log "apply ok" || log "apply FAILED"
fi
log "pull done"
