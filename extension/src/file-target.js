// The target "file on the device" (the mobile build, no sink): a link of sink-link.js gets a recorder in place of
// its WebSocket. The recorder takes exactly the messages the link would send to the sink (hello, the init segments in
// effect, every segment, initlost and other events) and keeps them, each with its length in front, as one recording
// per MediaSource. When the link closes (the video or the tab ends, or the popup saves now) the recording is saved
// through the downloads API as `streamtap-<time>-<host>.streamtap`. On the desktop `streamtap-sink replay <file>`
// feeds it through the sink's own code (DRM refusal, init assembly, duplicates) into a capture directory, and
// `streamtap-sink mux` makes the video, as for a live capture; gaps and lost inits are said there (MOB-09).
//
// Layout of a recording: the line `STREAMTAP-RECORDING/1\n`, then per message a 32-bit little-endian length and the
// message as the sink takes it (32-bit little-endian header length, JSON header, payload).
//
// Bounded (MSEC-08): all open recordings of the worker together hold at most FILE_MAX_TOTAL bytes. Beyond that a
// recording takes no more media (init bytes and events still, up to twice the bound, so what it holds stays usable),
// notes a `recordfull` event once at that point, and the popup says so. A recording without any media is not saved.
"use strict";

const FILE_MAGIC = "STREAMTAP-RECORDING/1\n";
const FILE_MAX_TOTAL = 512 * 1024 * 1024;
const FILE_REVOKE_MS = 120000;
// WebSocket.readyState values, which sink-link.js compares with.
const FILE_CONNECTING = 0;
const FILE_OPEN = 1;
const FILE_CLOSED = 3;

let fileRecordedTotal = 0;
// tabId -> Set of open recorders; read by the popup (status) and "save now".
const fileRecorders = new Map();
// The last saves, for the popup: { name, bytes, at } or { error, at }.
const fileSaved = [];

// Whether this browser can save a recording: a background page (not a service worker) can make a blob URL, and the
// downloads API is there (the Firefox build asks for its permission).
const fileTargetAvailable = () =>
  typeof URL !== "undefined" &&
  typeof URL.createObjectURL === "function" &&
  typeof chrome !== "undefined" &&
  Boolean(chrome.downloads && typeof chrome.downloads.download === "function");

