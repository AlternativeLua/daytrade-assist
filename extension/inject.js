// Runs in the page itself, before Pocket Option's scripts, and listens to the
// websocket the page opens.
//
// Pocket Option uses socket.io. Prices arrive as a binary attachment to an
// `updateStream` event, shaped [["EURUSD_otc", <unix seconds>, <price>], ...],
// chart history as {asset, period, history: [[<unix seconds>, <price>], ...]},
// the asset table (`updateAssets`) as rows of
// [id, symbol, name, kind, digits, payout%, ..., isOpen (index 14), ...],
// and the account balance as {isDemo, balance} (`successupdateBalance`).
//
// When Daytrade Assist sends an order, it is placed with the same socket.io
// `openOrder` message the site's own Buy buttons use, and only on a socket
// whose `auth` message matches the account the order is for: demo orders on a
// demo login, real-money orders (which the app only sends after the user has
// confirmed twice) on a real-money login. A mismatch is refused, never guessed.
(() => {
  if (window.__daytradeAssistBridge) return;
  window.__daytradeAssistBridge = true;

  const TAG = 'daytrade-assist-bridge';
  const TAG_IN = 'daytrade-assist-order';
  const decoder = new TextDecoder();
  const NativeWebSocket = window.WebSocket;
  const sockets = new Set();
  const placed = new Map(); // requestId -> dealId, for orders this bridge placed
  // What this page is doing, reported to the app so problems can be diagnosed.
  const diag = { sockets: [], workers: [] };

  const emit = (kind, rows) => {
    if (rows.length) window.postMessage({ [TAG]: true, kind, rows }, '*');
  };

  const isTick = (x) =>
    Array.isArray(x) && x.length >= 3 && typeof x[0] === 'string' &&
    typeof x[1] === 'number' && typeof x[2] === 'number';

  const isAssetRow = (x) =>
    Array.isArray(x) && x.length >= 15 && typeof x[1] === 'string' &&
    typeof x[3] === 'string' && typeof x[5] === 'number';

  const isDeal = (d) => d && typeof d === 'object' && 'openPrice' in d && 'asset' in d;

  function ours(deal) {
    const req = deal.requestId != null ? String(deal.requestId) : '';
    if (placed.has(req)) return req;
    for (const [r, id] of placed) if (id && id === deal.id) return r;
    return null;
  }

  function onDeals(event, payload) {
    const deals = Array.isArray(payload) ? payload : Array.isArray(payload?.deals) ? payload.deals : isDeal(payload) ? [payload] : [];
    const rows = [];
    for (const d of deals.filter(isDeal)) {
      const req = ours(d);
      if (req == null) continue;
      if (event === 'successopenOrder') {
        placed.set(req, d.id);
        rows.push(['opened', req, d.id ?? '', d.openPrice ?? '']);
      } else if (d.closePrice != null && d.profit != null) {
        rows.push(['closed', req, d.id ?? '', d.profit, d.closePrice]);
        placed.delete(req);
      }
    }
    emit('orders', rows);
  }

  function handleEvent(socket, name, data) {
    socket.__daLast = Date.now();
    if (socket.__daDiag && socket.__daDiag.events.length < 40 && !socket.__daDiag.events.includes(name)) socket.__daDiag.events.push(name);
    if (name === 'successopenOrder' || name === 'successcloseOrder' || name === 'updateClosedDeals') {
      onDeals(name, data);
    } else if (/balance/i.test(name) && data && typeof data === 'object' && Number.isFinite(Number(data.balance))) {
      // Which account: the message says, or else the account this socket logged in to.
      const demo = 'isDemo' in data ? Number(data.isDemo) === 1 : socket.__daDemo;
      if (demo !== undefined) emit('balance', [[demo ? 1 : 0, Number(data.balance)]]);
    } else if (name === 'failopenOrder') {
      const req = data && data.requestId != null ? String(data.requestId) : [...placed.keys()].pop();
      if (req && placed.has(req)) {
        placed.delete(req);
        emit('orders', [['failed', req, String(data?.error ?? 'rejected')]]);
      }
    } else {
      handle(data);
    }
  }

  function handle(payload) {
    if (payload == null) return;
    if (Array.isArray(payload)) {
      if (payload.length && payload.every(isTick)) {
        emit('stream', payload.map(([asset, time, price]) => [asset, time, price]));
      } else if (payload.length && payload.every(isAssetRow)) {
        emit('assets', payload.map((r) => [r[1], r[5], r[14] ? 1 : 0, r[3], String(r[2] ?? r[1])]));
      }
      return;
    }
    if (typeof payload !== 'object') return;
    const data = payload.data && typeof payload.data === 'object' && !Array.isArray(payload.data) ? payload.data : payload;
    const asset = typeof data.asset === 'string' ? data.asset : payload.asset;
    if (typeof asset === 'string' && Array.isArray(data.history)) {
      emit('history', data.history
        .filter((h) => Array.isArray(h) && typeof h[0] === 'number' && typeof h[1] === 'number')
        .map(([time, price]) => [asset, time, price]));
    }
  }

  function parseJson(text) {
    // Text frames carry engine.io/socket.io prefixes such as `42` or `451-`.
    const body = text.replace(/^[\x00-\x1f]*\d*-?/, '');
    if (body[0] !== '[' && body[0] !== '{') return undefined;
    try {
      return JSON.parse(body);
    } catch {
      return undefined;
    }
  }

  function onFrame(socket, text, binary) {
    if (socket.__daDiag) socket.__daDiag.frames++;
    const msg = parseJson(text);
    if (msg === undefined) return;
    if (binary) {
      // The payload of the event announced by the previous text frame.
      const name = socket.__daPending;
      socket.__daPending = null;
      if (name) handleEvent(socket, name, msg);
      else handle(msg);
    } else if (Array.isArray(msg) && typeof msg[0] === 'string') {
      if (msg[1] && msg[1]._placeholder) socket.__daPending = msg[0];
      else handleEvent(socket, msg[0], msg[1]);
    } else {
      handle(msg);
    }
  }

  function placeOrder(o) {
    const reject = (why) => emit('orders', [['failed', o.requestId, why]]);
    const wantDemo = o.account !== 'real';
    if (o.account !== 'demo' && o.account !== 'real') return reject('unknown account');
    const candidates = [...sockets].filter((s) => s.readyState === 1 && s.__daAuthed);
    const matching = candidates.filter((s) => s.__daDemo === wantDemo).sort((a, b) => (b.__daLast || 0) - (a.__daLast || 0));
    if (!matching.length) {
      if (candidates.length) {
        return reject(wantDemo
          ? 'this tab is logged in to the real-money account, but the app is set to demo; switch Pocket Option to the demo account'
          : 'this tab is logged in to the demo account, but the app is set to real money; switch Pocket Option to the real account');
      }
      return reject('no logged-in Pocket Option connection in this tab; reload the tab');
    }
    placed.set(o.requestId, null);
    const order = {
      asset: o.asset,
      amount: o.amount,
      action: o.action === 'call' ? 'call' : 'put',
      isDemo: wantDemo ? 1 : 0,
      requestId: /^\d+$/.test(o.requestId) ? Number(o.requestId) : o.requestId,
      optionType: 100,
      time: o.time,
    };
    NativeWebSocket.prototype.send.call(matching[0], '42' + JSON.stringify(['openOrder', order]));
  }

  window.addEventListener('message', (event) => {
    // Only the tag is checked: Firefox doesn't report the content script as `window`.
    if (!event.data || event.data[TAG_IN] !== true) return;
    const o = event.data.order;
    if (o && typeof o.asset === 'string' && Number.isFinite(o.amount) && o.amount > 0 && Number.isFinite(o.time)) placeOrder(o);
  });

  class BridgedWebSocket extends NativeWebSocket {
    constructor(...args) {
      super(...args);
      sockets.add(this);
      let host = '?';
      try { host = new URL(String(args[0]), location.href).host; } catch {}
      this.__daDiag = { host, frames: 0, events: [], login: 'none' };
      diag.sockets.push(this.__daDiag);
      if (diag.sockets.length > 10) diag.sockets.shift();
      this.addEventListener('close', () => sockets.delete(this));
      this.addEventListener('message', (event) => {
        const data = event.data;
        try {
          if (typeof data === 'string') onFrame(this, data, false);
          else if (data instanceof ArrayBuffer) onFrame(this, decoder.decode(data), true);
          else if (data instanceof Blob) data.text().then((t) => onFrame(this, t, true), () => {});
        } catch {}
      });
    }

    send(data) {
      // Note which account this socket logs in to; nothing is changed.
      if (typeof data === 'string' && data.includes('"auth"')) {
        const msg = parseJson(data);
        if (Array.isArray(msg) && msg[0] === 'auth' && msg[1] && typeof msg[1] === 'object') {
          this.__daAuthed = true;
          this.__daDemo = msg[1].isDemo === 1 || msg[1].isDemo === true || msg[1].isDemo === '1';
          this.__daDiag.login = this.__daDemo ? 'demo' : 'real';
        }
      }
      return super.send(data);
    }
  }
  window.WebSocket = BridgedWebSocket;

  // Note workers the page starts: a price connection inside one can't be seen from here.
  for (const name of ['Worker', 'SharedWorker']) {
    const Native = window[name];
    if (typeof Native !== 'function') continue;
    window[name] = class extends Native {
      constructor(url, ...rest) {
        super(url, ...rest);
        if (diag.workers.length < 10) diag.workers.push(`${name}: ${String(url).slice(0, 120)}`);
      }
    };
  }

  const report = () => emit('diag', [[JSON.stringify({ url: location.origin + location.pathname, top: window === window.top, ...diag })]]);
  report();
  setInterval(report, 5000);
})();
