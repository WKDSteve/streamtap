// Build flavour of the Firefox (for Android) variant. Only the Firefox manifest
// that scripts/build-firefox.mjs writes loads this file: in the background
// page before the other scripts, and in the isolated world before content.js.
// The Chrome build has no flavour file and keeps its defaults.
//
// - sourcePolicyDefault "on": the source policy (source-policy.js) is on
//   unless the user stored "off" (the same default as the Chrome build).
// - nativePairing false: Firefox for Android has no native messaging, and the
//   desktop app registers its host for Chrome, Edge and Brave only.
"use strict";

globalThis.STREAMTAP_FLAVOR = Object.freeze({ name: "firefox", sourcePolicyDefault: "on", nativePairing: false });
