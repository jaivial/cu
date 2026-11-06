#!/usr/bin/env bash
# Publish an Instagram Story from the command line, driven by slimium or by cu (on a slimium browser).
#
#   post-story.sh --text "hola" [--bg black] [--fg white]   # render a 1080x1920 text story
#   post-story.sh --image story.png                          # publish an existing image
#   --driver slimium|cu   who drives the browser (default slimium; cu = the cu CLI, e.g. CU=cu-slim)
#   --lang auto|en|es|pt|fr|de   Instagram UI language (default auto: the account's own language,
#                         English when it is not one of the five; a forced language is restored after)
#   --user PROFILE (default instagram)  --session NAME (slimium driver, default instagram-story)
#   --port N  --username NAME  --dry-run (stop at the editor, discard)  --close
#
# Requires slimium >= 0.3.0 (slimium driver) or cu with mobile emulation (cu driver), python3, and
# ffmpeg for --text. The profile must already be logged in (SKILL.md, "One-time login").
# Exit codes: 0 ok, 2 usage, 3 not logged in, 4 editor/upload failed, 5 not published.
set -u
DRIVER=slimium LANG_OPT=auto USER_PROFILE=instagram SESSION=instagram-story PORT= IMAGE= TEXT=
BG=black FG=white DRY=0 CLOSE=0 USERNAME=
SLIM=${SLIMIUM:-slimium} CU=${CU:-cu}
die() { echo "post-story: $2" >&2; exit "$1"; }
while [ $# -gt 0 ]; do
  case $1 in
    --text) TEXT=${2-}; shift 2;;      --image) IMAGE=${2-}; shift 2;;
    --bg) BG=${2-}; shift 2;;          --fg) FG=${2-}; shift 2;;
    --driver) DRIVER=${2-}; shift 2;;  --lang) LANG_OPT=${2-}; shift 2;;
    --user) USER_PROFILE=${2-}; shift 2;; --session) SESSION=${2-}; shift 2;;
    --port) PORT=${2-}; shift 2;;      --username) USERNAME=${2-}; shift 2;;
    --dry-run) DRY=1; shift;;          --close) CLOSE=1; shift;;
    -h|--help) sed -n '2,15p' "$0"; exit 0;;
    *) die 2 "unknown option $1";;
  esac
done
[ -n "$TEXT" ] || [ -n "$IMAGE" ] || die 2 "pass --text TEXT or --image FILE"
[ -n "$TEXT" ] && [ -n "$IMAGE" ] && die 2 "--text and --image are exclusive"
command -v python3 >/dev/null || die 2 "python3 is required"

# Instagram's own labels, read from its web UI (phone layout) in each language.
declare -A OWN=([en]="Your story" [es]="Tu historia" [pt]="A tua história" [fr]="Votre Story" [de]="Deine Story")
declare -A SHARE=([en]="Share story" [es]="Compartir historia" [pt]="Partilhar história" [fr]="Partager la story" [de]="Story teilen")
declare -A CLOSE_L=([en]="Close" [es]="Cerrar" [pt]="Fechar" [fr]="Fermer" [de]="Schließen")
declare -A DISCARD=([en]="Discard" [es]="Descartar" [pt]="Descartar" [fr]="Abandonner" [de]="Verwerfen")
declare -A LOGIN=([en]="Log in" [es]="Iniciar sesión" [pt]="Iniciar sessão" [fr]="Se connecter" [de]="Anmelden")
case $LANG_OPT in auto|en|es|pt|fr|de) ;; *) die 2 "--lang must be auto, en, es, pt, fr or de";; esac

WORK=${XDG_CACHE_HOME:-$HOME/.cache}/post-story; mkdir -p "$WORK"   # under $HOME: readable by any browser build

