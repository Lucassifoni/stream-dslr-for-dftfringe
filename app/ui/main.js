/**
 * Livestream UI: polls `get_status`, drives `set_playing` / `set_config`,
 * discovers cameras (`list_cameras`, `list_aravis`) and edits V4L2 controls
 * live (`list_controls`, `set_control`). Form logic lives in model.js.
 * The preview is the same MJPEG URL DFTFringe consumes.
 */
import {
  aravisConfig,
  binningChoices,
  cameraConfig,
  cameraFormats,
  cameraNote,
  cameraRates,
  cameraSizes,
  describe,
  errorLine,
  levelModes,
  pickCamera,
  rateKey,
  rateLabel,
  sourcePending,
} from "./model.js";

const { invoke } = window.__TAURI__.core;

const POLL_MS = 500;
const $ = (id) => document.getElementById(id);
const radios = (groupId, attr) => [...document.querySelectorAll(`#${groupId} [data-${attr}]`)];
const sourceButtons = radios("sources", "source");
const modeButtons = radios("modes", "mode");

let current = null;
let initialised = false;
let cameras = [];
let cameraSelection = null;
let aravisDevices = [];
let actionError = null;

function checked(buttons, attr) {
  return buttons.find((b) => b.getAttribute("aria-checked") === "true").dataset[attr];
}

function check(buttons, attr, value) {
  buttons.forEach((b) => b.setAttribute("aria-checked", String(b.dataset[attr] === value)));
}

function showSource(source) {
  check(sourceButtons, "source", source);
  $("dslr-options").hidden = source !== "dslr";
  $("camera-options").hidden = source !== "camera";
  $("aravis-options").hidden = source !== "aravis";
}

function showMode(mode) {
  check(modeButtons, "mode", mode);
  $("stills-options").hidden = mode !== "stills";
}

function showError(message) {
  $("error").textContent = message || "";
  $("error").hidden = !message;
}

/** Remember a failed command so polling keeps showing it until the next success. */
function fail(e) {
  actionError = String(e);
  showError(actionError);
}

function ok() {
  actionError = null;
  showError(current && !sourcePending(checked(sourceButtons, "source"), current) ? errorLine(current) : null);
}

function fillSelect(select, options, value) {
  select.replaceChildren(...options.map((o) => new Option(o.label, o.value)));
  if (options.some((o) => o.value === value)) select.value = value;
}

function dslrConfig() {
  return {
    source: "dslr",
    mode: checked(modeButtons, "mode"),
    interval: Math.round(Number($("interval").value)),
    imageFormat: $("format").value,
  };
}

async function sendConfig(config) {
  if (!config) return;
  try {
    await invoke("set_config", { config });
    ok();
  } catch (e) {
    fail(e);
  }
  refresh();
}

function selectedDevice() {
  return cameras.find((d) => d.id === cameraSelection?.device);
}

function renderCameraForm() {
  const device = selectedDevice();
  fillSelect(
    $("cam-device"),
    cameras.map((d) => ({ value: d.id, label: `${d.name} (${d.id})${d.error ? " — unavailable" : ""}` })),
    cameraSelection?.device,
  );
  const sel = cameraSelection ?? {};
  fillSelect($("cam-format"), cameraFormats(device).map((f) => ({ value: f.key, label: f.label })), sel.format);
  fillSelect($("cam-size"), cameraSizes(device, sel.format).map((s) => ({ value: s, label: s.replace("x", "×") })), sel.size);
  fillSelect(
    $("cam-fps"),
    cameraRates(device, sel.format, sel.size).map((r) => ({ value: rateKey(r), label: rateLabel(r) })),
    sel.rate,
  );
  const note = cameraSelection ? cameraNote(device, sel.format) : cameraNote(cameras.find((d) => d.error), null);
  $("cam-note").textContent = note || "";
  $("cam-note").hidden = !note;
}

