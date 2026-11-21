import { test } from "node:test";
import assert from "node:assert/strict";

import {
  aravisConfig,
  binningChoices,
  cameraConfig,
  cameraFormats,
  cameraNote,
  cameraSizes,
  describe,
  errorLine,
  fpsToRate,
  levelDto,
  levelModes,
  pickCamera,
  rateLabel,
} from "./model.js";

const r = (num, den = 1) => ({ num, den });
const mode = (format, width, height, rates, transcoded = format !== "MJPG") => ({
  format,
  formatLabel: format === "MJPG" ? "Motion-JPEG" : "YUYV 4:2:2",
  width,
  height,
  rates,
  transcoded,
});
const usb = {
  id: "/dev/video0",
  name: "USB Camera",
  modes: [mode("MJPG", 1280, 720, [r(30), r(15)]), mode("MJPG", 1920, 1080, [r(30)]), mode("YUYV", 640, 480, [r(30)])],
  error: null,
};
const h264 = { id: "/dev/video2", name: "USB Camera", modes: [mode("H264", 1280, 720, [r(30)])], error: null };
const denied = { id: "/dev/video4", name: "/dev/video4", modes: [], error: "permission denied" };

test("rate labels and conversions", () => {
  assert.equal(rateLabel(r(30)), "30");
  assert.equal(rateLabel(r(30000, 1001)), "29.97");
  assert.equal(rateLabel(r(10000000, 333333)), "30");
  assert.deepEqual(fpsToRate(7.5), r(7500, 1000));
});

test("formats and sizes", () => {
  assert.deepEqual(
    cameraFormats(usb).map((f) => f.label),
    ["MJPG — Motion-JPEG", "YUYV — YUYV 4:2:2 (re-encoded)"],
  );
  assert.deepEqual(cameraSizes(usb, "MJPG"), ["1920x1080", "1280x720"]);
  assert.deepEqual(cameraFormats({ modes: [{ format: null, formatLabel: "native", transcoded: true }] })[0].key, "native");
});

test("pickCamera defaults to first usable device, largest size, fastest rate", () => {
  assert.deepEqual(pickCamera([denied, usb, h264]), { device: "/dev/video0", format: "MJPG", size: "1920x1080", rate: "30/1" });
  assert.equal(pickCamera([denied]), null);
});

test("pickCamera keeps previous choices that still exist", () => {
  const previous = { device: "/dev/video0", format: "MJPG", size: "1280x720", rate: "15/1" };
  assert.deepEqual(pickCamera([usb], previous), previous);
  assert.deepEqual(pickCamera([usb], { ...previous, format: "YUYV" }), {
    device: "/dev/video0",
    format: "YUYV",
    size: "640x480",
    rate: "30/1",
  });
  assert.equal(pickCamera([usb, h264], { device: "/dev/video2" }).format, "H264");
});

test("camera config payload", () => {
  assert.deepEqual(cameraConfig({ device: "/dev/video0", format: "MJPG", size: "1280x720", rate: "30/1" }), {
    source: "camera",
    device: "/dev/video0",
    format: "MJPG",
    width: 1280,
    height: 720,
    fps: r(30),
  });
  assert.equal(cameraConfig({ device: "FaceTime", format: "native", size: "640x480", rate: "30/1" }).format, null);
});

test("camera notes", () => {
  assert.match(cameraNote(undefined, null), /No camera found/);
  assert.equal(cameraNote(denied, null), "permission denied");
  assert.equal(cameraNote(usb, "MJPG"), null);
  assert.match(cameraNote(usb, "YUYV"), /re-encoded/);
});

test("level modes and payloads", () => {
  assert.deepEqual(levelModes(null, false).map((m) => m.value), ["unchanged"]);
  assert.deepEqual(levelModes({ min: 0, max: 1 }, true).map((m) => m.value), ["unchanged", "auto", "manual"]);
  assert.deepEqual(levelDto("manual", "5000"), { mode: "manual", value: 5000 });
  assert.deepEqual(levelDto("manual", ""), { mode: "unchanged" });
  assert.deepEqual(levelDto("manual", "abc"), { mode: "unchanged" });
  assert.deepEqual(levelDto("auto", "12"), { mode: "auto" });
  assert.deepEqual(binningChoices(16), [1, 2, 3, 4, 5, 6, 7, 8]);
  assert.deepEqual(binningChoices(0), [1]);
});

test("aravis config payload", () => {
  const form = {
    camera: "Basler-1",
    pixelFormat: "Mono8",
    binning: "2",
    fpsMode: "manual",
    fps: "12.5",
    exposureMode: "manual",
    exposure: "8000",
    gainMode: "unchanged",
    gain: "3",
  };
  assert.deepEqual(aravisConfig(form), {
    source: "aravis",
    camera: "Basler-1",
    pixelFormat: "Mono8",
    binning: 2,
    fps: r(12500, 1000),
    exposure: { mode: "manual", value: 8000 },
    gain: { mode: "unchanged" },
  });
  assert.equal(aravisConfig({ ...form, fpsMode: "unchanged" }).fps, null);
  assert.equal(aravisConfig({ ...form, fps: "0" }).fps, null);
});

test("status description and errors", () => {
  const base = { width: 1280, height: 720, state: "streaming", fps: 30, frames: 5, clients: 1, config: { source: "camera" } };
  assert.equal(describe(base), "1280×720 · 30.0 fps · 5 frames · 1 client");
  assert.equal(describe({ ...base, fps: 0.5, config: { source: "dslr", mode: "stills" } }), "1280×720 · 2.0 s/frame · 5 frames · 1 client");
  assert.equal(errorLine({ serverError: null, missingTool: "ffmpeg not found", lastError: "exited" }), "ffmpeg not found");
  assert.equal(errorLine({}), null);
});