# ---- drivers: mobile, desktop, nav URL, js EXPR (prints the value), click LABEL, upload LABEL FILE, shot FILE
case $DRIVER in
slimium)
  command -v "$SLIM" >/dev/null || die 2 "slimium not found (set SLIMIUM=/path/to/slimium)"
  case $("$SLIM" --version | awk '{print $2}') in 0.0.*|0.1.*|0.2.*) die 2 "slimium 0.3.0+ is required (persistent mobile emulation)";; esac
  B=("$SLIM" --session "$SESSION")
  d_open() {
    "${B[@]}" status 2>/dev/null | grep -q "^user  *$USER_PROFILE\$" && return 0
    "${B[@]}" start --user "$USER_PROFILE" ${PORT:+--port "$PORT"} >/dev/null
  }
  d_mobile() { "${B[@]}" viewport mobile >/dev/null; }
  d_desktop() { "${B[@]}" viewport reset >/dev/null; }
  d_nav() { "${B[@]}" nav "$1" --wait load >/dev/null; }
  d_js() { "${B[@]}" --timeout 30000 eval "$1"; }
  d_click() { "${B[@]}" click "role=button[name=\"$1\"]" >/dev/null 2>&1 || "${B[@]}" click "text=\"$1\"" >/dev/null; }
  d_upload() { "${B[@]}" click "text=\"$1\"" >/dev/null && "${B[@]}" upload 'css=input[type=file]' "$2" >/dev/null; }
  d_shot() { "${B[@]}" shot "$1" >/dev/null; }
  d_stop() { "${B[@]}" stop >/dev/null; }
  ;;
