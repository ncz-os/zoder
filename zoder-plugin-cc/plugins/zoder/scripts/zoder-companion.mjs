#!/usr/bin/env node
// Zoder companion: a thin bridge from Claude Code slash commands to the native
// `zoder` / `zoder` CLI. It auto-detects the binary, maps the plugin
// subcommand to the matching native subcommand, forwards arguments, and streams
// the native stdout/stderr back verbatim. No vendor lock-in: works with either
// the internal (zoder) or public (zoder) build.

import { spawnSync, spawn } from "node:child_process";

// ---- binary auto-detect ----------------------------------------------------
function onPath(name) {
  const r = spawnSync(process.platform === "win32" ? "where" : "command", [
    process.platform === "win32" ? name : "-v",
    process.platform === "win32" ? "" : name,
  ].filter(Boolean), { encoding: "utf8" });
  return r.status === 0 && (r.stdout || "").trim().length > 0;
}

function detectBin() {
  if (process.env.ZODER_BIN && process.env.ZODER_BIN.trim()) {
    return process.env.ZODER_BIN.trim();
  }
  for (const cand of ["zoder", "zoder"]) {
    if (onPath(cand)) return cand;
  }
  return null;
}

// ---- minimal shell-like arg tokenizer --------------------------------------
function tokenize(raw) {
  if (!raw) return [];
  const out = [];
  let cur = "";
  let quote = null;
  for (let i = 0; i < raw.length; i++) {
    const c = raw[i];
    if (quote) {
      if (c === quote) quote = null;
      else cur += c;
    } else if (c === '"' || c === "'") {
      quote = c;
    } else if (/\s/.test(c)) {
      if (cur) { out.push(cur); cur = ""; }
    } else {
      cur += c;
    }
  }
  if (cur) out.push(cur);
  return out;
}

// ---- subcommand mapping ----------------------------------------------------
// plugin subcommand -> native subcommand
const MAP = {
  review: "review",
  "adversarial-review": "adversarial-review",
  task: "rescue",
  rescue: "rescue",
  status: "status",
  result: "result",
  cancel: "cancel",
  transfer: "transfer",
  exec: "exec",
};

function runNative(bin, args, { background = false } = {}) {
  if (background) {
    // Detach: native `--background` already returns a job id and forks a worker,
    // so we just run it foreground here and let it print the id.
  }
  const child = spawn(bin, args, { stdio: "inherit" });
  child.on("exit", (code) => process.exit(code ?? 0));
  child.on("error", (e) => {
    console.error(`zoder companion: failed to run ${bin}: ${e.message}`);
    process.exit(127);
  });
}

function main() {
  const sub = process.argv[2];
  const raw = process.argv[3] || "";
  const tokens = tokenize(raw);

  // Helper used by the rescue command to decide resume prompting. We do not
  // track Claude-session threads, so always report none available.
  if (sub === "task-resume-candidate") {
    if (tokens.includes("--json")) {
      console.log(JSON.stringify({ available: false }));
    } else {
      console.log("available: false");
    }
    return;
  }

  const bin = detectBin();

  if (sub === "setup") {
    if (!bin) {
      console.log(
        "zoder/zoder not found on PATH.\n" +
          "Install the matching build (it ships zerocode + zeroclaw alongside),\n" +
          "then set $ZODER_BIN or put the binary on PATH and re-run /zoder:setup.\n" +
          "Public build install:\n" +
          "  curl -fsSL https://gitlab.com/ncz-os/zoder/-/raw/main/install.sh | sh"
      );
      process.exit(1);
    }
    console.log(`Found native binary: ${bin}`);
    const r = spawnSync(bin, ["configure"], { stdio: "inherit" });
    process.exit(r.status ?? 0);
  }

  if (!bin) {
    console.error(
      "zoder companion: neither `zoder` nor `zoder` found on PATH. Run /zoder:setup."
    );
    process.exit(127);
  }

  const native = MAP[sub];
  if (!native) {
    console.error(`zoder companion: unknown subcommand '${sub}'`);
    process.exit(2);
  }

  // `--wait` is a Claude-side execution hint; native CLIs do not know it.
  const filtered = tokens.filter((t) => t !== "--wait");
  const background = filtered.includes("--background");

  // Reviews: emit human-readable verdict by default; callers can add --json.
  const args = [native, ...filtered];
  runNative(bin, args, { background });
}

main();
