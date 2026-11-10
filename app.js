const $ = id => document.getElementById(id);
const video = $("video");
const keyLabels = { save: "Save short clip", save_long: "Save long clip", toggle: "Start / stop buffer", record: "Start / stop recording", screenshot: "Screenshot" };
let state;
let tab = "clips";
let current;
let playbackVersion = 0;
let previewRequested = false;
let lastFiles = "";
let lastKeys = "";
let editingKey;
const keyButtons = new Map();
let lastNotice;
let busy = false;

function element(tag, text, className) {
  const node = document.createElement(tag);
  if (text) node.textContent = text;
  if (className) node.className = className;
  return node;
}

function notice(message, error = false) {
  $("notice").hidden = !message;
  $("notice").textContent = message;
  $("notice").classList.toggle("error", error);
}

function path(file) {
  return "/media/" + encodeURIComponent(file.name);
}

function size(file) {
  return (file.size / 1_000_000).toFixed(1) + " MB";
}

function dateLabel(day) {
  const date = new Date(day + "T00:00:00");
  const today = new Date();
  const yesterday = new Date();
  yesterday.setDate(today.getDate() - 1);
  const label = date.toDateString();
  return label === today.toDateString() ? "Today" :
    label === yesterday.toDateString() ? "Yesterday" :
    date.toLocaleDateString(undefined, { weekday: "long" });
}

function renderLibrary() {
  if (!state) return;
  const query = $("search").value.trim().toLowerCase();
  const extension = tab === "clips" ? ".mp4" : ".png";
  const files = state.files.filter(file => file.name.endsWith(extension) && file.name.toLowerCase().includes(query));
  $("count").textContent = String(files.length);
  const byDay = Map.groupBy(files, file => file.name.slice(0, 10));
  const groups = $("groups");
  groups.replaceChildren();
  if (!files.length) {
    const empty = element("div", "", "empty");
    const shortcut = state.hotkeys?.[tab === "clips" ? "save" : "screenshot"];
    const message = query ? "Try a different search." : tab === "clips"
      ? (shortcut?.registered ? `Start the replay buffer and press ${shortcut.label} to save a clip.`
        : "Start the replay buffer and select Save short.")
      : (shortcut?.registered ? `Press ${shortcut.label} to capture your selected display.`
        : "Select Take screenshot to capture your selected display.");
    empty.append(element("h2", query ? "No matches" : tab === "clips" ? "No clips yet" : "No screenshots yet"),
      element("p", message));
    groups.append(empty);
  }
  for (const [day, entries] of byDay) {
    const section = element("section", "", "group");
    const heading = element("h2", dateLabel(day));
    heading.append(element("span", new Date(day + "T00:00:00").toLocaleDateString(undefined,
      { month: "short", day: "numeric" }), "group-date"));
    const grid = element("div", "", "grid");
    for (const file of entries) {
      const card = element("button", "", "card");
      const thumb = element("div", "", "thumb");
      if (extension === ".png") {
        const img = element("img");
        img.loading = "lazy";
        img.alt = "";
        img.src = path(file);
        thumb.append(img);
      } else {
        const img = element("img");
        img.loading = "lazy";
        img.decoding = "async";
        img.alt = "";
        img.src = "/thumb/" + encodeURIComponent(file.name);
        img.onerror = () => img.replaceWith(element("span", "Play"));
        thumb.append(img);
      }
      const time = file.name.slice(11, 19).replaceAll("-", ":");
      const title = file.name.endsWith(" recording.mp4") ? "Recording"
        : file.name.endsWith(" short.mp4") ? "Short clip"
        : file.name.endsWith(" long.mp4") ? "Long clip" : extension === ".mp4" ? "Clip" : "Screenshot";
      card.append(thumb, element("strong", title + " · " + time),
        element("span", size(file), "meta"));
      card.onclick = () => openFile(file);
      grid.append(card);
    }
    section.append(heading, grid);
    groups.append(section);
  }
}

function timecode(value) {
  if (!Number.isFinite(value)) return "0:00.0";
  return Math.floor(value / 60) + ":" + (value % 60).toFixed(1).padStart(4, "0");
}

