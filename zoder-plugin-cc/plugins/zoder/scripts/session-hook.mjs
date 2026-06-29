#!/usr/bin/env node
// Lightweight session lifecycle hook. Best-effort, non-fatal: it never blocks
// the session. On SessionStart it verifies the native Zoder binary is reachable
// and emits a one-line notice if it is not, so the user knows to run /zoder:setup.

import { spawnSync } from "node:child_process";

const phase = process.argv[2] || "SessionStart";

function onPath(name) {
  const r = spawnSync("command", ["-v", name], { encoding: "utf8" });
  return r.status === 0 && (r.stdout || "").trim().length > 0;
}

if (phase === "SessionStart") {
  const found =
    (process.env.ZODER_BIN && process.env.ZODER_BIN.trim()) ||
    onPath("zoder") ||
    onPath("zoder");
  if (!found) {
    console.error("[zoder] CLI not found on PATH. Run /zoder:setup to install/configure.");
  }
}

process.exit(0);
