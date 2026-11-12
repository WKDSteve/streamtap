// tools/sheet-mse.mjs -- the app of the test sheet mse-capture (.testing.lua): a real streamtap-sink and a page with an
// MSE player of the fixture TESTS/sheets/fixtures/vp9-frag.mp4, both on the loopback, in a folder of the run.
//
//   node tools/sheet-mse.mjs <run-folder>
//
// Prints the page's address (`http://127.0.0.1:<port>/`) once both are up; ends with its stdin, and takes the sink
// with it. The page is the fixture's own: never a page of somebody else, never DRM (the rule of streamtap).
//   GET /          the player: one SourceBuffer, the fixture appended whole (init and media in one append)
//   GET /media.mp4 the fixture
//   GET /index     every line of the index.jsonl files of the run's captures, as text
// The sink listens on the fixed port SINK_PORT with the fixed token SINK_TOKEN: the sheet stores both in the
// extension (as the popup does), and a loopback sink of a test run guards nothing worth a secret.
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { existsSync, mkdirSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const SINK_PORT = 17651;
export const SINK_TOKEN = 'sheet-mse-capture-token';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const run = process.argv[2];
if (!run) {
  console.error('usage: node tools/sheet-mse.mjs <run-folder>');
  process.exit(2);
}
// the newer of the two builds: a release binary left from an older checkout must not win over the fresh debug build
const exe = ['release', 'debug']
  .map((p) => join(root, 'desktop', 'target', p, process.platform === 'win32' ? 'streamtap-sink.exe' : 'streamtap-sink'))
  .filter(existsSync)
  .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs)[0];
if (!exe) {
  console.error('sheet-mse: streamtap-sink is not built (cargo build -p streamtap-sink in desktop/)');
  process.exit(1);
}
const fixture = readFileSync(join(root, 'TESTS', 'sheets', 'fixtures', 'vp9-frag.mp4'));
const captures = join(run, 'captures');
mkdirSync(captures, { recursive: true });

const PAGE = `<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>streamtap MSE fixture</title></head>
<body>
<video id="v" muted width="160" height="120"></video>
<p id="state">loading</p>
<script>
  const state = (t) => { document.getElementById('state').textContent = t; };
  const ms = new MediaSource();
  document.getElementById('v').src = URL.createObjectURL(ms);
  ms.addEventListener('sourceopen', async () => {
    try {
      const sb = ms.addSourceBuffer('video/mp4; codecs="vp09.00.10.08"');
      const bytes = await (await fetch('/media.mp4')).arrayBuffer();
      sb.addEventListener('updateend', () => { ms.endOfStream(); state('appended'); }, { once: true });
      sb.appendBuffer(bytes);
    } catch (e) {
      state('error: ' + e.message);
    }
  }, { once: true });
</script>
</body></html>
`;

function indexLines() {
  const lines = [];
  for (const dir of readdirSync(captures, { withFileTypes: true })) {
    const file = join(captures, dir.name, 'index.jsonl');
    if (dir.isDirectory() && existsSync(file)) lines.push(readFileSync(file, 'utf8').trim());
  }
  return lines.filter(Boolean).join('\n');
}

const sink = spawn(exe, ['serve', '--token-stdin', '--port', String(SINK_PORT), '--out', captures], {
  stdio: ['pipe', 'pipe', 'inherit'],
});
sink.stdin.end(`${SINK_TOKEN}\n`);
sink.on('exit', (code) => {
  console.error(`sheet-mse: the sink ended (exit ${code})`);
  process.exit(1);
});
await new Promise((done) => {
  let seen = '';
  sink.stdout.on('data', (chunk) => {
    seen += chunk;
    if (seen.includes('listening on')) {
      sink.stdout.resume();
      done();
    }
  });
});

const server = createServer((req, res) => {
  if (req.url === '/') {
    res.writeHead(200, { 'content-type': 'text/html; charset=utf-8' });
    res.end(PAGE);
  } else if (req.url === '/media.mp4') {
    res.writeHead(200, { 'content-type': 'video/mp4' });
    res.end(fixture);
  } else if (req.url === '/index') {
    res.writeHead(200, { 'content-type': 'text/plain; charset=utf-8', 'cache-control': 'no-store' });
    res.end(indexLines());
  } else {
    res.writeHead(404);
    res.end();
  }
});
server.listen(0, '127.0.0.1', () => {
  console.log(`http://127.0.0.1:${server.address().port}/`);
});

// the end of stdin is the end of the run: nothing of it stays behind
process.stdin.resume();
process.stdin.on('end', () => {
  sink.removeAllListeners('exit');
  sink.kill();
  server.close();
  process.exit(0);
});
