/**
 * Livestream UI: polls `get_status`, drives `set_playing` / `set_config`.
 * The preview is the same MJPEG URL DFTFringe consumes.
 */
const { invoke } = window.__TAURI__.core;

const POLL_MS = 500;
const $ = (id) => document.getElementById(id);
const modeButtons = [...document.querySelectorAll(".segmented [data-mode]")];

let current = null;
let initialised = false;

function selectedMode() {
  return modeButtons.find((b) => b.getAttribute("aria-checked") === "true").dataset.mode;
}

function showMode(mode) {
  modeButtons.forEach((b) => b.setAttribute("aria-checked", String(b.dataset.mode === mode)));
  $("stills-options").hidden = mode !== "stills";
}

function formConfig() {
  return {
    mode: selectedMode(),
    interval: Math.round(Number($("interval").value)),
    imageFormat: $("format").value,
  };
}

function showError(message) {
  $("error").textContent = message || "";
  $("error").hidden = !message;
}

function describe(s) {
  const parts = [];
  if (s.width && s.height) parts.push(`${s.width}×${s.height}`);
  if (s.state === "streaming") {
    parts.push(s.config.mode === "stills" && s.fps > 0 ? `${(1 / s.fps).toFixed(1)} s/frame` : `${s.fps.toFixed(1)} fps`);
  }
  parts.push(`${s.frames} frames`);
  parts.push(`${s.clients} client${s.clients === 1 ? "" : "s"}`);
  return parts.join(" · ");
}

function initialise(s) {
  $("url").value = s.streamUrl;
  $("format").replaceChildren(
    ...s.jpegFormats.map((f) => new Option(f === "L" ? "L (full size)" : f, f)),
  );
  $("format").value = s.config.imageFormat;
  $("interval").max = String(s.maxInterval);
  $("interval").value = String(s.config.interval);
  showMode(s.config.mode);
  setPreview($("show-preview").checked);
  initialised = true;
}

function render(s) {
  current = s;
  if (!initialised) initialise(s);
  $("play").setAttribute("aria-pressed", String(s.playing));
  $("play").querySelector(".label").textContent = s.playing ? "Pause" : "Play";
  $("play").disabled = Boolean(s.serverError);
  $("state").textContent = s.state;
  $("state").className = `badge ${s.state}`;
  $("details").textContent = describe(s);
  $("preview-empty").hidden = s.frames > 0 || s.width !== null;
  const gphotoMissing = s.gphoto2 ? null : "gphoto2 not found — install it with `brew install gphoto2`.";
  showError(s.serverError || s.lastError || gphotoMissing);
}

async function refresh() {
  try {
    render(await invoke("get_status"));
  } catch (e) {
    showError(String(e));
  }
}

async function applyConfig() {
  showMode(selectedMode());
  try {
    await invoke("set_config", { config: formConfig() });
  } catch (e) {
    showError(String(e));
  }
  refresh();
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

modeButtons.forEach((b) =>
  b.addEventListener("click", () => {
    if (b.dataset.mode === selectedMode()) return;
    showMode(b.dataset.mode);
    applyConfig();
  }),
);
$("interval").addEventListener("change", applyConfig);
$("format").addEventListener("change", applyConfig);
$("play").addEventListener("click", async () => {
  await invoke("set_playing", { playing: !(current && current.playing) });
  refresh();
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
