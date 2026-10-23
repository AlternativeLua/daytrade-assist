// Forwards ticks to Daytrade Assist. One small POST per message keeps latency
// to a few milliseconds; the badge shows whether the app is reachable.
const ENDPOINT = 'http://127.0.0.1:8787/ingest';
let connected = null;

function setStatus(ok) {
  if (ok === connected) return;
  connected = ok;
  chrome.action.setBadgeText({ text: ok ? 'ON' : 'OFF' });
  chrome.action.setBadgeBackgroundColor({ color: ok ? '#1fbf75' : '#f0485a' });
  chrome.action.setTitle({ title: ok ? 'Daytrade Assist: receiving prices' : 'Daytrade Assist: app not running' });
}

chrome.runtime.onMessage.addListener((msg) => {
  if (!msg || !Array.isArray(msg.rows) || !msg.rows.length) return;
  const lines = [msg.kind === 'history' ? 'history' : 'stream'];
  for (const [asset, time, price] of msg.rows) {
    if (typeof asset === 'string' && Number.isFinite(time) && Number.isFinite(price)) {
      lines.push(`${asset.replace(/[,\s]/g, '')},${time},${price}`);
    }
  }
  if (lines.length < 2) return;
  fetch(ENDPOINT, { method: 'POST', body: lines.join('\n'), headers: { 'Content-Type': 'text/plain' } })
    .then((r) => setStatus(r.ok), () => setStatus(false));
});
