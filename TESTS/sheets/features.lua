-- TESTS/sheets/features.lua -- the functionalities of streamtap as its user meets them; `testing sheets report` says
-- which ones have a feature sheet (`feature = "<id>"`) and in which state, and K16 warns about a sheet that names an
-- id not listed here. Data: read with an empty environment like a sheet (testing.nvim docs/SHEETS.md).
-- The list follows README.md (Status, the install steps), desktop/README.md and extension/README.md. streamtap has no
-- command surface for `testing surface`, so no entry names one.
return {
  {
    id = "streamtap.install",
    title = "Install from the release zip for the user only, with a Start menu entry, and uninstall again",
    docs = "README.md",
  },
  {
    id = "streamtap.sink",
    title = "Start and stop the sink in the app; a capture needs it running",
    docs = "desktop/README.md",
  },
  {
    id = "streamtap.pairing",
    title = "Connect browser: register Chrome, Edge or Brave once, approve the pairing, no copy and paste",
    docs = "desktop/README.md",
  },
  {
    id = "streamtap.capture",
    title = "Pick a tab that holds a video and capture what its player feeds into MSE, per SourceBuffer",
    docs = "extension/README.md",
  },
  {
    id = "streamtap.captures-list",
    title = "The app lists the captures live and opens or deletes them",
    docs = "desktop/README.md",
  },
  {
    id = "streamtap.mux",
    title = "Mux a capture into one file with ffmpeg -c copy, reordered after seeks, and open the result",
    docs = "desktop/README.md",
  },
  {
    id = "streamtap.ffmpeg-missing",
    title = 'Without ffmpeg on the PATH the app says "ffmpeg not found" instead of failing a mux',
    docs = "README.md",
  },
  {
    id = "streamtap.button-states",
    title = "The filled button of the app's page shows rest, pointer and keyboard focus apart, light and dark",
    docs = "desktop/README.md",
  },
}
