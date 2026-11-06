#!/usr/bin/env bash
# Real-browser check of JavaScript dialog handling and tab hygiene.
#
# Everything is isolated: a daemon of its own (CU_DAEMON_DIR and HOME under
# $ROOT, HTTP 8796, DevTools 9396), the Chromium named by CU_BROWSER with a
# fresh profile, and a local fixture on 127.0.0.1:9598. No real site is
# visited. Only the processes this script starts are stopped, by the pids it
# recorded.
#
#   cargo build && bash tests/dialog_real_browser.sh
#
# Env: CU_BIN (default target/debug/cu), CU_BROWSER (default
# /usr/lib64/chromium-browser/chromium-browser), ROOT (default
# /tmp/cu-dialog-check), SKIP_REAP=1 skips the ~75 s wait for the collector.
set -uo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
CU_BIN=${CU_BIN:-$here/target/debug/cu}
ROOT=${ROOT:-/tmp/cu-dialog-check}
BROWSER=${CU_BROWSER:-/usr/lib64/chromium-browser/chromium-browser}
HTTP_PORT=8796
CDP_PORT=9396
FIXTURE_PORT=9598

for port in $HTTP_PORT $CDP_PORT $FIXTURE_PORT; do
  if (exec 3<>/dev/tcp/127.0.0.1/$port) 2>/dev/null; then
    echo "port $port is already in use; not touching whatever runs there" >&2
    exit 2
  fi
done

case "$ROOT" in /tmp/?*) ;; *) echo "ROOT must be under /tmp" >&2; exit 2;; esac
rm -rf "${ROOT:?}"
mkdir -p "$ROOT/home" "$ROOT/daemon" "$ROOT/fixture"

# The fixture: a button arms a beforeunload handler (it needs the click's
# user activation, as on Instagram), three more open alert, confirm and
# prompt and write what the page got back into #out.
cat >"$ROOT/fixture/index.html" <<'EOF'
<!doctype html><title>dialog fixture</title>
<button id="arm">arm beforeunload</button>
<button id="alert">open alert</button>
<button id="confirm">open confirm</button>
<button id="prompt">open prompt</button>
<pre id="out"></pre>
<script>
var out=document.getElementById('out');
function log(s){out.textContent+=s+'\n';}
document.getElementById('arm').onclick=function(){
  window.onbeforeunload=function(e){e.preventDefault();e.returnValue='unsaved';return 'unsaved';};
  log('armed');};
document.getElementById('alert').onclick=function(){alert('hello from alert');log('alert returned');};
document.getElementById('confirm').onclick=function(){log('confirm='+confirm('delete it?'));};
document.getElementById('prompt').onclick=function(){log('prompt='+prompt('your name?','nobody'));};
</script>
EOF
cat >"$ROOT/fixture/other.html" <<'EOF'
<!doctype html><title>other page</title><p>the page after leaving</p>
EOF
# A page that shows an alert by itself, before anyone watches it.
cat >"$ROOT/fixture/late.html" <<'EOF'
<!doctype html><title>late</title><script>setTimeout(function(){alert('too early for cu')},500)</script>
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
unset DISPLAY CU_DATA_DIR CU_DIALOGS
"$CU_BIN" daemon >"$ROOT/daemon.log" 2>&1 &
daemon_pid=$!
pids+=($daemon_pid)
for _ in $(seq 300); do
  [ -f "$ROOT/daemon/server.json" ] && "$CU_BIN" status >/dev/null 2>&1 && break
  sleep 0.2
done

# Every cu call runs in this shell, never in a $(...): the CLI names its
# session after its parent process, and a subshell is a different parent.
out=$ROOT/out.json
err=$ROOT/err.txt
code=0
secs=0
cu() {
  local t0 t1
  t0=$(date +%s.%N)
  "$CU_BIN" "$@" >"$out" 2>"$err"; code=$?
  t1=$(date +%s.%N)
  secs=$(python3 -c "print(f'{$t1-$t0:.2f}')")
  cat "$err" >>"$ROOT/cli.log"
}
report() { printf '%-44s %s\n' "$1" "$2"; }
field() { python3 - "$out" "$1" <<'EOF'
import json,sys
try: d=json.load(open(sys.argv[1]))
except Exception: print(open(sys.argv[1]).read().strip()[:160]); sys.exit()
for k in sys.argv[2].split('.'):
    d = d[int(k)] if isinstance(d, list) and len(d) > int(k) else (d.get(k) if isinstance(d, dict) else None)
print(d if not isinstance(d,(dict,list)) else json.dumps(d))
EOF
}
errline() { tr '\n' ' ' <"$err" | grep -o 'cu: {.*' | cut -c1-220; }
ref_of() {
  "$CU_BIN" snapshot 2>>"$ROOT/cli.log" | python3 -c '
import re, sys
text = sys.stdin.read().replace("\\\"", "\"")
m = re.search(r"ref=(e[0-9]+) \S+ \"" + re.escape(sys.argv[1]), text)
print(m.group(1) if m else "")' "$1"
}
pagetext() { cu eval 'document.getElementById("out").textContent.trim().split("\n").join(" | ")'; }
tabcount() { "$CU_BIN" tabs 2>/dev/null | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["tabs"]))'; }
url=http://127.0.0.1:$FIXTURE_PORT/

