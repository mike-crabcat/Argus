// Opens the Argus web viewer in Chrome and saves, in OUT_DIR:
//   grid.png      the camera grid (2x scale)
//   focus.png     one camera full window (main stream)
//   replay.png    synced replay after a seek (with --replay)
//   viewer.webm   a screen recording of all of it
//   summary.json  per-tile fps/latency, replay seek and sync numbers, and
//                 the GPU path actually used (WebGPU adapter, WebGL
//                 renderer, hardware decode)
//
// Usage: node capture.mjs URL OUT_DIR [--seconds N] [--focus CAMERA]
//                                     [--replay MS_SINCE_EPOCH]
// The URL may carry ?token=...; it is never printed.
//
// Headed Chrome on the desktop when there is a display (so the real GPU is
// used); headless otherwise or with ARGUS_CAPTURE_HEADLESS=1.

import fs from "node:fs";
import path from "node:path";
import { SIZE, launch, probeGpu } from "./browser.mjs";

const [url, out, ...rest] = process.argv.slice(2);
if (!url || !out) {
  console.error("usage: node capture.mjs URL OUT_DIR [--seconds N] [--focus CAMERA] [--replay MS]");
  process.exit(2);
}
const opt = (name, def) => {
  const i = rest.indexOf(name);
  return i >= 0 ? rest[i + 1] : def;
};
const seconds = Number(opt("--seconds", "15"));
const focusCamera = opt("--focus", null);
const replayAt = opt("--replay", null);
fs.mkdirSync(out, { recursive: true });

const { browser, headless, chrome } = await launch();
const context = await browser.newContext({ viewport: null, recordVideo: { dir: out, size: SIZE } });
const page = await context.newPage();
const errors = [];
page.on("pageerror", (e) => errors.push(String(e)));
page.on("console", (m) => m.type() === "error" && errors.push(m.text().slice(0, 300)));

const stats = async () => {
  const s = await page.evaluate(() => window.argusStats || null);
  return s ? JSON.parse(s) : { tiles: [] };
};

await page.goto(url);
const gpu = await probeGpu(page);

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

// Focus view on one camera (the first, unless named), then back. Tile
// rectangles come from the app itself (window.argusStats).
const names = grid.tiles.map((t) => t.camera);
const idx = Math.max(0, focusCamera ? names.indexOf(focusCamera) : 0);
const r = grid.tiles[idx]?.rect ?? [0, 44, SIZE.width, SIZE.height - 44];
const box = [r[0] + r[2] / 2, r[1] + r[3] / 2];
await page.mouse.move(box[0], box[1]);
await page.waitForTimeout(300);
await page.mouse.dblclick(box[0], box[1]);
await page.waitForTimeout(Math.max(3, seconds * 0.4) * 1000);
const focus = await stats();
await page.screenshot({ path: path.join(out, "focus.png") });
await page.keyboard.press("Escape");
await page.waitForTimeout(1500);

// Synced replay: switch with the top bar's REPLAY half, click the
// timeline at the requested time, and let it play.
let replay = null;
if (replayAt) {
  // The REPLAY half of the top bar's Live/Replay switch (right edge - 16,
  // 148 wide). Measured from the page: a headed window can be narrower.
  const vw = await page.evaluate(() => window.innerWidth);
  await page.mouse.click(vw - 16 - 37, 22);
  await page.waitForTimeout(2500);
  const s = await stats();
  if (s.timeline) {
    const [x, y, w, h, t0, t1] = s.timeline;
    const cx = x + ((Number(replayAt) - t0) / (t1 - t0)) * w;
    await page.mouse.move(cx, y + h / 2);
    await page.waitForTimeout(300);
    await page.mouse.click(cx, y + h / 2);
    await page.waitForTimeout(Math.max(6, seconds * 0.6) * 1000);
    replay = (await stats()).replay;
    await page.screenshot({ path: path.join(out, "replay.png") });
  } else {
    errors.push("this build has no replay timeline");
  }
}

const video = page.video();
await context.close();
if (video) fs.renameSync(await video.path(), path.join(out, "viewer.webm"));
await browser.close();

const summary = {
  captured_at: new Date().toISOString(),
  browser: chrome,
  headless,
  gpu,
  backend: grid.backend,
  grid: grid.tiles,
  focus: { camera: focus.focus, tile: focus.tiles.find((t) => t.camera === focus.focus) },
  replay,
  page_errors: errors.slice(0, 20),
};
fs.writeFileSync(path.join(out, "summary.json"), JSON.stringify(summary, null, 2));
for (const t of grid.tiles) {
  console.log(
    `${t.camera.padEnd(10)} ${t.stream} ${String(t.size)} ${t.shown_fps.toFixed(1)} fps, ` +
      `latency ${t.latency_ms == null ? "-" : t.latency_ms.toFixed(0) + " ms"}, dropped ${t.dropped}`,
  );
}
const ft = summary.focus.tile;
if (ft) {
  console.log(`focus ${ft.camera} ${ft.stream} ${String(ft.size)} ${ft.shown_fps.toFixed(1)} fps, latency ${ft.latency_ms?.toFixed(0)} ms, dropped ${ft.dropped}`);
}
if (replay) {
  const ff = replay.seek_to_first_frame_ms.map((v) => (v == null ? "-" : Math.round(v)));
  console.log(`replay seek->first frame [${ff.join(", ")}] ms, sync spread p50 ${replay.sync_spread_ms.p50?.toFixed(0)} p95 ${replay.sync_spread_ms.p95?.toFixed(0)} ms`);
}
console.log(`${headless ? "headless" : "headed"}, backend ${grid.backend}, WebGPU adapter ${gpu.webgpu_adapter ?? "none"}, hardware decode ${JSON.stringify(gpu.hardware_decode)}`);
console.log(`saved to ${out}`);
