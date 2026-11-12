#!/usr/bin/env bash
# Print the figma-mcp-go plugin wrapper for cu inject (replaces
# figma-fork-slim-bridge.mjs). It runs the plugin's code.js inside the Figma
# page with a shimmed figma.ui that talks to the MCP leader over a WebSocket,
# and returns "started ...", "already running" or "figma not ready".
#
#   figma-fork-wrapper.sh [PORT] [PLUGIN_JS] > wrapper.js
#   cu inject add figma-fork --match 'https://www.figma.com/design/*' \
#     --file wrapper.js --retry-ms 3000
set -euo pipefail
PORT="${1:-1996}" PLUGIN="${2:-/home/jaime/src/figma-mcp-go/plugin/code.js}" node - <<'JS'
const fs = require('fs');
const PORT = process.env.PORT;
const PLUGIN = process.env.PLUGIN;
function pluginWrapper() {
  const code = fs.readFileSync(PLUGIN, 'utf8');
  return '(() => {' +
    'if (window.__figmaForkGo) return "already running";' +
    'if (typeof figma !== "object" || !figma.root) return "figma not ready";' +
    'const WS_URL = "ws://localhost:' + PORT + '/ws";' +
    'let socket = null, handler = null, timer = null, lastIn = null, lastOut = null;' +
    'const deliver = (msg) => { lastIn = msg; if (handler) Promise.resolve().then(() => handler(msg)); };' +
    'const connect = () => {' +
    '  try { socket = new WebSocket(WS_URL); } catch (e) { socket = null; }' +
    '  if (!socket) { timer = setTimeout(connect, 1500); return; }' +
    '  socket.onopen = () => { window.__figmaForkGo.connected = true; deliver({ type: "ui-ready" }); };' +
    '  socket.onclose = () => { window.__figmaForkGo.connected = false; socket = null; if (!timer) timer = setTimeout(() => { timer = null; connect(); }, 1500); };' +
    '  socket.onerror = () => {};' +
    '  socket.onmessage = (event) => { try { deliver({ type: "server-request", payload: JSON.parse(event.data) }); } catch (e) {} };' +
    '};' +
    'const ui = {' +
    '  postMessage(msg) { lastOut = msg; if (msg && "requestId" in msg && socket && socket.readyState === 1) socket.send(JSON.stringify(msg)); },' +
    '  get onmessage() { return handler; }, set onmessage(fn) { handler = fn; },' +
    '  on(type, fn) { if (type === "message") handler = fn; },' +
    '  show() {}, hide() {}, close() {}, resize() {},' +
    '};' +
    'const shim = new Proxy({}, {' +
    '  get(_t, key) {' +
    '    if (key === "ui") return ui;' +
    '    if (key === "showUI" || key === "closePlugin") return () => {};' +
    '    const v = figma[key]; return typeof v === "function" ? v.bind(figma) : v;' +
    '  },' +
    '  set(_t, key, value) { figma[key] = value; return true; },' +
    '  has(_t, key) { return key in figma; },' +
    '});' +
    'window.__figmaForkGo = { connected: false, file: figma.root.name, port: "' + PORT + '",' +
    '  debug: () => ({ handler: typeof handler, socketState: socket ? socket.readyState : null, lastIn: lastIn && lastIn.payload && lastIn.payload.type, lastOut: lastOut && lastOut.type }) };' +
    'try { new Function("figma", "__html__", ' + JSON.stringify(code) + ')(shim, ""); }' +
    'catch (e) { window.__figmaForkGo.error = String(e && e.stack || e).slice(0, 600); }' +
    'connect();' +
    'return window.__figmaForkGo.error ? "started with error: " + window.__figmaForkGo.error : "started in " + figma.root.name + " (port ' + PORT + ')";' +
    '})()';
}
process.stdout.write(pluginWrapper() + '\n');
JS
