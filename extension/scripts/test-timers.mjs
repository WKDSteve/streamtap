#!/usr/bin/env node
// Every wait in the shipped code has a reason (task streamtap/settimeout-stellen-pruefen). Reads the scripts the
// extension and the app's page ship (not the test scripts) and fails on a setTimeout or setInterval that carries no
// comment naming why it is a timer: on its own line or in the four lines above. The kinds that are fine, each said in
// its comment: a zero delay that runs after the caller set its handlers, a debounce, a timeout of an API that has no
// AbortSignal.timeout (a WebSocket), a fallback where an event is missing, a UI delay. A poll must be paused while
// the window is hidden (checked below for the app's page).
//
// Usage: node extension/scripts/test-timers.mjs   (exit 0 when all checks pass)
import { readFileSync, readdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, relative } from "node:path";

const here = dirname(fileURLToPath(import.meta.url));
const repo = join(here, "..", "..");
const failures = [];
const check = (ok, what) => {
  console.log(`${ok ? "ok  " : "FAIL"} ${what}`);
  if (!ok) failures.push(what);
};

const dirs = [join(repo, "extension", "src"), join(repo, "extension", "popup"), join(repo, "desktop", "crates", "streamtap-app", "ui")];
const files = dirs.flatMap((d) => readdirSync(d).filter((f) => f.endsWith(".js") || f.endsWith(".html")).map((f) => join(d, f)));

/** The timer calls of a source with their line numbers and whether a comment explains them. */
export const timers = (src) => {
  const lines = src.split("\n");
  const found = [];
  lines.forEach((line, i) => {
    const code = line.replace(/\/\/.*$/, "");
    if (!/\b(setTimeout|setInterval)\s*\(/.test(code)) return;
    const near = lines.slice(Math.max(0, i - 4), i + 1);
    const why = near.some((l) => /\/\/\s*\S/.test(l) && !/^\s*\/\/\s*(eslint|prettier)/.test(l));
    found.push({ line: i + 1, why });
  });
  return found;
};

// The detector itself: with and without a reason.
check(timers("setTimeout(f, 5);").length === 1 && !timers("setTimeout(f, 5);")[0].why, "detector: a bare setTimeout has no reason");
check(timers("// debounce: one list a second\nsetTimeout(f, 5);")[0].why, "detector: a comment above is the reason");
check(timers("setInterval(f, 5); // poll while visible")[0].why, "detector: a comment on the line is the reason");
check(timers("const x = 1;\nclearTimeout(t);").length === 0, "detector: clearTimeout is no timer");
check(!timers("// a\n\n\n\n\n\nsetTimeout(f, 5);")[0].why, "detector: a comment far above is no reason");

let total = 0;
for (const file of files) {
  for (const t of timers(readFileSync(file, "utf8"))) {
    total++;
    check(t.why, `${relative(repo, file)}:${t.line} has a comment that says why it waits`);
  }
}
check(total >= 10, `the scan finds the timers of the shipped code (${total})`);

// The page of the app polls only while it is visible.
const app = readFileSync(join(repo, "desktop", "crates", "streamtap-app", "ui", "app.js"), "utf8");
check(/visibilitychange/.test(app) && /clearInterval/.test(app), "app.js: the poll stops when the window is hidden (visibilitychange, clearInterval)");

console.log(failures.length ? `FAILED: ${failures.length} check(s)` : "ok: all checks passed");
process.exit(failures.length ? 1 : 0);