function cameraFormValues() {
  return {
    device: $("cam-device").value,
    format: $("cam-format").value,
    size: $("cam-size").value,
    rate: $("cam-fps").value,
  };
}

async function loadCameras() {
  try {
    cameras = await invoke("list_cameras");
  } catch (e) {
    cameras = [];
    fail(e);
  }
  cameraSelection = pickCamera(cameras, cameraSelection ?? {});
  renderCameraForm();
  await loadControls();
}

async function applyCamera(changed) {
  const previous = cameraFormValues();
  cameraSelection = pickCamera(cameras, previous);
  renderCameraForm();
  if (changed === "device") await loadControls();
  if (cameraSelection) sendConfig(cameraConfig(cameraSelection));
}

function controlInput(c) {
  const disabled = c.inactive || c.readOnly;
  const send = (value) => setControl(c.id, value);
  if (c.kind === "boolean") {
    const box = document.createElement("input");
    box.type = "checkbox";
    box.checked = c.value === 1;
    box.disabled = disabled;
    box.addEventListener("change", () => send(box.checked ? 1 : 0));
    return [box];
  }
  if (c.kind === "menu" || c.kind === "integerMenu") {
    const select = document.createElement("select");
    fillSelect(select, c.menu.map((m) => ({ value: String(m.index), label: m.label })), String(c.value));
    select.disabled = disabled;
    select.addEventListener("change", () => send(Number(select.value)));
    return [select];
  }
  if (c.kind === "button") {
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = "Run";
    button.disabled = disabled;
    button.addEventListener("click", () => send(0));
    return [button];
  }
  const range = document.createElement("input");
  range.type = "range";
  Object.assign(range, { min: c.min, max: c.max, step: Math.max(1, c.step), value: c.value ?? c.default, disabled });
  const number = document.createElement("input");
  number.type = "number";
  number.className = "short";
  Object.assign(number, { min: c.min, max: c.max, step: Math.max(1, c.step), value: c.value ?? c.default, disabled });
  range.addEventListener("input", () => (number.value = range.value));
  range.addEventListener("change", () => send(Number(range.value)));
  number.addEventListener("change", () => send(Number(number.value)));
  return [range, number];
}

function renderControls(controls) {
  const rows = controls.map((c) => {
    const row = document.createElement("div");
    row.className = `control${c.inactive ? " inactive" : ""}`;
    const name = document.createElement("span");
    name.className = "control-name";
    name.textContent = c.name;
    name.title = c.inactive ? "Inactive: controlled by its automatic mode" : `default ${c.default}`;
    const reset = document.createElement("button");
    reset.type = "button";
    reset.className = "icon-button";
    reset.textContent = "⟲";
    reset.title = `Reset to default (${c.default})`;
    reset.disabled = c.inactive || c.readOnly || c.kind === "button" || c.value === c.default;
    reset.addEventListener("click", () => setControl(c.id, c.default));
    row.append(name, ...controlInput(c), reset);
    return row;
  });
  $("cam-control-list").replaceChildren(...rows);
  $("cam-controls").hidden = controls.length === 0;
}

async function loadControls() {
  const device = selectedDevice();
  if (!device || device.error) return renderControls([]);
  try {
    renderControls(await invoke("list_controls", { device: device.id }));
  } catch (e) {
    renderControls([]);
    fail(e);
  }
}

async function setControl(id, value) {
  try {
    renderControls(await invoke("set_control", { device: cameraSelection.device, id, value }));
    ok();
  } catch (e) {
    fail(e);
    loadControls();
  }
}

function selectedAravis() {
  return aravisDevices.find((d) => d.id === $("arv-camera").value);
}

function rangeHint(range, unit) {
  return range ? `${range.min}–${range.max}${unit}, now ${range.value}` : "";
}

function renderLevel(prefix, range, autoAvailable, unit) {
  const mode = $(`${prefix}-mode`);
  fillSelect(mode, levelModes(range, autoAvailable), mode.value || "unchanged");
  const input = $(prefix);
  input.hidden = mode.value !== "manual";
  if (range) {
    Object.assign(input, { min: range.min, max: range.max });
    if (input.value === "") input.value = String(range.value);
  }
  input.title = rangeHint(range, unit);
}