cu)
  command -v "$CU" >/dev/null || die 2 "cu not found (set CU=/path/to/cu or CU=cu-slim)"
  export SLIM_USER_PROFILE=$USER_PROFILE   # a cu-slim daemon started from here opens this profile
  cu_value() { python3 -c 'import json,sys
d=json.load(sys.stdin); r=(d.get("results") or [d])[-1]
if not r.get("ok",True) or "error" in r: sys.stderr.write(str(r.get("error",r))+"\n"); sys.exit(1)
v=r.get("value"); print(v if isinstance(v,str) else json.dumps(v))'; }
  # cu gives each calling process (its "session") its own page, and every $(...) here is another
  # process: open one tab from this script's own process and name it on every call (--tab).
  d_open() { local i   # a daemon that just started answers before its browser is up: wait on status
    for i in $(seq 1 60); do "$CU" status 2>/dev/null | grep -q '"running":true' && break; sleep 0.5; done
    "$CU" navigate about:blank >"$WORK/cu-nav.json" 2>/dev/null   # once: retries would each open a tab
    TAB=$(python3 -c 'import re,sys;m=re.search(r"\"frameId\":\"([0-9A-Fa-f]+)\"",open(sys.argv[1]).read());print(m.group(1) if m else "")' "$WORK/cu-nav.json")
    [ -n "$TAB" ]; }
  T() { "$CU" --tab "$TAB" "$@"; }
  d_mobile() { T act '[{"do":"emulate","mode":"mobile"}]' >/dev/null 2>&1; }
  d_desktop() { T act '[{"do":"emulate","mode":"desktop"}]' >/dev/null 2>&1; }
  d_nav() { T navigate "$1" >/dev/null 2>&1 || return 1; wait_js 'document.readyState==="complete"?"1":""' 20 >/dev/null; }
  d_js() { T eval "$1" 2>>"${PS_DEBUG:-/dev/null}" | tee -a "${PS_DEBUG:-/dev/null}" | cu_value 2>>"${PS_DEBUG:-/dev/null}"; }
  d_ref() { T snapshot 2>/dev/null | python3 -c 'import json,re,sys
raw=sys.stdin.read()
try: raw=json.loads(raw).get("snapshot",raw)
except Exception: pass
want=sys.argv[1]
for line in raw.splitlines():
    m=re.search(r"ref=(e\d+) \S+ \"([^\"]*)\"",line)
    if m and m.group(2).strip()==want: print(m.group(1)); break' "$1"; }
  # By accessible name first; some controls (the editor's share button) have none in cu's snapshot,
  # so tag the one whose visible text matches and ask cu for its ref.
  d_click() { local r; r=$(d_ref "$1")
    if [ -z "$r" ]; then
      [ "$(d_js "(()=>{document.querySelectorAll('[data-ps-target]').forEach(e=>e.removeAttribute('data-ps-target'));const b=[...document.querySelectorAll('button,[role=button],a')].find(e=>(e.innerText||'').trim()==='$1');if(!b)return '';b.setAttribute('data-ps-target','1');return '1'})()")" = 1 ] || return 1
      r=$(T inspect --selector '[data-ps-target="1"]' 2>/dev/null | python3 -c 'import json,sys
try: print(json.load(sys.stdin).get("ref",""))
except Exception: pass')
    fi
    [ -n "$r" ] && T click "$r" >/dev/null 2>&1; }
  # A tap on our avatar opens the viewer when a story is already up, so the image goes straight into
  # the page's story file input (the first one; cu >= upload --selector).
  d_upload() { T snapshot >/dev/null 2>&1; T upload --selector 'input[type=file]' "$2" >/dev/null 2>&1; }
  d_shot() { T shot "$1" --png >/dev/null 2>&1; }
  d_stop() { :; }   # the cu daemon is shared: never stopped from here
  ;;
*) die 2 "--driver must be slimium or cu";;
esac

# wait_js EXPR SECONDS: poll a quick expression (each eval well under any driver timeout) until it
# returns a non-empty string; prints it, or nothing on timeout.
wait_js() { local end=$(( $(date +%s) + $2 )) v
  while :; do v=$(d_js "$1" 2>/dev/null); [ -n "$v" ] && { printf '%s\n' "$v"; return 0; }
    [ "$(date +%s)" -ge "$end" ] && return 1; sleep 0.4; done; }

if [ -n "$TEXT" ]; then
  command -v ffmpeg >/dev/null || die 2 "ffmpeg is required for --text"
  # textfile= + expansion=none: drawn literally (no escaping of : ' \, no %{...} expansion)
  printf '%s' "$TEXT" >"$WORK/text.txt"; IMAGE=$WORK/story-$$.png
  ffmpeg -hide_banner -loglevel error -y -f lavfi -i "color=c=$BG:s=1080x1920:d=1" \
    -vf "drawtext=font='DejaVu Sans':textfile=$WORK/text.txt:expansion=none:fontcolor=$FG:fontsize=72:x=(w-text_w)/2:y=(h-text_h)/2" \
    -frames:v 1 "$IMAGE" || die 2 "could not render the text story"
fi
[ -f "$IMAGE" ] || die 2 "image not found: $IMAGE"
IMAGE=$(realpath "$IMAGE")

# 1. Browser on the logged-in profile, phone emulation BEFORE Instagram loads.
d_open || die 4 "could not start the browser on profile $USER_PROFILE (in use by another browser?)"
d_mobile || die 4 "mobile emulation failed"
d_nav https://www.instagram.com/ || die 4 "could not load instagram.com"
ORIG=$(wait_js 'document.readyState==="complete"?document.documentElement.lang.slice(0,2):""' 15)
L=$LANG_OPT; [ "$L" = auto ] && { L=$ORIG; [ -n "${OWN[$L]+x}" ] || L=en; }
if [ "$L" != "$ORIG" ]; then d_nav "https://www.instagram.com/?hl=$L" || die 4 "could not switch Instagram to $L"; fi
restore_lang() { [ "$L" != "$ORIG" ] && [ -n "$ORIG" ] && d_nav "https://www.instagram.com/?hl=$ORIG"; }

# 2. Logged in? Whichever shows first: our story tray, or a login prompt.
AUTH=$(wait_js "(()=>{const t=document.body?document.body.innerText:'';if(t.includes('${OWN[$L]}'))return 'in';if(document.querySelector('input[type=password]')||location.pathname.startsWith('/accounts/login')||[...document.querySelectorAll('a,button')].some(e=>(e.innerText||'').trim()==='${LOGIN[$L]}'))return 'out';return ''})()" 25)
case $AUTH in
  in) ;;
  out) restore_lang; die 3 "profile '$USER_PROFILE' is not logged in to Instagram (see SKILL.md, One-time login)";;
  *) die 4 "no '${OWN[$L]}' in the story tray after 20 s (is the phone emulation active?)";;
