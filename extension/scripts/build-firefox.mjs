#!/usr/bin/env node
// Builds the Firefox (for Android) variant of the extension from the same
// sources as the Chrome one: writes <out>/firefox/ (load it with web-ext or
// about:debugging) and <out>/streamtap-firefox-<version>.xpi (a zip; install it
// unsigned in Firefox Nightly, or have it signed on AMO as unlisted).
//
// The manifest is derived from manifest.json, so the two never drift:
// - background: an event page (background.scripts) instead of a service
//   worker, which Firefox does not run for extensions; the scripts are listed
//   in the order background.js would load them, behind flavor-firefox.js;
// - browser_specific_settings: a fixed add-on id, Firefox 128 or later on
//   desktop and Android (content scripts in the MAIN world);
// - no `key`, `minimum_chrome_version`, `nativeMessaging` or `favicon`
//   (Chrome only; there is no desktop app to pair with on a phone);
// - flavor-firefox.js in front of content.js: the source policy is on.
// The zip is deterministic (fixed dates, sorted entries).
//
// Usage: node extension/scripts/build-firefox.mjs [--out <dir>]   (default: extension/dist)
import { readFileSync, writeFileSync, mkdirSync, rmSync, readdirSync, statSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, relative, resolve, sep } from "node:path";
import zlib from "node:zlib";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..");

export const GECKO_ID = "streamtap@wkdsteve.github.io";
export const GECKO_MIN = "128.0";
const FLAVOR = "src/flavor-firefox.js";
// What the add-on consists of; scripts/, README and build output stay out.
const SHIPPED = ["icons", "popup", "src"];

export const firefoxManifest = (chrome) => {
  const m = structuredClone(chrome);
  delete m.key;
  delete m.minimum_chrome_version;
  m.permissions = (m.permissions || []).filter((p) => p !== "nativeMessaging" && p !== "favicon");
  // background.js loads these with importScripts in Chrome; keep it the single source.
  const bg = readFileSync(join(root, "src", "background.js"), "utf8");
  const imp = /importScripts\(([^)]*)\)/.exec(bg);
  if (!imp) throw new Error("background.js: no importScripts call to read the script order from");
  const imported = [...imp[1].matchAll(/"([^"]+)"/g)].map((x) => "src/" + x[1]);
  m.background = { scripts: [FLAVOR, ...imported, "src/background.js"] };
  m.browser_specific_settings = {
    gecko: { id: GECKO_ID, strict_min_version: GECKO_MIN, data_collection_permissions: { required: ["none"] } },
    gecko_android: { strict_min_version: GECKO_MIN },
  };
  m.content_scripts = m.content_scripts.map((c) =>
    c.world === "MAIN" ? c : { ...c, js: c.js.includes(FLAVOR) ? c.js : [FLAVOR, ...c.js] }
  );
  return m;
};

const walk = (dir) =>
  readdirSync(dir)
    .sort()
    .flatMap((n) => {
      const p = join(dir, n);
      return statSync(p).isDirectory() ? walk(p) : [p];
    });

// A zip of [name, bytes] entries, deflated, with a fixed DOS date (1980-01-01).
export const zip = (entries) => {
  const local = [];
  const central = [];
  let offset = 0;
  for (const [name, data] of entries) {
    const nameBytes = Buffer.from(name, "utf8");
    const packed = zlib.deflateRawSync(data, { level: 9 });
    const crc = zlib.crc32(data);
    const head = Buffer.alloc(30);
    head.writeUInt32LE(0x04034b50, 0);
    head.writeUInt16LE(20, 4); // version needed
    head.writeUInt16LE(0x0800, 6); // UTF-8 names
    head.writeUInt16LE(8, 8); // deflate
    head.writeUInt16LE(0, 10); // time
    head.writeUInt16LE(0x21, 12); // date 1980-01-01
    head.writeUInt32LE(crc, 14);
    head.writeUInt32LE(packed.length, 18);
    head.writeUInt32LE(data.length, 22);
    head.writeUInt16LE(nameBytes.length, 26);
    head.writeUInt16LE(0, 28);
    const dir = Buffer.alloc(46);
    dir.writeUInt32LE(0x02014b50, 0);
    dir.writeUInt16LE(20, 4);
    dir.writeUInt16LE(20, 6);
    dir.writeUInt16LE(0x0800, 8);
    dir.writeUInt16LE(8, 10);
    dir.writeUInt16LE(0, 12);
    dir.writeUInt16LE(0x21, 14);
    dir.writeUInt32LE(crc, 16);
    dir.writeUInt32LE(packed.length, 20);
    dir.writeUInt32LE(data.length, 24);
    dir.writeUInt16LE(nameBytes.length, 28);
    dir.writeUInt32LE(offset, 42);
    local.push(head, nameBytes, packed);
    central.push(dir, nameBytes);
    offset += head.length + nameBytes.length + packed.length;
  }
  const cd = Buffer.concat(central);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(cd.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...local, cd, end]);
};

export const build = (out) => {
  const chrome = JSON.parse(readFileSync(join(root, "manifest.json"), "utf8"));
  const manifest = firefoxManifest(chrome);
  const dir = join(out, "firefox");
  rmSync(dir, { recursive: true, force: true });
  const entries = [["manifest.json", Buffer.from(JSON.stringify(manifest, null, 2) + "\n", "utf8")]];
  for (const top of SHIPPED) {
    for (const p of walk(join(root, top))) entries.push([relative(root, p).split(sep).join("/"), readFileSync(p)]);
  }
  entries.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
  for (const [name, data] of entries) {
    const target = join(dir, ...name.split("/"));
    mkdirSync(dirname(target), { recursive: true });
    writeFileSync(target, data);
  }
  const xpi = join(out, `streamtap-firefox-${manifest.version}.xpi`);
  writeFileSync(xpi, zip(entries));
  return { dir, xpi, manifest, names: entries.map((e) => e[0]) };
};

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const i = process.argv.indexOf("--out");
  const out = resolve(i > 0 && process.argv[i + 1] ? process.argv[i + 1] : join(root, "dist"));
  mkdirSync(out, { recursive: true });
  const r = build(out);
  console.log(`firefox: ${r.dir}`);
  console.log(`xpi:     ${r.xpi} (${r.names.length} files)`);
}