function updateTimeline() {
  const duration = video.duration;
  if (!Number.isFinite(duration) || duration <= 0) return;
  const start = Number($("start").value);
  const end = Number($("end").value);
  $("time").textContent = timecode(video.currentTime) + " / " + timecode(duration);
  $("selection").textContent = timecode(start) + " – " + timecode(end);
  $("range").style.left = (start / duration * 100) + "%";
  $("range").style.width = ((end - start) / duration * 100) + "%";
  $("seek").value = video.currentTime;
  $("export").disabled = end <= start;
}

function openFile(file) {
  current = file;
  playbackVersion++;
  previewRequested = false;
  playbackStatus("");
  const clip = file.name.endsWith(".mp4");
  $("library").hidden = true;
  $("editor").hidden = false;
  $("filename").textContent = file.name;
  $("size").textContent = size(file);
  $("duration").textContent = clip ? "Loading…" : "—";
  $("resolution").textContent = "—";
  $("download").href = path(file);
  $("download").download = file.name;
  $("timeline").hidden = $("export").hidden = $("compatible").hidden = video.hidden = !clip;
  $("compatible").disabled = false;
  $("reveal-media").hidden = !clip;
  setPlaybackReady(false);
  $("image").hidden = clip;
  if (clip) {
    video.poster = "/thumb/" + encodeURIComponent(file.name);
    preparePlayback(file);
  } else {
    $("image").src = path(file);
  }
  $("back").focus();
}

function goBack() {
  current = undefined;
  playbackVersion++;
  video.pause();
  video.removeAttribute("src");
  video.removeAttribute("poster");
  video.load();
  $("image").removeAttribute("src");
  $("editor").hidden = true;
  $("library").hidden = false;
}

function switchTab(next) {
  goBack();
  tab = next;
  const clips = tab === "clips";
  $("clips").setAttribute("aria-pressed", clips);
  $("shots").setAttribute("aria-pressed", !clips);
  $("title").textContent = clips ? "Clips" : "Screenshots";
  $("capture").textContent = clips ? "Save short" : "Take screenshot";
  $("capture-long").hidden = !clips;
  $("search").placeholder = clips ? "Search clips" : "Search screenshots";
  $("search").value = "";
  renderLibrary();
}

async function refresh() {
  const response = await fetch("/api");
  const firstState = !state;
  state = await response.json();
  if (!settingsPage.hidden && !$("audio-options").hidden) renderMixer();
  if (firstState && videoSettingsVisible()) loadCaptureSettings();
  $("toggle").disabled = state.recording;
  $("record").classList.toggle("recording", state.recording);
  $("record").querySelector("b").textContent = state.recording
    ? (state.active ? "Stop recording" : "Save recording") : "Record";
  $("record").title = state.hotkeys?.record?.registered ? state.hotkeys.record.label : "";
  for (const key of generalKeys) {
    $(key).checked = state.general[key];
    $(key).disabled = false;
  }
  $("toggle").classList.toggle("live", state.active);
  $("toggle").querySelector("b").textContent = state.active ? "Buffering · Stop" : "Start replay buffer";
  const { seconds, longSeconds } = state.settings;
  $("media-folder").value = state.mediaFolder;
  $("length").textContent = `${formatLength(seconds)} / ${formatLength(longSeconds)}`;
  $("capture").title = tab === "clips" ? `Save the last ${formatLength(seconds)}` : "";
  $("capture-long").title = `Save the last ${formatLength(longSeconds)}`;
  if (!$("lengths").closest("details").open) {
    $("short-seconds").value = seconds;
    $("long-seconds").value = longSeconds;
  }
  updateCaptureDetails();
  const message = state.error || state.hotkeyError || state.notice ||
    (state.available ? "" : "FFmpeg is missing. Install a Windows build with ddagrab and NVENC, then restart rsclip.");
  if (message !== lastNotice) {
    notice(message, Boolean(state.error || !state.available));
    lastNotice = message;
  }
  const files = JSON.stringify(state.files);
  const keys = JSON.stringify(state.hotkeys);
  renderKeybinds();
  if (files !== lastFiles || keys !== lastKeys) {
    lastFiles = files;
    lastKeys = keys;
    renderLibrary();
  }
  if (current?.name.endsWith(".mp4") && video.videoWidth > 0) updateTimeline();
}

