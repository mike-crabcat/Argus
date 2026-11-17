// Launching Chrome for captures, and finding out what the GPU path really is.

import { createRequire } from "node:module";
import fs from "node:fs";

const require = createRequire(import.meta.url);
const { chromium } = require("playwright");

export const SIZE = { width: 1440, height: 900 };

/// Headed Chrome on the desktop when there is a display (real GPU, WebGPU);
/// headless otherwise, or when ARGUS_CAPTURE_HEADLESS=1.
export async function launch() {
  const chrome = ["/usr/bin/google-chrome", "/usr/bin/google-chrome-stable"].find((p) =>
    fs.existsSync(p),
  );
  const display = !!(process.env.DISPLAY || process.env.WAYLAND_DISPLAY);
  const headless = !display || process.env.ARGUS_CAPTURE_HEADLESS === "1";
  const browser = await chromium.launch({
    executablePath: chrome,
    headless,
    args: [
      // A real 2x device scale (Playwright's emulated one confuses eframe's
      // canvas sizing).
      "--force-device-scale-factor=2",
      `--window-size=${SIZE.width},${SIZE.height}`,
      "--enable-unsafe-webgpu",
      // Hardware decode on Linux goes through VA-API; on NVIDIA that needs
      // nvidia-vaapi-driver installed. Harmless where it isn't.
      "--enable-features=Vulkan,VaapiVideoDecoder,VaapiIgnoreDriverChecks",
      "--ignore-gpu-blocklist",
    ],
  });
  return { browser, headless, chrome: chrome ? "google-chrome" : "playwright-chromium" };
}

/// What the page can actually use: WebGPU adapter, WebGL renderer, and
/// whether the browser offers hardware H.264 decoding.
export async function probeGpu(page) {
  return page.evaluate(async () => {
    const out = { webgpu_adapter: null, webgl_renderer: null, hardware_decode: {} };
    try {
      const a = await navigator.gpu?.requestAdapter();
      if (a) {
        const i = a.info || {};
        out.webgpu_adapter = [i.vendor, i.architecture, i.description || i.device]
          .filter(Boolean)
          .join(" / ");
      }
    } catch {}
    try {
      const gl = document.createElement("canvas").getContext("webgl2");
      const ext = gl?.getExtension("WEBGL_debug_renderer_info");
      out.webgl_renderer = ext ? gl.getParameter(ext.UNMASKED_RENDERER_WEBGL) : null;
    } catch {}
    for (const [codec, w, h] of [
      ["avc1.640033", 2560, 1920],
      ["avc1.64001F", 896, 672],
    ]) {
      try {
        const r = await VideoDecoder.isConfigSupported({
          codec,
          codedWidth: w,
          codedHeight: h,
          hardwareAcceleration: "prefer-hardware",
        });
        out.hardware_decode[codec] = r.supported;
      } catch (e) {
        out.hardware_decode[codec] = `error: ${e}`;
      }
    }
    return out;
  });
}
