#!/usr/bin/env node
// Runs the two end-to-end drivers against a real sink: starts the built streamtap-sink on a free port with a capture
// folder of its own, runs test-sink.mjs and the live scenarios of test-sink-link.mjs with the fixture, stops the sink.
//
//   node desktop/scripts/test-sink-live.mjs [path to streamtap-sink]    (after cargo build; default target/debug)
//
// fixtures/frag.mp4 is 4 s of test picture and tone, fragmented, made by:
//   ffmpeg -f lavfi -i testsrc=duration=4:size=160x120:rate=10 -f lavfi -i sine=duration=4 -c:v libx264 -g 10 \
//     -c:a aac -b:a 32k -movflags frag_keyframe+empty_moov+default_base_moof frag.mp4
import { spawn, spawnSync } from "node:child_process";
import { randomBytes } from "node:crypto";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const exeName = process.platform === "win32" ? "streamtap-sink.exe" : "streamtap-sink";
const exe = process.argv[2] ?? join(here, "..", "target", "debug", exeName);
const fixture = join(here, "fixtures", "frag.mp4");
for (const file of [exe, fixture]) {
  if (!existsSync(file)) {
    console.error(`test-sink-live: ${file} is missing (build first: cargo build -p streamtap-sink)`);
    process.exit(2);
  }
}

// The origin the drivers send: test-sink-link.mjs takes it as the extension's; the sink's default is the same.
const out = mkdtempSync(join(tmpdir(), "streamtap-sink-live-"));
const token = randomBytes(16).toString("hex");
const sink = spawn(exe, ["serve", "--token-stdin", "--port", "0", "--out", out], {
  stdio: ["pipe", "pipe", "inherit"],
});
sink.stdin.end(`${token}\n`);

const port = await new Promise((resolve, reject) => {
  let seen = "";
  const limit = setTimeout(() => reject(new Error("the sink printed no port within 30 s")), 30_000);
  sink.on("exit", (code) => reject(new Error(`the sink ended early (exit ${code})`)));
  sink.stdout.on("data", (chunk) => {
    seen += chunk;
    const m = seen.match(/ws:\/\/127\.0\.0\.1:(\d+)/);
    if (m) {
      clearTimeout(limit);
      // Keep reading: a sink that blocks on a full pipe would look like a hang of the driver.
      sink.stdout.resume();
      resolve(Number(m[1]));
    }
  });
}).catch((e) => {
  console.error(`test-sink-live: ${e.message}`);
  sink.kill();
  rmSync(out, { recursive: true, force: true });
  process.exit(1);
});
sink.removeAllListeners("exit");

const drivers = [
  ["test-sink.mjs", [fixture, "--token", token, "--port", String(port)]],
  ["test-sink-link.mjs", [fixture, "--token", token, "--captures", out, "--port", String(port)]],
];
let failed = 0;
for (const [script, args] of drivers) {
  console.log(`== ${script}`);
  const run = spawnSync(process.execPath, [join(here, script), ...args], {
    stdio: "inherit",
    timeout: 180_000,
  });
  if (run.status !== 0) {
    console.error(`test-sink-live: ${script} failed (exit ${run.status}${run.error ? `, ${run.error.message}` : ""})`);
    failed += 1;
  }
}

sink.kill();
await new Promise((resolve) => (sink.exitCode === null ? sink.on("exit", resolve) : resolve()));
rmSync(out, { recursive: true, force: true, maxRetries: 5, retryDelay: 200 });
if (failed) process.exit(1);
console.log("test-sink-live: both drivers green");
