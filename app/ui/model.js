/**
 * Pure UI model: turns device descriptions from the backend into form
 * choices and form values into `set_config` payloads. No DOM access, so it
 * runs under `node --test` (see model.test.js).
 */

/** Frame rate `{num, den}` as a short label: "30", "29.97", "7.5". */
export function rateLabel(rate) {
  const fps = rate.num / rate.den;
  return String(Math.round(fps * 100) / 100);
}

export function rateKey(rate) {
  return `${rate.num}/${rate.den}`;
}

/** `fps` as a `{num, den}` fraction with millihertz precision. */
export function fpsToRate(fps) {
  return { num: Math.round(fps * 1000), den: 1000 };
}

function formatKey(mode) {
  return mode.format ?? "native";
}

function sizeKey(mode) {
  return `${mode.width}x${mode.height}`;
}

/** Distinct formats of a camera, in driver order. */
export function cameraFormats(device) {
  const seen = new Map();
  for (const m of device?.modes ?? []) {
    const key = formatKey(m);
    if (!seen.has(key)) {
      const label = m.format ? `${m.format} — ${m.formatLabel}` : m.formatLabel;
      seen.set(key, { key, label: m.transcoded && m.format ? `${label} (re-encoded)` : label, transcoded: m.transcoded });
    }
  }
  return [...seen.values()];
}

/** Sizes available for `format`, largest first. */
export function cameraSizes(device, format) {
  const modes = (device?.modes ?? []).filter((m) => formatKey(m) === format);
  return modes
    .slice()
    .sort((a, b) => b.width * b.height - a.width * a.height)
    .map(sizeKey);
}

function findMode(device, format, size) {
  return (device?.modes ?? []).find((m) => formatKey(m) === format && sizeKey(m) === size);
}

export function cameraRates(device, format, size) {
  return findMode(device, format, size)?.rates ?? [];
}

function keepOr(value, choices, fallback) {
  return choices.includes(value) ? value : fallback;
}

/**
 * Consistent camera selection, keeping `previous` choices that are still
 * offered and otherwise falling back to the first usable device, its first
 * format, its largest size and fastest rate.
 */
export function pickCamera(devices, previous = {}) {
  const usable = devices.filter((d) => d.modes.length > 0);
  const device = usable.find((d) => d.id === previous.device) ?? usable[0];
  if (!device) return null;
  const formats = cameraFormats(device).map((f) => f.key);
  const format = keepOr(previous.format, formats, formats[0]);
  const sizes = cameraSizes(device, format);
  const size = keepOr(previous.size, sizes, sizes[0]);
  const rates = cameraRates(device, format, size).map(rateKey);
  const rate = keepOr(previous.rate, rates, rates[0]);
  return { device: device.id, format, size, rate };
}

/** `set_config` payload for a camera selection made by {@link pickCamera}. */
export function cameraConfig(selection) {
  const [width, height] = selection.size.split("x").map(Number);
  const [num, den] = selection.rate.split("/").map(Number);
  return {
    source: "camera",
    device: selection.device,
    format: selection.format === "native" ? null : selection.format,
    width,
    height,
    fps: { num, den },
  };
}

/** Level choices for a GenICam setting: auto only when the camera has it, manual only with a range. */
export function levelModes(range, autoAvailable) {
  const modes = [{ value: "unchanged", label: "camera" }];
  if (autoAvailable) modes.push({ value: "auto", label: "auto" });
  if (range) modes.push({ value: "manual", label: "manual" });
  return modes;
}

export function levelDto(mode, input) {
  const value = Number(input);
  if (mode === "manual" && input !== "" && Number.isFinite(value)) return { mode: "manual", value };
  if (mode === "auto") return { mode: "auto" };
  return { mode: "unchanged" };
}

export function binningChoices(max) {
  return Array.from({ length: Math.max(1, Math.min(max, 8)) }, (_, i) => i + 1);
}

/** `set_config` payload for the GenICam form values. */
export function aravisConfig(form) {
  const fps = Number(form.fps);
  return {
    source: "aravis",
    camera: form.camera,
    pixelFormat: form.pixelFormat,
    binning: Number(form.binning) || 1,
    fps: form.fpsMode === "manual" && form.fps !== "" && fps > 0 ? fpsToRate(fps) : null,
    exposure: levelDto(form.exposureMode, form.exposure),
    gain: levelDto(form.gainMode, form.gain),
  };
}

/** One-line status details: size, rate, frame and client counts. */
export function describe(s) {
  const parts = [];
  if (s.width && s.height) parts.push(`${s.width}×${s.height}`);
  if (s.state === "streaming") {
    const stills = s.config.source === "dslr" && s.config.mode === "stills";
    parts.push(stills && s.fps > 0 ? `${(1 / s.fps).toFixed(1)} s/frame` : `${s.fps.toFixed(1)} fps`);
  }
  parts.push(`${s.frames} frames`);
  parts.push(`${s.clients} client${s.clients === 1 ? "" : "s"}`);
  return parts.join(" · ");
}

/** Error line to show, most fundamental problem first. */
export function errorLine(s) {
  return s.serverError || s.missingTool || s.lastError || null;
}

/** Camera note: device error, or the re-encoding cost of the chosen format. */
export function cameraNote(device, format) {
  if (!device) return "No camera found. Plug one in and press ↻.";
  if (device.error) return device.error;
  const f = cameraFormats(device).find((x) => x.key === format);
  return f?.transcoded ? "This format is re-encoded to JPEG (uses CPU). MJPG, when offered, is passed through untouched." : null;
}