esac

# Language-independent publish check: our reel in Instagram's own story tray (count:newest taken_at).
TRAY_JS='(()=>{const ck=n=>(document.cookie.match(new RegExp("(?:^|; )"+n+"=([^;]+)"))||[])[1];const id=ck("ds_user_id");if(!id)return "noid";const x=new XMLHttpRequest();x.open("GET","/api/v1/feed/reels_tray/",false);x.setRequestHeader("X-IG-App-ID","936619743392459");x.setRequestHeader("X-CSRFToken",ck("csrftoken")||"");x.setRequestHeader("X-Requested-With","XMLHttpRequest");x.send();if(x.status!==200)return "http"+x.status;const t=(JSON.parse(x.responseText).tray||[]).find(t=>String((t.user||{}).pk)===id||String(t.id)===id);const it=(t&&t.items)||[];return it.length+":"+(it.length?Math.max(...it.map(i=>i.taken_at)):0)})()'
BEFORE=$(d_js "$TRAY_JS" 2>/dev/null)

# 3. Creator + file. The editor must be in portrait (it says "rotate your device" otherwise).
d_upload "${OWN[$L]}" "$IMAGE" || die 4 "could not open the story creator / upload the file"
ED=$(wait_js "(()=>{const b=[...document.querySelectorAll('button,[role=button]')].some(e=>(e.innerText||'').trim()==='${SHARE[$L]}');return location.pathname.startsWith('/create/story')&&b?screen.orientation.type:''})()" 35 || d_js 'location.pathname')
case $ED in
  portrait*) ;;
  landscape*) die 4 "the editor is in landscape: phone emulation lost (Instagram asks to rotate the device)";;
  *) die 4 "the story editor did not open ($ED)";;
esac
PREVIEW=$WORK/preview-$$.png; d_shot "$PREVIEW" || PREVIEW=

if [ "$DRY" = 1 ]; then
  d_click "${CLOSE_L[$L]}" || true
  sleep 0.8
  d_click "${DISCARD[$L]}" >/dev/null 2>&1 || true
  RESULT="{\"ok\":true,\"dry_run\":true,\"driver\":\"$DRIVER\",\"lang\":\"$L\",\"preview\":\"$PREVIEW\"}"
else
  # 4. Publish; confirmed when our reel gains a newer item.
  d_click "${SHARE[$L]}" || die 4 "could not tap '${SHARE[$L]}'"
  OK=0
  for _ in $(seq 1 45); do
    sleep 2
    NOW=$(d_js "$TRAY_JS" 2>/dev/null)
    if [ "${NOW#*:}" != "${BEFORE#*:}" ] && [ "${NOW#*:}" -gt "${BEFORE#*:}" ] 2>/dev/null; then OK=1; break; fi
  done
  [ "$OK" = 1 ] || die 5 "Instagram's story tray shows no new story after 90 s (before $BEFORE, now ${NOW:-?})"
  RESULT="{\"ok\":true,\"published\":true,\"driver\":\"$DRIVER\",\"lang\":\"$L\",\"reel\":\"$BEFORE->$NOW\",\"preview\":\"$PREVIEW\"}"
fi
d_nav https://www.instagram.com/ >/dev/null 2>&1; restore_lang >/dev/null 2>&1
d_desktop >/dev/null 2>&1 || true
[ "$CLOSE" = 1 ] && d_stop
rm -f "$WORK/text.txt" "$WORK/story-$$.png"
echo "$RESULT"