function renderAravisForm() {
  const previous = $("arv-camera").value;
  fillSelect(
    $("arv-camera"),
    aravisDevices.map((d) => ({ value: d.id, label: d.error ? `${d.id} — unavailable` : d.id })),
    previous,
  );
  const dev = selectedAravis();
  fillSelect($("arv-format"), (dev?.pixelFormats ?? []).map((p) => ({ value: p, label: p })), $("arv-format").value);
  fillSelect(
    $("arv-binning"),
    binningChoices(dev?.maxBinning ?? 1).map((b) => ({ value: String(b), label: b === 1 ? "none" : `${b}×${b}` })),
    $("arv-binning").value,
  );
  $("arv-fps").hidden = $("arv-fps-mode").value !== "manual";
  if (dev?.frameRate && $("arv-fps").value === "") $("arv-fps").value = String(dev.frameRate.value);
  $("arv-fps").title = rangeHint(dev?.frameRate, " fps");
  renderLevel("arv-exposure", dev?.exposure, dev?.exposureAuto, " µs");
  renderLevel("arv-gain", dev?.gain, dev?.gainAuto, "");
  const size = dev?.width && dev?.height ? `Sensor ROI ${dev.width}×${dev.height}. ` : "";
  $("arv-note").textContent = dev?.error
    ? dev.error
    : aravisDevices.length === 0
      ? "No GenICam camera found. Close other camera software (SpinView, pylon Viewer), then press ↻."
      : `${size}Settings apply when the stream starts; changing one restarts it.`;
}

function aravisFormValues() {
  return {
    camera: $("arv-camera").value,
    pixelFormat: $("arv-format").value,
    binning: $("arv-binning").value,
    fpsMode: $("arv-fps-mode").value,
    fps: $("arv-fps").value,
    exposureMode: $("arv-exposure-mode").value,
    exposure: $("arv-exposure").value,
    gainMode: $("arv-gain-mode").value,
    gain: $("arv-gain").value,
  };
}

async function loadAravis() {
  $("arv-note").textContent = "Scanning…";
  try {
    aravisDevices = await invoke("list_aravis");
  } catch (e) {
    aravisDevices = [];
    fail(e);
  }
  renderAravisForm();
}

async function applyAravis() {
  renderAravisForm();
  const dev = selectedAravis();
  if (dev && !dev.error) await sendConfig(aravisConfig(aravisFormValues()));
}

async function selectSource(source) {
  showSource(source);
  if (source === "dslr") {
    await sendConfig(dslrConfig());
  } else if (source === "camera") {
    if (cameras.length === 0) await loadCameras();
    if (cameraSelection) await sendConfig(cameraConfig(cameraSelection));
  } else {
    if (aravisDevices.length === 0) await loadAravis();
    await applyAravis();
  }
  await stopOtherSource(source);
}

/** Pause a producer left running for another source when the shown one has no usable camera. */
async function stopOtherSource(source) {
  try {
    const s = await invoke("get_status");
    if (s.playing && sourcePending(source, s)) await invoke("set_playing", { playing: false });
  } catch (e) {
    fail(e);
  }
  refresh();
}

function initialise(s) {
  $("url").value = s.streamUrl;
  $("format").replaceChildren(...s.jpegFormats.map((f) => new Option(f === "L" ? "L (full size)" : f, f)));
  if (s.config.source === "dslr") {
    $("format").value = s.config.imageFormat;
    $("interval").value = String(s.config.interval);
    showMode(s.config.mode);
  }
  $("interval").max = String(s.maxInterval);
  sourceButtons.find((b) => b.dataset.source === "camera").hidden = !s.cameraBackend;
  showSource(s.config.source);
  setPreview($("show-preview").checked);
  initialised = true;
}