echo "== beforeunload under the default policy"
cu navigate "$url"
report "navigate fixture:" "exit $code in ${secs}s"
arm=$(ref_of 'arm beforeunload')
cu click "$arm"
report "click 'arm beforeunload' ($arm):" "exit $code in ${secs}s"
pagetext; report "page says:" "$(field results.0.value)"
cu navigate "${url}other.html"
report "navigate away (beforeunload armed):" "exit $code in ${secs}s"
report "  reply dialogs:" "$(field dialogs)"
cu eval 'document.title'
report "eval document.title after:" "exit $code in ${secs}s -> $(field results.0.value)"

echo "== alert / confirm / prompt under the default policy"
cu navigate "$url"
for b in alert confirm prompt; do
  r=$(ref_of "open $b")
  cu click "$r"
  report "click 'open $b':" "exit $code in ${secs}s, $(field dialogs.0.type)=$(field dialogs.0.action)"
done
pagetext; report "page got:" "$(field results.0.value)"

echo "== a tab's own policy: confirm=accept, prompt=accept"
cu dialog policy confirm=accept,prompt=accept
report "cu dialog policy:" "exit $code -> $(field policy)"
for b in confirm prompt; do
  r=$(ref_of "open $b"); cu click "$r"
done
pagetext; report "page got:" "$(field results.0.value)"

echo "== manual: commands fail fast, cu dialog answers"
cu dialog policy all=manual
r=$(ref_of 'open confirm')
cu click "$r"
report "click 'open confirm' (manual):" "exit $code in ${secs}s"
report "  error:" "$(errline)"
cu eval 'document.title'
report "eval while it waits:" "exit $code in ${secs}s"
cu snapshot
report "snapshot while it waits:" "exit $code in ${secs}s"
cu dialog
report "cu dialog pending:" "$(field pending)"
cu dialog accept
report "cu dialog accept:" "exit $code in ${secs}s -> $(field action)"
pagetext; report "page got:" "$(field results.0.value)"
r=$(ref_of 'open prompt'); cu click "$r"
report "click 'open prompt' (manual):" "exit $code in ${secs}s"
cu dialog accept "Ada Lovelace"
report "cu dialog accept 'Ada Lovelace':" "exit $code -> $(field action)"
pagetext; report "page got:" "$(field results.0.value)"
arm=$(ref_of 'arm beforeunload'); cu click "$arm"
cu navigate "${url}other.html"
report "navigate away (manual beforeunload):" "exit $code in ${secs}s"
report "  error:" "$(errline)"
before=$(tabcount)
for i in 1 2 3 4 5; do cu eval 'document.title'; done
report "5 more calls of this session:" "last exit $code in ${secs}s"
report "tabs before / after them:" "$before / $(tabcount)"
cu dialog dismiss
report "cu dialog dismiss (stay):" "exit $code -> $(field action)"
cu eval 'location.pathname'
report "still on:" "$(field results.0.value)"
cu dialog policy default
cu dialog
report "log entries:" "$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1]))["log"]))' "$out")"
cu console --level dialog
report "cu console --level dialog (lines):" "$(grep -c DIALOG "$out")"
grep DIALOG "$out" | head -2 | sed 's/^/    /'

echo "== a dialog that opened before cu watched the tab"
late=$(curl -s -X PUT "http://127.0.0.1:$CDP_PORT/json/new?${url}late.html" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
sleep 2
cu --tab "$late" eval 'document.title'
report "first command on it:" "exit $code in ${secs}s"
report "  error:" "$(errline | cut -c1-170)"
cu --tab "$late" eval 'document.title'
report "second command on it:" "exit $code in ${secs}s"
cu tabs close "$late"
report "cu tabs close (Target.closeTarget):" "exit $code in ${secs}s -> $(cat "$out")"

echo "== the keeper is light"
report "daemon threads / RSS:" "$(ls /proc/$daemon_pid/task | wc -l) / $(awk '/VmRSS/{print $2" kB"}' /proc/$daemon_pid/status)"
report "tabs now:" "$(tabcount)"

if [ "${SKIP_REAP:-0}" != 1 ]; then
  echo "== the tab of an exited session is collected"
  before=$(tabcount)
  # A session of its own that exits at once: python is the CLI's parent.
  python3 -c 'import subprocess,sys; subprocess.run([sys.argv[1],"navigate",sys.argv[2]],capture_output=True)' "$CU_BIN" "$url"
  report "tabs before / with the exited session's:" "$before / $(tabcount)"
  t0=$(date +%s)
  while [ "$(tabcount)" -gt "$before" ] && [ $(( $(date +%s) - t0 )) -lt 120 ]; do sleep 2; done
  report "collected after:" "$(( $(date +%s) - t0 ))s (tabs now $(tabcount))"
fi

echo
echo "logs: $ROOT/daemon.log $ROOT/cli.log"
grep 'cu: dialog\|collected\|did not answer' "$ROOT/daemon.log" | sed 's/^/  daemon: /' | head -30
