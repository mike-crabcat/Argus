// Web Push for the Argus viewer (called from the wasm app as
// window.argusPush). Subscriptions are sent with the viewer's token, so
// each phone's are its own.
function b64ToBytes(s) {
  const pad = "=".repeat((4 - (s.length % 4)) % 4);
  const raw = atob((s + pad).replace(/-/g, "+").replace(/_/g, "/"));
  return Uint8Array.from(raw, (c) => c.charCodeAt(0));
}

const isIos = () => /iphone|ipad|ipod/i.test(navigator.userAgent) ||
  (navigator.platform === "MacIntel" && navigator.maxTouchPoints > 1);
const standalone = () => window.matchMedia("(display-mode: standalone)").matches || navigator.standalone === true;

async function registration() {
  return navigator.serviceWorker.register("/sw.js", { scope: "/" }).then(() => navigator.serviceWorker.ready);
}

window.argusPush = {
  // "unsupported", "ios-needs-home-screen", "denied", "off" or "on".
  async state() {
    if (!("serviceWorker" in navigator) || !("PushManager" in window) || !("Notification" in window)) {
      return isIos() && !standalone() ? "ios-needs-home-screen" : "unsupported";
    }
    if (Notification.permission === "denied") return "denied";
    const reg = await navigator.serviceWorker.getRegistration("/");
    const sub = reg && (await reg.pushManager.getSubscription());
    return sub ? "on" : "off";
  },
  async enable(token) {
    if (isIos() && !standalone()) return "ios-needs-home-screen";
    const perm = await Notification.requestPermission();
    if (perm !== "granted") return "denied";
    const auth = token ? { Authorization: "Bearer " + token } : {};
    const key = (await (await fetch("/api/push/key", { headers: auth })).json()).public_key;
    const reg = await registration();
    let sub = await reg.pushManager.getSubscription();
    if (!sub) sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: b64ToBytes(key) });
    const r = await fetch("/api/push/subscribe", {
      method: "POST",
      headers: { ...auth, "Content-Type": "application/json" },
      body: JSON.stringify(sub.toJSON()),
    });
    if (!r.ok) throw new Error("subscribe: HTTP " + r.status);
    return "on";
  },
  async disable(token) {
    const reg = await navigator.serviceWorker.getRegistration("/");
    const sub = reg && (await reg.pushManager.getSubscription());
    if (sub) {
      await fetch("/api/push/subscribe", {
        method: "DELETE",
        headers: { Authorization: "Bearer " + token, "Content-Type": "application/json" },
        body: JSON.stringify({ endpoint: sub.endpoint }),
      });
      await sub.unsubscribe();
    }
    return "off";
  },
  async test(token) {
    const r = await fetch("/api/push/test", { method: "POST", headers: { Authorization: "Bearer " + token } });
    return r.ok ? "sent" : "failed";
  },
};
