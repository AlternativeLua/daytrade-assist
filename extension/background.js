// Forwards prices, payouts and order results to Daytrade Assist, and passes the
// app's orders to the Pocket Option tab that streams that pair. One small POST
// per message keeps latency to a few milliseconds; the badge shows whether the
// app is reachable.
const APP = 'http://127.0.0.1:8787';
const tabs = new Map(); // asset -> { tabId, frameId }
let connected = null;

function setStatus(ok) {
  if (ok === connected) return;
  connected = ok;
  chrome.action.setBadgeText({ text: ok ? 'ON' : 'OFF' });
  chrome.action.setBadgeBackgroundColor({ color: ok ? '#1fbf75' : '#f0485a' });
  chrome.action.setTitle({ title: ok ? 'Daytrade Assist: connected' : 'Daytrade Assist: app not running' });
}

const clean = (s) => String(s).replace(/[,\r\n]/g, ' ').trim();

function encode(msg) {
  if (msg.kind === 'assets') {
    const lines = ['assets'];
    for (const [asset, payout, open, kind, name] of msg.rows) {
      if (typeof asset === 'string' && Number.isFinite(payout)) {
        lines.push(`${clean(asset).replace(/\s/g, '')},${payout},${open ? 1 : 0},${clean(kind)},${clean(name)}`);
      }
    }
    return lines;
  }
  if (msg.kind === 'orders') {
    return ['orders', ...msg.rows.map((r) => r.map(clean).join(','))];
  }
  const lines = [msg.kind === 'history' ? 'history' : 'stream'];
  for (const [asset, time, price] of msg.rows) {
    if (typeof asset === 'string' && Number.isFinite(time) && Number.isFinite(price)) {
      lines.push(`${asset.replace(/[,\s]/g, '')},${time},${price}`);
    }
  }
  return lines;
}

chrome.runtime.onMessage.addListener((msg, sender) => {
  if (!msg || !Array.isArray(msg.rows) || !msg.rows.length) return;
  if (msg.kind === 'stream' && sender.tab) {
    for (const [asset] of msg.rows) tabs.set(asset, { tabId: sender.tab.id, frameId: sender.frameId ?? 0 });
  }
  const lines = encode(msg);
  if (lines.length < 2) return;
  fetch(`${APP}/ingest`, { method: 'POST', body: lines.join('\n'), headers: { 'Content-Type': 'text/plain' } })
    .then((r) => setStatus(r.ok), () => setStatus(false));
});

// Orders: `order,requestId,asset,call|put,amount,expirySeconds,demo|real` lines, long-polled.
function dispatch(line) {
  const [kind, requestId, asset, action, amount, time, account] = line.split(',');
  if (kind !== 'order') return;
  const order = { requestId, asset, action, amount: Number(amount), time: Number(time), account };
  const target = tabs.get(asset);
  const fail = (why) => fetch(`${APP}/ingest`, { method: 'POST', body: `orders\nfailed,${requestId},${clean(why)}` }).catch(() => {});
  if (!target) return fail(`no open Pocket Option tab is streaming ${asset}`);
  chrome.tabs.sendMessage(target.tabId, { type: 'order', order }, { frameId: target.frameId }).catch(() => fail('the Pocket Option tab did not respond; reload it'));
}

let polling = false;
async function pollOrders() {
  if (polling) return;
  polling = true;
  for (;;) {
    try {
      const r = await fetch(`${APP}/bridge/orders`, { cache: 'no-store' });
      setStatus(true);
      if (r.status === 200) (await r.text()).split('\n').forEach(dispatch);
    } catch {
      setStatus(false);
      await new Promise((done) => setTimeout(done, 2000));
    }
  }
}
pollOrders();
