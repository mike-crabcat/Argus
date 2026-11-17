// Opens the Argus web viewer in Chrome/Chromium and saves:
//   <out>/grid.png      the camera grid (2x scale)
//   <out>/focus.png     one camera full window (main stream)
//   <out>/viewer.webm   a screen recording: grid, focus, back to grid
//   <out>/summary.json  per-tile fps/latency from window.argusStats
//
// Usage: node capture.mjs URL OUT_DIR [--seconds N] [--focus CAMERA]
// The URL may carry ?token=...; it is never printed.
//
// Needs the `playwright` package (npm i -D playwright, or NODE_PATH pointing
// at an install). Uses Google Chrome when present (H.264 + GPU), else
// Playwright's Chromium.

import { createRequire } from "node:module";
import fs from "node:fs";
import path from "node:path";

const require = createRequire(import.meta.url);
const { chromium } = require("playwright");

const [url, out, ...rest] = process.argv.slice(2);
if (!url || !out) {
  console.error("usage: node capture.mjs URL OUT_DIR [--seconds N] [--focus CAMERA]");
  process.exit(2);
}
const opt = (name, def) => {
  const i = rest.indexOf(name);
  return i >= 0 ? rest[i + 1] : def;
};
const seconds = Number(opt("--seconds", "15"));
const focusCamera = opt("--focus", null);
fs.mkdirSync(out, { recursive: true });

const chrome = ["/usr/bin/google-chrome", "/usr/bin/google-chrome-stable"].find((p) => fs.existsSync(p));
const size = { width: 1440, height: 900 };
const browser = await chromium.launch({
  executablePath: chrome,
  args: [
    // A real 2x device scale (Playwright's emulated one confuses eframe's
    // canvas sizing), plus everything that might give WebGPU and hardware
    // video decoding a chance in headless mode.
    "--force-device-scale-factor=2",
    `--window-size=${size.width},${size.height}`,
    "--enable-unsafe-webgpu",
    "--enable-features=Vulkan,VaapiVideoDecoder",
    "--use-angle=vulkan",
    "--ignore-gpu-blocklist",
    "--enable-gpu-rasterization",
  ],
});
const context = await browser.newContext({
  viewport: null,
  recordVideo: { dir: out, size },
});
const page = await context.newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
page.on("console", (m) => m.type() === "error" && errors.push(m.text()));

const stats = async () => {
  const s = await page.evaluate(() => window.argusStats || null);
  return s ? JSON.parse(s) : { tiles: [] };
};

await page.goto(url);
const gpu = await page.evaluate(async () => ({
  webgpu: !!navigator.gpu && !!(await navigator.gpu.requestAdapter().catch(() => null)),
  webcodecs: typeof VideoDecoder === "function",
}));

// Grid: wait until tiles show frames (or time out), then let it run.
const start = Date.now();
while (Date.now() - start < 20_000) {
  const s = await stats();
  if (s.tiles.length && s.tiles.every((t) => t.size)) break;
  await page.waitForTimeout(500);
}
await page.waitForTimeout(Math.max(3, seconds * 0.6) * 1000);
const grid = await stats();
await page.screenshot({ path: path.join(out, "grid.png") });

// Focus view on one camera (the first, unless named), then back.
const names = grid.tiles.map((t) => t.camera);
const idx = Math.max(0, focusCamera ? names.indexOf(focusCamera) : 0);
// Tile rectangles come from the app itself (window.argusStats).
const r = grid.tiles[idx]?.rect ?? [0, 44, size.width, size.height - 44];
const box = [r[0] + r[2] / 2, r[1] + r[3] / 2];
await page.mouse.move(box[0], box[1]);
await page.waitForTimeout(300);
await page.mouse.dblclick(box[0], box[1]);
await page.waitForTimeout(Math.max(3, seconds * 0.4) * 1000);
const focus = await stats();
await page.screenshot({ path: path.join(out, "focus.png") });
await page.keyboard.press("Escape");
await page.waitForTimeout(1500);

const video = page.video();
await context.close();
if (video) fs.renameSync(await video.path(), path.join(out, "viewer.webm"));
await browser.close();

const summary = {
  captured_at: new Date().toISOString(),
  browser: chrome ? "google-chrome" : "playwright-chromium",
  webgpu_adapter: gpu.webgpu,
  webcodecs: gpu.webcodecs,
  backend: grid.backend,
  grid: grid.tiles,
  focus: { camera: focus.focus, tile: focus.tiles.find((t) => t.camera === focus.focus) },
  page_errors: errors.slice(0, 20),
};
fs.writeFileSync(path.join(out, "summary.json"), JSON.stringify(summary, null, 2));
for (const t of grid.tiles) {
  console.log(
    `${t.camera.padEnd(10)} ${t.stream} ${String(t.size)} ${t.shown_fps.toFixed(1)} fps, ` +
      `latency ${t.latency_ms == null ? "-" : t.latency_ms.toFixed(0) + " ms"}, dropped ${t.dropped}`,
  );
}
console.log(`backend ${grid.backend}, WebGPU adapter ${gpu.webgpu}, focus ${focus.focus}`);
console.log(`saved to ${out}`);