function render(s) {
  current = s;
  if (!initialised) initialise(s);
  $("play").setAttribute("aria-pressed", String(s.playing));
  $("play").querySelector(".label").textContent = s.playing ? "Pause" : "Play";
  const pending = sourcePending(checked(sourceButtons, "source"), s);
  $("play").disabled = Boolean(s.serverError) || (pending && !s.playing);
  $("save").disabled = !s.hasFrame;
  $("arv-refresh").disabled = s.playing && s.config.source === "aravis";
  $("state").textContent = s.state;
  $("state").className = `badge ${s.state}`;
  $("details").textContent = describe(s);
  $("preview-empty").hidden = s.frames > 0 || s.width !== null;
  showError((pending ? null : errorLine(s)) ?? actionError);
}

async function refresh() {
  try {
    render(await invoke("get_status"));
  } catch (e) {
    fail(e);
  }
}

function setPreview(on) {
  const img = $("preview");
  if (on) {
    img.src = `${current ? current.streamUrl : $("url").value}?preview=${Date.now()}`;
  } else {
    img.removeAttribute("src");
  }
  img.hidden = !on;
}

function timestampStem(date) {
  const pad = (n) => String(n).padStart(2, "0");
  const day = `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())}`;
  const time = `${pad(date.getHours())}-${pad(date.getMinutes())}-${pad(date.getSeconds())}`;
  return `livestream-${day}_${time}`;
}

let noticeTimer = null;
function showNotice(message) {
  $("notice").textContent = message;
  $("notice").hidden = false;
  clearTimeout(noticeTimer);
  noticeTimer = setTimeout(() => ($("notice").hidden = true), 4000);
}

sourceButtons.forEach((b) =>
  b.addEventListener("click", () => {
    if (b.dataset.source !== checked(sourceButtons, "source")) selectSource(b.dataset.source);
  }),
);
modeButtons.forEach((b) =>
  b.addEventListener("click", () => {
    if (b.dataset.mode === checked(modeButtons, "mode")) return;
    showMode(b.dataset.mode);
    sendConfig(dslrConfig());
  }),
);
$("interval").addEventListener("change", () => sendConfig(dslrConfig()));
$("format").addEventListener("change", () => sendConfig(dslrConfig()));

$("cam-device").addEventListener("change", () => applyCamera("device"));
$("cam-format").addEventListener("change", () => applyCamera("format"));
$("cam-size").addEventListener("change", () => applyCamera("size"));
$("cam-fps").addEventListener("change", () => applyCamera("rate"));
$("cam-refresh").addEventListener("click", async () => {
  await loadCameras();
  if (cameraSelection) sendConfig(cameraConfig(cameraSelection));
});

["arv-camera", "arv-format", "arv-binning", "arv-fps-mode", "arv-fps", "arv-exposure-mode", "arv-exposure", "arv-gain-mode", "arv-gain"].forEach(
  (id) => $(id).addEventListener("change", applyAravis),
);
$("arv-refresh").addEventListener("click", async () => {
  await loadAravis();
  applyAravis();
});

$("play").addEventListener("click", async () => {
  await invoke("set_playing", { playing: !(current && current.playing) });
  refresh();
});
$("save").addEventListener("click", async () => {
  try {
    const path = await invoke("save_frame", { stem: timestampStem(new Date()) });
    showNotice(`Saved ${path}`);
  } catch (e) {
    fail(e);
  }
});
$("show-preview").addEventListener("change", (e) => setPreview(e.target.checked));
$("preview").addEventListener("error", () => setTimeout(() => setPreview($("show-preview").checked), 1000));
$("copy").addEventListener("click", async () => {
  const value = $("url").value;
  try {
    await navigator.clipboard.writeText(value);
  } catch {
    $("url").select();
    document.execCommand("copy");
  }
  $("copy").textContent = "Copied";
  setTimeout(() => ($("copy").textContent = "Copy"), 1200);
});

refresh();
setInterval(refresh, POLL_MS);
