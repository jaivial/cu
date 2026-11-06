#!/usr/bin/env bash
# Publish an Instagram Story from the command line with slimium's mobile emulation.
#
#   post-story.sh --text "hello" [--bg black] [--fg white]      # render a 1080x1920 text story
#   post-story.sh --image story.png                               # publish an existing image
#   options: --user PROFILE (default: instagram)  --session NAME (default: instagram-story)
#            --port N  --dry-run (stop at the editor, discard)  --close (stop the browser afterwards)
#            --username NAME (for the viewer check; detected when omitted)
#
# Requires slimium >= 0.3.0 (persistent emulation keeper) and ffmpeg for --text.
# The profile must already be logged in to Instagram (see SKILL.md, "One-time login").
# Exit codes: 0 published (or dry run ok), 2 usage, 3 not logged in, 4 editor/upload failed,
#             5 Instagram did not confirm the story.
set -u
SLIM=${SLIMIUM:-slimium}
USER_PROFILE=instagram SESSION=instagram-story PORT= IMAGE= TEXT= BG=black FG=white
DRY=0 CLOSE=0 USERNAME=
die() { echo "post-story: $2" >&2; exit "$1"; }
while [ $# -gt 0 ]; do
  case $1 in
    --text) TEXT=${2-}; shift 2;;
    --image) IMAGE=${2-}; shift 2;;
    --bg) BG=${2-}; shift 2;;
    --fg) FG=${2-}; shift 2;;
    --user) USER_PROFILE=${2-}; shift 2;;
    --session) SESSION=${2-}; shift 2;;
    --port) PORT=${2-}; shift 2;;
    --username) USERNAME=${2-}; shift 2;;
    --dry-run) DRY=1; shift;;
    --close) CLOSE=1; shift;;
    -h|--help) sed -n '2,15p' "$0"; exit 0;;
    *) die 2 "unknown option $1";;
  esac
done
[ -n "$TEXT" ] || [ -n "$IMAGE" ] || die 2 "pass --text TEXT or --image FILE"
[ -n "$TEXT" ] && [ -n "$IMAGE" ] && die 2 "--text and --image are exclusive"
command -v "$SLIM" >/dev/null || die 2 "slimium not found (set SLIMIUM=/path/to/slimium)"
ver=$("$SLIM" --version | awk '{print $2}')
case $ver in 0.0.*|0.1.*|0.2.*) die 2 "slimium $ver is too old: 0.3.0+ keeps mobile emulation across Instagram's reloads";; esac
B=("$SLIM" --session "$SESSION")
TMP=$(mktemp -d /tmp/post-story.XXXXXX); trap 'rm -rf "$TMP"' EXIT

if [ -n "$TEXT" ]; then
  command -v ffmpeg >/dev/null || die 2 "ffmpeg is required for --text"
  # textfile= + expansion=none: drawn literally (no escaping of : ' \, no %{...} expansion)
  printf '%s' "$TEXT" >"$TMP/text.txt"
  IMAGE=$TMP/story.png
  ffmpeg -hide_banner -loglevel error -y -f lavfi -i "color=c=$BG:s=1080x1920:d=1" \
    -vf "drawtext=font='DejaVu Sans':textfile=$TMP/text.txt:expansion=none:fontcolor=$FG:fontsize=72:x=(w-text_w)/2:y=(h-text_h)/2" \
    -frames:v 1 "$IMAGE" || die 2 "could not render the text story"
fi
[ -f "$IMAGE" ] || die 2 "image not found: $IMAGE"
IMAGE=$(realpath "$IMAGE")

# 1. A browser on the logged-in profile (reused when the session already runs on it).
if ! "${B[@]}" status 2>/dev/null | grep -q "^user  *$USER_PROFILE\$"; then
  "${B[@]}" start --user "$USER_PROFILE" ${PORT:+--port "$PORT"} >/dev/null || die 4 "could not start slimium on profile $USER_PROFILE (in use by another browser?)"
