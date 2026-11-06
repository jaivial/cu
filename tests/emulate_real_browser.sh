#!/usr/bin/env bash
# Real-browser check of 'emulate': the whole phone, kept through a page that
# reloads itself and through later cu commands.
#
# Everything is isolated: a daemon of its own (CU_DAEMON_DIR and HOME under
# $ROOT, HTTP 8795, DevTools 9395), the Chromium named by CU_BROWSER with a
# fresh profile, and a local fixture on 127.0.0.1:9599. No real site is
# visited. Only the processes this script starts are stopped, by the pids it
# recorded.
#
#   cargo build && bash tests/emulate_real_browser.sh
#
# Env: CU_BIN (default target/debug/cu), CU_BROWSER (default
# /usr/lib64/chromium-browser/chromium-browser), ROOT (default
# /tmp/cu-emulate-check).
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
CU_BIN=${CU_BIN:-$here/target/debug/cu}
ROOT=${ROOT:-/tmp/cu-emulate-check}
BROWSER=${CU_BROWSER:-/usr/lib64/chromium-browser/chromium-browser}
HTTP_PORT=8795
CDP_PORT=9395
FIXTURE_PORT=9599

for port in $HTTP_PORT $CDP_PORT $FIXTURE_PORT; do
  if (exec 3<>/dev/tcp/127.0.0.1/$port) 2>/dev/null; then
    echo "port $port is already in use; not touching whatever runs there" >&2
    exit 2
  fi
done

case "$ROOT" in /tmp/?*) ;; *) echo "ROOT must be under /tmp" >&2; exit 2;; esac
rm -rf "${ROOT:?}"
mkdir -p "$ROOT/home" "$ROOT/daemon" "$ROOT/fixture"

# The fixture: on every load it records what the page sees into localStorage;
# its button reloads the page 300 ms after a click, as Instagram's editor does.
cat >"$ROOT/fixture/index.html" <<'EOF'
<!doctype html><meta name="viewport" content="width=device-width,initial-scale=1">
<title>emulate fixture</title>
<button id="b" style="width:200px;height:80px">reload me</button>
<pre id="out" style="white-space:pre-wrap;word-break:break-all"></pre>
<script>
var rec={w:innerWidth,h:innerHeight,sw:screen.width,sh:screen.height,
  o:screen.orientation&&screen.orientation.type,tp:navigator.maxTouchPoints,
  dpr:devicePixelRatio,ua:navigator.userAgent,
  uad:navigator.userAgentData?(navigator.userAgentData.mobile+' '+navigator.userAgentData.platform+' '+
    navigator.userAgentData.brands.map(function(b){return b.brand+'/'+b.version}).join(',')):'none'};
var loads=JSON.parse(localStorage.getItem('loads')||'[]');loads.push(rec);
localStorage.setItem('loads',JSON.stringify(loads));
document.getElementById('out').textContent=JSON.stringify(rec);
var ev=JSON.parse(localStorage.getItem('events')||'[]');
['pointerdown','touchstart','mousedown','click'].forEach(function(n){
  document.addEventListener(n,function(e){ev.push(n+(e.pointerType?':'+e.pointerType:''));
    localStorage.setItem('events',JSON.stringify(ev));},true)});
document.getElementById('b').addEventListener('click',function(){
  setTimeout(function(){location.reload()},300)});
</script>
EOF

pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${pids[@]}"; do
    for _ in $(seq 50); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
  done
}
trap cleanup EXIT

python3 -m http.server $FIXTURE_PORT --bind 127.0.0.1 --directory "$ROOT/fixture" \
  >"$ROOT/fixture.log" 2>&1 &
pids+=($!)

export HOME=$ROOT/home CU_DAEMON_DIR=$ROOT/daemon CU_PORT=$HTTP_PORT CU_CDP_PORT=$CDP_PORT \
  CU_BROWSER=$BROWSER
unset DISPLAY CU_DATA_DIR
"$CU_BIN" daemon >"$ROOT/daemon.log" 2>&1 &
pids+=($!)
for _ in $(seq 300); do
  [ -f "$ROOT/daemon/server.json" ] && "$CU_BIN" status >/dev/null 2>&1 && break
  sleep 0.2
done

# Every cu call runs in this shell, never in a $(...) or a pipeline: the CLI
# names its session after its parent process, and a subshell is a different
# parent -- and so a different tab.
out=$ROOT/out.json
cu() { "$CU_BIN" "$@" >"$out" 2>>"$ROOT/cli.log"; }
value() { python3 -c 'import json,sys; r=json.load(open(sys.argv[1]))["results"]; print(r[0].get("value", r))' "$out"; }
url=http://127.0.0.1:$FIXTURE_PORT/
report() { printf '%-36s %s\n' "$1" "$2"; }
load() {
  cu eval 'JSON.stringify(JSON.parse(localStorage.getItem("loads")).slice(-1)[0])'
  report "$1" "$(value)"
}

cu navigate "$url"
cu eval 'localStorage.clear(); "cleared"'
cu act '[{"do":"navigate","url":"'"$url"'"}]'
load "before (no emulation):"
cu act '[{"do":"emulate","mode":"mobile"},{"do":"navigate","url":"'"$url"'"}]'
report "emulate step says:" "$(value)"
load "after emulate mobile + navigate:"

cu snapshot
ref=$(grep 'reload me' "$out" | grep -o 'e[0-9][0-9]*' | head -1)
cu eval 'localStorage.setItem("events","[]"); "ok"'
cu click "$ref"
sleep 1.5
cu eval 'localStorage.getItem("events")'
report "events of 'cu click $ref':" "$(value)"
load "after the page reloaded itself:"
cu eval 'String(JSON.parse(localStorage.getItem("loads")).length)'
report "loads recorded so far:" "$(value)"

cu snapshot
cu text
cu shot "$ROOT/plain.jpg"
cu eval 'setTimeout(function(){location.reload()},100); "ok"'
sleep 1
load "after snapshot/text/shot, reload:"

cu shot "$ROOT/wide.jpg" --width 800 --height 600
cu eval 'setTimeout(function(){location.reload()},100); "ok"'
sleep 1
load "after shot --width 800, reload:"

cu act '[{"do":"emulate","mode":"desktop"},{"do":"navigate","url":"'"$url"'"}]'
load "after emulate desktop + navigate:"

echo
echo "logs: $ROOT/daemon.log $ROOT/cli.log"
