"use strict";

const out = document.getElementById("out");

const fmtBytes = (n) =>
  n >= 1 << 20 ? (n / (1 << 20)).toFixed(1) + " MiB" : n >= 1024 ? (n / 1024).toFixed(0) + " KiB" : n + " B";

const esc = (s) => String(s).replace(/[&<>]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;" })[c]);

const render = (state) => {
  if (!state) {
    out.innerHTML = '<span class="muted">No MSE activity seen in this tab yet. Play a video, then reopen.</span>';
    return;
  }
  let html = "";
  if (state.drm.length) {
    html += `<div class="drm">DRM requested: ${state.drm.map((d) => esc(d.keySystem) + (d.robustness.length ? " (" + esc(d.robustness.join(", ")) + ")" : "")).join("; ")}</div>`;
  }
  if (!state.mediaSources.length) {
    html += '<div class="muted">No MediaSource created yet.</div>';
  }
  for (const m of state.mediaSources) {
    html += `<div class="ms"><h2>MediaSource #${m.msId}${m.ended ? " · ended" : ""}</h2>`;
    html += `<div class="muted">${esc(m.frameUrl)}${m.blobUrl ? "<br>" + esc(m.blobUrl) : ""}</div>`;
    if (m.sourceBuffers.length) {
      html += "<table><tr><th>#</th><th>mime</th><th>kind</th><th>init</th><th>media</th><th>bytes</th><th>range</th></tr>";
      for (const s of m.sourceBuffers) {
        const range =
          s.firstSec === null ? "–" : `${s.firstSec}s – ${s.lastEndSec ?? s.lastSec}s`;
        html += `<tr><td>${s.sbId}</td><td class="mime">${esc(s.mime)}${s.mimes.length ? '<br><span class="muted">was: ' + esc(s.mimes.join(" → ")) + "</span>" : ""}</td><td>${esc(s.kind ?? "–")}</td><td>${s.inits}</td><td>${s.media}</td><td>${fmtBytes(s.bytes)}</td><td>${range}</td></tr>`;
      }
      html += "</table>";
    }
    html += "</div>";
  }
  html += `<div class="muted">${state.events} events · ${state.frames.length} frame(s)</div>`;
  out.innerHTML = html;
};

const load = async () => {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (!tab) return render(null);
  chrome.runtime.sendMessage({ kind: "vc-get", tabId: tab.id }, (state) => render(state));
  document.getElementById("clear").onclick = () =>
    chrome.runtime.sendMessage({ kind: "vc-clear", tabId: tab.id }, () => render(null));
};

load();