fi
# 2. Instagram only offers "Your story" to a phone: mobile preset (390x844, dpr 3, touch,
#    portrait, Android UA) BEFORE loading the page. slimium's keeper keeps it across the
#    editor's own reloads; without it the editor says "Rotate your device".
"${B[@]}" viewport mobile >/dev/null || die 4 "viewport mobile failed"
"${B[@]}" nav https://www.instagram.com/ --wait load >/dev/null || die 4 "could not load instagram.com"
# Whichever shows first: the story tray (logged in) or a login prompt (logged out).
AUTH=$("${B[@]}" --timeout 25000 eval 'new Promise(r=>{const t0=Date.now();(function f(){const t=document.body?document.body.innerText:"";if(t.includes("Your story"))return r("in");if(document.querySelector("input[type=password]")||location.pathname.startsWith("/accounts/login")||[...document.querySelectorAll("a,button")].some(e=>/^log ?in$/i.test((e.innerText||"").trim())))return r("out");if(Date.now()-t0>20000)return r("unknown");setTimeout(f,250)})()})' 2>/dev/null)
case $AUTH in
  in) ;;
  out) die 3 "profile '$USER_PROFILE' is not logged in to Instagram (see SKILL.md, One-time login)";;
  *) die 4 "no 'Your story' button after 20 s (UI language not English, or mobile preset not active?)";;
esac
# 3. Open the creator and hand it the file.
"${B[@]}" click 'text="Your story"' >/dev/null || die 4 "could not tap Your story"
"${B[@]}" upload 'css=input[type=file]' "$IMAGE" >/dev/null || die 4 "upload failed"
"${B[@]}" --timeout 30000 wait 'role=button[name="Share story"]' >/dev/null || die 4 "the story editor did not open"
if "${B[@]}" text | grep -q 'Rotate your device'; then die 4 "editor asks to rotate the device (mobile emulation lost)"; fi
PREVIEW=${TMPDIR:-/tmp}/post-story-preview-$$.png
"${B[@]}" shot "$PREVIEW" >/dev/null || true

if [ "$DRY" = 1 ]; then
  "${B[@]}" click 'role=button[name="Close"]' >/dev/null 2>&1 || true
  "${B[@]}" --timeout 3000 click 'role=button[name="Discard"]' >/dev/null 2>&1 || true
  echo "{\"ok\":true,\"dry_run\":true,\"preview\":\"$PREVIEW\"}"
else
  # 4. Publish and wait for Instagram's own confirmation.
  "${B[@]}" click 'role=button[name="Share story"]' >/dev/null || die 4 "could not tap Share story"
  "${B[@]}" --timeout 60000 wait 'text=Your story was added to Instagram' >/dev/null || die 5 "Instagram did not confirm the story within 60 s"
  # 5. Independent check in the story viewer: a portrait image less than a few minutes old.
  if [ -z "$USERNAME" ]; then
    USERNAME=$("${B[@]}" eval '(()=>{const a=[...document.querySelectorAll("a[href]")].find(a=>/profile/i.test(a.textContent)||a.querySelector("img[alt*=profile]"));return a?a.getAttribute("href").replace(/\//g,""):""})()' 2>/dev/null)
  fi
  VIEW=unchecked
  if [ -n "$USERNAME" ] && "${B[@]}" nav "https://www.instagram.com/stories/$USERNAME/" --wait load >/dev/null 2>&1; then
    "${B[@]}" eval 'new Promise(r=>setTimeout(r,3000))' >/dev/null
    VIEW=$("${B[@]}" eval '(()=>{const m=[...document.querySelectorAll("img,video")].find(e=>e.getBoundingClientRect().width>200);return m&&(m.naturalHeight||m.videoHeight)>(m.naturalWidth||m.videoWidth)?"portrait-media-visible":"not-found"})()')
  fi
  echo "{\"ok\":true,\"published\":true,\"viewer\":\"$VIEW\",\"preview\":\"$PREVIEW\"}"
fi
"${B[@]}" viewport reset >/dev/null 2>&1 || true
[ "$CLOSE" = 1 ] && "${B[@]}" stop >/dev/null
exit 0