async function act(action, data = {}, onError = error => notice(error, true)) {
  if (busy || !state) return false;
  busy = true;
  try {
    const response = await fetch("/api/" + action, {
      method: "POST", headers: { "Content-Type": "application/json", "X-Rsclip-Token": state.token },
      body: JSON.stringify(data),
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.error);
    if (action !== "hotkey") notice(result.notice);
    await refresh();
    return true;
  } catch (error) {
    onError(error.message);
    return false;
  } finally {
    busy = false;
  }
}

function formatLength(seconds) {
  if (seconds < 60) return `${seconds}s`;
  const remainder = seconds % 60;
  return `${Math.floor(seconds / 60)}m${remainder ? ` ${remainder}s` : ""}`;
}

function updateCaptureDetails() {
  const mode = $("compression").value;
  $("quality-row").hidden = mode !== "quality";
  $("bitrate-label").textContent = mode === "quality" ? "Bitrate cap (Mbps)"
    : mode === "vbr" ? "Target bitrate (Mbps)" : "Bitrate (Mbps)";
  const bitrate = $("bitrate").valueAsNumber;
  const seconds = state?.settings?.longSeconds;
  if (Number.isFinite(bitrate) && seconds) {
    const peak = bitrate * (mode === "vbr" ? 2 : 1);
    const mb = Math.ceil((seconds + 4) * (peak + .16) / 8);
    $("buffer-budget").textContent = `At the current long-clip length, allow about ${mb >= 1000 ? (mb / 1000).toFixed(1) + " GB" : mb + " MB"} of temporary storage at peak bitrate.`;
  }
}

for (const id of ["compression", "bitrate"]) $(id).oninput = updateCaptureDetails;

$("lengths").onsubmit = async event => {
  event.preventDefault();
  if (!state) return;
  const seconds = $("short-seconds").valueAsNumber;
  const longSeconds = $("long-seconds").valueAsNumber;
  if (await act("settings", { ...state.settings, seconds, longSeconds })) {
    $("lengths").closest("details").open = false;
  }
};

$("capture-settings").onsubmit = async event => {
  event.preventDefault();
  if (!state) return;
  if (!monitorSelect.value || monitorSelect.disabled) return;
  const encoding = {
    output: Number(monitorSelect.value),
    fps: $("fps").valueAsNumber,
    codec: $("codec").value,
    compression: $("compression").value,
    bitrate: $("bitrate").valueAsNumber,
    quality: $("quality").valueAsNumber,
    preset: Number($("preset").value),
  };
  await act("settings", { ...state.settings, encoding });
};

const settingsPage = $("settings-page");
const generalKeys = ["launchOnLogin", "autoBuffer", "startHidden", "captureOverlay", "saveSounds"];
const videoSettingsVisible = () => !settingsPage.hidden && !$("video-options").hidden;
const monitorSelect = $("output");
const previewImage = $("monitor-preview-image");
const previewStatus = $("monitor-preview-status");
const refreshPreview = $("refresh-monitor");
let monitorListVersion = 0;
let previewVersion = 0;
let previewUrl;
let previewPending;
let previewRunning = false;

function clearMonitorPreview(message) {
  if (previewUrl) URL.revokeObjectURL(previewUrl);
  previewUrl = undefined;
  previewImage.removeAttribute("src");
  previewImage.hidden = true;
  previewStatus.hidden = false;
  previewStatus.textContent = message;
}

async function loadMonitors() {
  const version = ++monitorListVersion;
  monitorSelect.replaceChildren(new Option("Loading monitors...", ""));
  monitorSelect.disabled = refreshPreview.disabled = true;
  clearMonitorPreview("Looking for monitors...");
  try {
    const response = await fetch("/api/monitors");
    const monitors = await response.json();
    if (!response.ok) throw new Error(monitors.error);
    if (!videoSettingsVisible() || version !== monitorListVersion) return;
    if (!monitors.length) throw new Error("No monitors found on the capture adapter.");
    monitorSelect.replaceChildren(...monitors.map(monitor => {
      const label = monitor.name === monitor.display ? monitor.name : `${monitor.name} (${monitor.display})`;
      return new Option(`${label} · ${monitor.width} × ${monitor.height}`, monitor.output);
    }));
    monitorSelect.disabled = false;
    monitorSelect.value = state.settings.encoding.output;
    if (!monitorSelect.value) {
      const placeholder = new Option("Choose a monitor", "", true, true);
      placeholder.disabled = true;
      monitorSelect.prepend(placeholder);
      monitorSelect.value = "";
    }
    queueMonitorPreview();
  } catch (error) {
    if (!videoSettingsVisible() || version !== monitorListVersion) return;
    monitorSelect.replaceChildren(new Option("No monitor available", ""));
    clearMonitorPreview(error.message);
  }
}

function queueMonitorPreview() {
  const version = ++previewVersion;
  previewPending = undefined;
  refreshPreview.disabled = !monitorSelect.value || !state?.available;
  if (!monitorSelect.value) {
    clearMonitorPreview("Choose a monitor to preview.");
    return;
  }
  if (!state?.available) {
    clearMonitorPreview("Install FFmpeg to preview your monitor.");
    return;
  }
  clearMonitorPreview("Capturing one frame...");
  previewPending = { version, output: Number(monitorSelect.value) };
  if (!previewRunning) showMonitorPreview();
}

async function showMonitorPreview() {
  previewRunning = true;
  try {
    while (previewPending) {
      const { version, output } = previewPending;
      previewPending = undefined;
      try {
        const response = await fetch("/api/monitor-preview", {
          method: "POST",
          headers: { "Content-Type": "application/json", "X-Rsclip-Token": state.token },
          body: JSON.stringify({ output }),
        });
        if (!response.ok) throw new Error((await response.json()).error);
        const png = await response.blob();
        if (!videoSettingsVisible() || version !== previewVersion) continue;
        previewUrl = URL.createObjectURL(png);
        previewImage.src = previewUrl;
        previewImage.hidden = false;
        previewStatus.hidden = true;
      } catch (error) {
        if (videoSettingsVisible() && version === previewVersion) clearMonitorPreview(error.message);
      }
    }
  } finally {
    previewRunning = false;
  }
}

function loadCaptureSettings() {
  const { encoding } = state.settings;
  for (const key of ["fps", "codec", "compression", "bitrate", "quality", "preset"]) {
    $(key).value = encoding[key];
  }
  updateCaptureDetails();
  loadMonitors();
}

function leaveVideoSettings() {
  monitorListVersion++;
  previewVersion++;
  previewPending = undefined;
  clearMonitorPreview("Choose a monitor to preview.");
}

function selectSettingsSection(section) {
  const wasVideo = videoSettingsVisible();
  for (const tab of ["general", "video", "audio"]) {
    $(`settings-${tab}`).setAttribute("aria-pressed", tab === section);
    $(`${tab}-options`).hidden = tab !== section;
  }
  if (section === "video" && !wasVideo && state) loadCaptureSettings();
  if (wasVideo && section !== "video") leaveVideoSettings();
  if (section === "audio" && state) renderMixer();
}

for (const section of ["general", "video", "audio"]) {
  $(`settings-${section}`).onclick = () => selectSettingsSection(section);
}

let mixerSources = "";
let mixerPresets = "";
function renderMixer() {
  const config = state.mixer;
  if (!config) return;
  $("mixer-mic").checked = config.microphone;
  $("mixer-apps").checked = config.perApp;
  $("mixer-mic").disabled = $("mixer-apps").disabled = false;
  const presets = Object.keys(config.presets);
  const presetKey = JSON.stringify(presets);
  if (presetKey !== mixerPresets) {
    mixerPresets = presetKey;
    const selected = $("mixer-presets").value;
    $("mixer-presets").replaceChildren(...presets.map(name => new Option(name, name)));
    if (presets.includes(selected)) $("mixer-presets").value = selected;
    $("mixer-load").disabled = !presets.length;
  }
  const sources = Object.entries(state.audioSources);
  const sourceKey = JSON.stringify(sources);
  if (sourceKey !== mixerSources) {
    mixerSources = sourceKey;
    $("mixer-channels").replaceChildren(...sources.map(([id, name]) => {
      const label = element("label", "", "mixer-channel");
      const range = element("input");
      range.type = "range";
      range.min = 0;
      range.max = 200;
      range.id = `gain-${id}`;
      const readout = element("output");
      readout.id = `gain-value-${id}`;
      range.oninput = () => { readout.textContent = `${range.value}%`; };
      range.onchange = async () => {
        if (!await act("mixer", { action: "gain", source: id, gain: range.valueAsNumber })) {
          range.value = state.mixer.gains[id] ?? 100;
          range.oninput();
        }
      };
      label.append(element("span", name), range, readout);
      return label;
    }));
  }
  for (const [id] of sources) {
    const range = $(`gain-${id}`);
    if (document.activeElement !== range) {
      range.value = config.gains[id] ?? 100;
      range.oninput();
    }
  }
}

for (const [id, action] of [["mixer-mic", "microphone"], ["mixer-apps", "perApp"]]) {
  $(id).onchange = async () => {
    if (!await act("mixer", { action, enabled: $(id).checked })) renderMixer();
  };
}
$("mixer-save").onclick = () => act("mixer", { action: "savePreset", name: $("mixer-name").value });
$("mixer-load").onclick = () => act("mixer", { action: "loadPreset", name: $("mixer-presets").value });
$("browse-media").onclick = () => act("browse-media");
$("add-library-folder").onclick = () => act("add-library-folder");
$("record").onclick = () => act("record");
for (const key of generalKeys) {
  $(key).onchange = async () => {
    if (!await act("general", { setting: key, enabled: $(key).checked })) {
      $(key).checked = state.general[key];
    }
  };
}

function showSettings(open) {
  for (const menu of document.querySelectorAll(".tools details[open]")) menu.open = false;
  settingsPage.hidden = !open;
  $("library-nav").inert = open;
  $("settings-icon").hidden = open;
  $("home-icon").hidden = !open;
  $("settings-nav-label").textContent = open ? "Home" : "Settings";

  if (open) {
    goBack();
    $("library").hidden = true;
    selectSettingsSection("general");
    $("settings-title").focus();
  } else {
    leaveVideoSettings();
    $("library").hidden = false;
    $("settings-nav").focus();
  }
}

$("settings-nav").onclick = () => showSettings(settingsPage.hidden);
monitorSelect.onchange = queueMonitorPreview;
refreshPreview.onclick = queueMonitorPreview;

const keyHelp = $("key-help");
const keyPanel = keyHelp.closest("details");

function keyFeedback(message, error = false) {
  keyHelp.textContent = message;
  keyHelp.hidden = !message;
  keyHelp.classList.toggle("error", error);
}

function renderKeybinds() {
  for (const [action, button] of keyButtons) {
    const binding = state?.hotkeys?.[action];
    const editing = action === editingKey;
    button.textContent = editing ? "Press shortcut..." : binding?.label ?? "Loading...";
    button.classList.toggle("unavailable", Boolean(binding && !binding.registered && !editing));
    button.setAttribute("aria-pressed", editing);
    button.setAttribute("aria-label", `${keyLabels[action]} shortcut: ${button.textContent}${binding && !binding.registered ? ", unavailable" : ""}`);
    button.disabled = !binding;
  }
}

function cancelKeyEdit() {
  editingKey = undefined;
  renderKeybinds();
  keyFeedback("");
}

for (const [action, label] of Object.entries(keyLabels)) {
  const row = element("div");
  const button = element("button", "Loading...", "keybind");
  button.disabled = true;
  button.onclick = () => {
    if (busy) return;
    if (editingKey === action) {
      cancelKeyEdit();
      return;
    }
    editingKey = action;
    keyFeedback("");
    renderKeybinds();
  };
  keyButtons.set(action, button);
  row.append(element("span", label), button);
  $("keys").append(row);
}

keyPanel.addEventListener("toggle", () => {
  if (!keyPanel.open) cancelKeyEdit();
});

document.addEventListener("keydown", async event => {
  if (!editingKey || !keyPanel.open) return;
  if (event.key === "Tab") {
    cancelKeyEdit();
    return;
  }
  event.preventDefault();
  event.stopImmediatePropagation();
  if (event.key === "Escape") {
    cancelKeyEdit();
    return;
  }
  if (busy || event.repeat || ["Control", "Alt", "Shift", "Meta"].includes(event.key)) return;
  const modifiers = Number(event.altKey) | (Number(event.ctrlKey) << 1) | (Number(event.shiftKey) << 2);
  const code = event.code;
  const key = /^(Key[A-Z]|Digit[0-9])$/.test(code) ? code.slice(-1) : event.key;
  if (event.metaKey || !modifiers || !/^([A-Z0-9]|F([1-9]|1[0-9]|2[0-4]))$/.test(key)) {
    keyFeedback("Use Ctrl, Alt, or Shift with a letter, number, or function key.", true);
    return;
  }
  const action = editingKey;
  const changed = await act("hotkey", { action, binding: { key, modifiers } }, error => keyFeedback(error, true));
  if (changed && editingKey === action) {
    editingKey = undefined;
    renderKeybinds();
    keyFeedback("");
  }
}, true);

document.addEventListener("click", event => {
  for (const panel of document.querySelectorAll("details[open]")) {
    if (!panel.contains(event.target)) panel.open = false;
  }
});
document.addEventListener("keydown", event => {
  if (event.key === "Escape") {
    if (document.body.classList.contains("clip-fullscreen")) {
      setClipFullscreen(false);
      return;
    }
    const panel = document.querySelector("details[open]");
    if (panel) {
      panel.open = false;
      panel.querySelector("summary").focus();
    } else if (!settingsPage.hidden) showSettings(false);
    else if (current) goBack();
  }
});

$("clips").onclick = () => switchTab("clips");
$("shots").onclick = () => switchTab("shots");
$("back").onclick = goBack;
$("search").oninput = renderLibrary;
$("toggle").onclick = () => act("toggle");
$("capture").onclick = () => act(tab === "clips" ? "save" : "screenshot");
$("capture-long").onclick = () => act("save_long");
$("compatible").onclick = () => current && act("compatible", { name: current.name });
$("image").onload = () => { $("resolution").textContent = $("image").naturalWidth + " × " + $("image").naturalHeight; };
let fullscreenBusy = false;
let fullscreenVersion = 0;

function renderFullscreen(enabled) {
  document.body.classList.toggle("clip-fullscreen", enabled);
  $("fullscreen").setAttribute("aria-pressed", enabled);
  $("fullscreen").textContent = enabled ? "Exit fullscreen" : "Fullscreen";
}

async function setClipFullscreen(enabled) {
  if (!state || fullscreenBusy) return;
  fullscreenBusy = true;
  const version = ++fullscreenVersion;
  try {
    const response = await fetch("/api/fullscreen", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Rsclip-Token": state.token },
      body: JSON.stringify({ enabled }),
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.error);
    if (version === fullscreenVersion) renderFullscreen(result.fullscreen);
  } catch (error) {
    playbackStatus(error.message);
  } finally {
    fullscreenBusy = false;
  }
}

window.pauseClipPlayback = () => {
  video.pause();
  fullscreenVersion++;
  renderFullscreen(false);
};

$("fullscreen").onclick = () => setClipFullscreen(!document.body.classList.contains("clip-fullscreen"));
$("reveal-media").onclick = () => current && act("reveal-media", { name: current.name });

function playbackStatus(message) {
  $("playback-status").textContent = message;
  $("playback-status").hidden = !message;
}

function setPlaybackReady(ready) {
  for (const id of ["play", "seek", "start", "end", "export"]) $(id).disabled = !ready;
}

async function preparePlayback(file, force = false) {
  if (force) previewRequested = true;
  const version = playbackVersion;
  video.pause();
  setPlaybackReady(false);
  playbackStatus(force ? "Preparing a compatible preview..." : "Opening clip...");
  try {
    const response = await fetch("/api/prepare-playback", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-Rsclip-Token": state.token },
      body: JSON.stringify({ name: file.name, force }),
    });
    const result = await response.json();
    if (!response.ok) throw new Error(result.error);
    if (version !== playbackVersion) return;
    previewRequested = result.proxy;
    video.src = result.proxy ? "/playback/" + encodeURIComponent(file.name) : path(file);
  } catch (error) {
    if (version === playbackVersion) playbackStatus(error.message);
  }
}

