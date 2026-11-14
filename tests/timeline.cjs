const fs = require("node:fs");
const vm = require("node:vm");
const assert = require("node:assert/strict");
const source = fs.readFileSync(require("node:path").join(__dirname, "../app.js"), "utf8");
const nodes = new Map();
function $(id) {
  if (!nodes.has(id)) nodes.set(id, { value: id === "end" ? 1 : 0, max: 1, style: {}, replaceChildren() {} });
  return nodes.get(id);
}
const video = { readyState: 1, videoWidth: 0, videoHeight: 0, duration: NaN, currentTime: 0, hidden: false, paused: true };
const context = vm.createContext({
  $, video, current: { name: "clip.mp4" }, clipDuration: 30, trimReady: false,
  playbackOffset: 0, mixResume: undefined, activePreviewGains: undefined, previewRequested: true,
  timecode: String, paintSlider() {}, element() { return {}; }, playbackStatus() {},
  setPlaybackReady() {}, playbackFailed() {}, preparePlayback() {},
});
vm.runInContext(source.slice(source.indexOf("function updateTimeline()"), source.indexOf("\nfunction openFile(")), context);
vm.runInContext(source.slice(source.indexOf("video.onloadedmetadata ="), source.indexOf("\nvideo.onerror =")), context);
vm.runInContext(source.slice(source.indexOf("function fallbackPlayback()")), context);

// Native audio-only metadata must not save the HTML input's placeholder end=1.
context.fallbackPlayback();
assert.equal(context.mixResume, undefined);
video.videoWidth = 1920;
video.videoHeight = 1080;
video.duration = 12;
video.onloadedmetadata();
assert.equal($("start").value, 0);
assert.equal($("end").value, 30);
assert.equal($("range").style.width, "100%");

// Sections and fallback must retain the user's absolute selection.
$("start").value = 5;
$("end").value = 25;
context.mixResume = { start: 5, end: 25, time: 18, playing: false };
context.playbackOffset = 12;
video.onloadedmetadata();
assert.equal($("start").value, 5);
assert.equal($("end").value, 25);
assert.equal(video.currentTime, 6);
video.onloadedmetadata();
assert.equal($("end").value, 25);

// A different clip starts fully selected, even with old slider values present.
context.trimReady = false;
context.clipDuration = 45;
context.playbackOffset = 0;
video.onloadedmetadata();
assert.equal($("start").value, 0);
assert.equal($("end").value, 45);
assert.equal($("range").style.width, "100%");
console.log("Timeline regression checks passed.");
