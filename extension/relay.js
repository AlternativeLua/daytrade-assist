// Passes price messages from the page script to the extension's background
// worker, which is allowed to reach the app on 127.0.0.1.
window.addEventListener('message', (event) => {
  if (event.source !== window || !event.data || event.data['daytrade-assist-bridge'] !== true) return;
  const { kind, rows } = event.data;
  if (!Array.isArray(rows)) return;
  chrome.runtime.sendMessage({ kind, rows }).catch(() => {});
});
