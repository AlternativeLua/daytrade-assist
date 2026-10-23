// Runs in the page itself, before Pocket Option's scripts, and listens to the
// price websocket the page opens. It only reads messages; it never sends any.
//
// Pocket Option uses socket.io. Prices arrive as a binary attachment to an
// `updateStream` event, shaped [["EURUSD_otc", <unix seconds>, <price>], ...],
// and chart history as {asset, period, history: [[<unix seconds>, <price>], ...]}.
(() => {
  if (window.__daytradeAssistBridge) return;
  window.__daytradeAssistBridge = true;

  const TAG = 'daytrade-assist-bridge';
  const decoder = new TextDecoder();
  const NativeWebSocket = window.WebSocket;

  const emit = (kind, rows) => {
    if (rows.length) window.postMessage({ [TAG]: true, kind, rows }, '*');
  };

  const isTick = (x) =>
    Array.isArray(x) && x.length >= 3 && typeof x[0] === 'string' &&
    typeof x[1] === 'number' && typeof x[2] === 'number';

  function handle(payload) {
    if (payload == null) return;
    if (Array.isArray(payload)) {
      if (payload.length && payload.every(isTick)) {
        emit('stream', payload.map(([asset, time, price]) => [asset, time, price]));
      } else if (typeof payload[0] === 'string' && payload.length >= 2) {
        handle(payload[1]); // socket.io event: ["name", data]
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

  function parse(text) {
    // Text frames carry engine.io/socket.io prefixes such as `42` or `451-`.
    const body = text.replace(/^[\x00-\x1f]*\d*-?/, '');
    if (body[0] !== '[' && body[0] !== '{') return;
    try {
      handle(JSON.parse(body));
    } catch {}
  }

  function onMessage(event) {
    const data = event.data;
    try {
      if (typeof data === 'string') parse(data);
      else if (data instanceof ArrayBuffer) parse(decoder.decode(data));
      else if (data instanceof Blob) data.text().then(parse, () => {});
    } catch {}
  }

  class BridgedWebSocket extends NativeWebSocket {
    constructor(...args) {
      super(...args);
      this.addEventListener('message', onMessage);
    }
  }
  window.WebSocket = BridgedWebSocket;
})();
