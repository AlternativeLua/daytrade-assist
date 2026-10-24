// Passes messages between the page script and the extension's background
// worker, which is the part allowed to reach the app on 127.0.0.1.
window.addEventListener('message', (event) => {
  if (event.source !== window || !event.data || event.data['daytrade-assist-bridge'] !== true) return;
  const { kind, rows } = event.data;
  if (!Array.isArray(rows)) return;
  chrome.runtime.sendMessage({ kind, rows }).catch(() => {});
});

chrome.runtime.onMessage.addListener((msg) => {
  if (msg && msg.type === 'order' && msg.order) {
    window.postMessage({ 'daytrade-assist-order': true, order: msg.order }, '*');
  }
});