const fileStamp = (d) => {
  const p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}${p(d.getMonth() + 1)}${p(d.getDate())}-${p(d.getHours())}${p(d.getMinutes())}${p(d.getSeconds())}`;
};

// A file name from the page's host (the browser's URL, not the page's word): letters, digits, dots and dashes only.
const fileNameFor = (url, at, ext = "streamtap") => {
  let host = "";
  try {
    host = new URL(url).hostname;
  } catch (e) {
    host = "";
  }
  host = host.toLowerCase().replace(/[^a-z0-9.-]/g, "").replace(/^[.-]+|[.-]+$/g, "").slice(0, 64) || "video";
  return `streamtap-${fileStamp(at)}-${host}.${ext}`;
};

const fileLengthOf = (part) => (typeof part.size === "number" ? part.size : part.byteLength);

class FileRecorder {
  // `mode` "video": at the save the recording is made into an MP4 here (fmp4-mux.js); a capture it cannot take is
  // saved as the recording, with the reason.
  constructor(tabId, url, mode = "file") {
    this.readyState = FILE_CONNECTING;
    this.binaryType = "arraybuffer";
    this.bufferedAmount = 0;
    this.onopen = null;
    this.onclose = null;
    this.onerror = null;
    this.onmessage = null;
    this.tabId = tabId;
    this.url = String(url || "");
    this.startedAt = new Date();
    this.mode = mode === "video" ? "video" : "file";
    this.frames = [];
    this.bytes = 0;
    this.media = 0;
    this.full = false;
    this.dropped = 0;
    // Closed only to be saved, never refused: sink-link.js opens the next one without a backoff.
    this.restartAtOnce = true;
    if (!fileRecorders.has(tabId)) fileRecorders.set(tabId, new Set());
    fileRecorders.get(tabId).add(this);
    // Opened like a socket: after the caller has set its handlers.
    setTimeout(() => {
      if (this.readyState !== FILE_CONNECTING) return;
      this.readyState = FILE_OPEN;
      if (this.onopen) this.onopen();
    }, 0);
  }

  // One message as the link sends it (a Blob). `media` says that it is a media segment (sink-link.js linkWrite).
  send(message, media = false) {
    if (this.readyState !== FILE_OPEN) return;
    const size = fileLengthOf(message);
    const limit = media ? FILE_MAX_TOTAL : 2 * FILE_MAX_TOTAL;
    if (fileRecordedTotal + size + 4 > limit) {
      this.dropped++;
      if (media && !this.full) {
        this.full = true;
        this.push(sinkFrame({ type: "event", name: "recordfull", sb: null }));
      }
      return;
    }
    this.push(message);
    if (media) this.media++;
  }

  push(message) {
    const size = fileLengthOf(message);
    this.frames.push(message);
    this.bytes += size + 4;
    fileRecordedTotal += size + 4;
  }

  close() {
    if (this.readyState === FILE_CLOSED) return;
    this.readyState = FILE_CLOSED;
    const set = fileRecorders.get(this.tabId);
    if (set) {
      set.delete(this);
      if (set.size === 0) fileRecorders.delete(this.tabId);
    }
    this.save();
    if (this.onclose) this.onclose({ code: 1000, reason: "" });
  }

  save() {
    const frames = this.frames;
    const bytes = this.bytes;
    this.frames = [];
    fileRecordedTotal -= bytes;
    this.bytes = 0;
    // A page that held a MediaSource but appended no media is no recording.
    if (this.media === 0) return;
    const recording = () => {
      const parts = [FILE_MAGIC];
      for (const f of frames) {
        const len = new Uint8Array(4);
        new DataView(len.buffer).setUint32(0, fileLengthOf(f), true);
        parts.push(len, f);
      }
      return new Blob(parts, { type: "application/octet-stream" });
    };
    if (this.mode !== "video") {
      this.download(recording(), fileNameFor(this.url, this.startedAt), { bytes });
      return;
    }
    fileVideoOf(frames).then(
      (made) => {
        if (made.ok) this.download(made.blob, fileNameFor(this.url, this.startedAt, "mp4"), { bytes: made.blob.size, video: true, report: made.report });
        else this.download(recording(), fileNameFor(this.url, this.startedAt), { bytes, fallback: made.text, reason: made.reason });
      },
      () => this.download(recording(), fileNameFor(this.url, this.startedAt), { bytes, fallback: "the video could not be made", reason: "failed" })
    );
  }

  download(blob, name, note) {
    let href = null;
    try {
      href = URL.createObjectURL(blob);
    } catch (e) {
      fileNote({ error: "blob", at: Date.now() });
      return;
    }
    const done = (id) => {
      const failed = chrome.runtime.lastError || typeof id !== "number";
      fileNote(failed ? { error: "download", name, at: Date.now() } : { name, full: this.full, at: Date.now(), ...note });
      // The download reads the blob; it is let go once that had time to finish.
      setTimeout(() => URL.revokeObjectURL(href), FILE_REVOKE_MS);
    };
    try {
      chrome.downloads.download({ url: href, filename: name, saveAs: false, conflictAction: "uniquify" }, done);
    } catch (e) {
      done(null);
    }
  }
}

// The recorded messages made into one MP4 (fmp4-mux.js): { ok, blob, report } or { ok: false, reason, text }.
const fileVideoOf = async (frames) => {
  const messages = [];
  const dec = new TextDecoder();
  for (const f of frames) {
    const b = new Uint8Array(await f.arrayBuffer());
    if (b.byteLength < 4) continue;
    const hl = new DataView(b.buffer).getUint32(0, true);
    if (4 + hl > b.byteLength) continue;
    let header = null;
    try {
      header = JSON.parse(dec.decode(b.subarray(4, 4 + hl)));
    } catch (e) {
      continue;
    }
    messages.push({ header, payload: b.subarray(4 + hl) });
  }
  const r = fmp4Mux(messages);
  if (!r.ok) return { ok: false, reason: r.reason, text: r.text };
  const report = { gaps: r.report.gaps.length, gapSeconds: r.report.gaps.reduce((n, g) => n + g.seconds, 0), dupes: r.report.dupes, overlaps: r.report.overlaps, cut: r.report.cut };
  return { ok: true, blob: new Blob(r.parts, { type: "video/mp4" }), report };
};

const fileNote = (entry) => {
  fileSaved.unshift(entry);
  fileSaved.length = Math.min(fileSaved.length, 5);
};

// "Save now" from the popup: every recording of the tab is closed and saved; the links start a new recording at
// their next segment, which replays the inits in effect first (sink-link.js), so each file stands on its own.
const fileSaveTab = (tabId) => {
  const set = fileRecorders.get(tabId);
  if (!set) return 0;
  const open = [...set];
  for (const r of open) r.close();
  return open.length;
};

// The popup's view of the file target for a tab.
const fileStatus = (tabId) => {
  const set = fileRecorders.get(tabId);
  const s = { available: fileTargetAvailable(), recording: 0, bytes: 0, media: 0, full: false, saved: fileSaved.slice() };
  if (set) {
    for (const r of set) {
      s.recording++;
      s.bytes += r.bytes;
      s.media += r.media;
      s.full = s.full || r.full;
    }
  }
  return s;
};