function playbackFailed() {
  if (!current || video.hidden || !video.getAttribute("src")) return;
  if (!previewRequested) preparePlayback(current, true);
  else {
    video.pause();
    setPlaybackReady(false);
    playbackStatus("The video could not be played. You can still download the original or make an H.264 copy.");
  }
}

video.onloadedmetadata = () => {
  if (!current || video.hidden) return;
  if (!video.videoWidth || !Number.isFinite(video.duration)) {
    playbackFailed();
    return;
  }
  playbackStatus("");
  setPlaybackReady(true);
  $("duration").textContent = timecode(video.duration);
  $("resolution").textContent = video.videoWidth + " x " + video.videoHeight + (previewRequested ? " preview" : "");
  for (const id of ["seek", "start", "end"]) $(id).max = video.duration;
  $("start").value = 0;
  $("end").value = video.duration;
  $("ticks").replaceChildren(...[0, .25, .5, .75, 1].map(fraction => element("span", timecode(video.duration * fraction))));
  updateTimeline();
};
video.onerror = playbackFailed;
video.ontimeupdate = updateTimeline;
video.onplay = video.onpause = video.onended = () => {
  const label = video.paused || video.ended ? "Play" : "Pause";
  $("play").textContent = label;
  $("play").setAttribute("aria-label", label);
};
$("play").onclick = () => video.paused ? video.play().catch(error => playbackStatus(error.message)) : video.pause();
$("mute").onclick = () => { video.muted = !video.muted; };
$("volume").oninput = () => { video.volume = Number($("volume").value); video.muted = false; };
video.onvolumechange = () => {
  $("mute").textContent = video.muted ? "Unmute" : "Mute";
  $("mute").setAttribute("aria-pressed", video.muted);
  $("volume").value = video.volume;
};
$("seek").oninput = () => { video.currentTime = Number($("seek").value); };
for (const id of ["start", "end"]) {
  $(id).oninput = () => {
    if (Number($("start").value) > Number($("end").value)) {
      $(id).value = $(id === "start" ? "end" : "start").value;
    }
    video.currentTime = Number($(id).value);
    updateTimeline();
  };
}
$("export").onclick = () => current && act("trim", { name: current.name,
  start: Number($("start").value), end: Number($("end").value) });

async function poll() {
  try {
    if (!busy) await refresh();
  } catch {
    notice("rsclip is disconnected. Restart the launcher to reconnect.", true);
  }
  setTimeout(poll, document.hidden ? 5000 : 1800);
}
poll();

function renderPerformance({ cpu, ramBytes, gpu }) {
  const percent = value => Number.isFinite(value) ? `${value.toFixed(1)}%` : "—";
  $("perf-cpu").textContent = percent(cpu);
  $("perf-gpu").textContent = percent(gpu);
  $("perf-ram").textContent = Number.isFinite(ramBytes)
    ? ramBytes >= 2 ** 30 ? `${(ramBytes / 2 ** 30).toFixed(1)} GiB` : `${Math.round(ramBytes / 2 ** 20)} MiB`
    : "—";
}

async function pollPerformance() {
  if (!document.hidden) {
    try {
      const response = await fetch("/api/performance");
      if (!response.ok) throw new Error("Performance data unavailable");
      renderPerformance(await response.json());
    } catch {
      renderPerformance({});
    }
  }
  setTimeout(pollPerformance, document.hidden ? 5000 : 2000);
}

pollPerformance();
