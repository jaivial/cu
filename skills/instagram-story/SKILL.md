---
name: instagram-story
description: Publish an Instagram Story (an image, or text on a solid background) from the command line, driven by slimium or by cu on a slimium browser, with Instagram in English, Spanish, Portuguese, French or German. Use when the user asks to post, upload or test an Instagram story from an agent (publicar una historia / story de Instagram). Covers the one-time login into a persistent profile, a dry run, publishing, and a language-independent verification.
---

# Instagram Story from the CLI

Instagram's web app only offers stories to a phone. The desktop "Create" menu has Post, Live video and
Ad, but no Story. In a phone layout the story tray starts with **Your story (+)**. To get that layout the
browser must look like a phone *when the page loads*: 390x844 CSS px, DPR 3, portrait orientation, touch
and a mobile user agent. It must keep looking like that while the story editor reloads itself; otherwise the
editor answers "Rotate your device to add to your story".

slimium 0.3.0+ does this with a per-session keeper process. `slimium viewport mobile` sets the whole phone
emulation once, and the keeper re-applies it to every reload, navigation and new tab. It also accepts the
editor's "leave page?" (beforeunload) dialog instead of freezing the tab.

## Requirements

- `slimium` >= 0.3.0 on PATH (or `SLIMIUM=/path/to/slimium`), and `ffmpeg` for text stories.
- A slimium user profile that is already logged in to Instagram (below).
- `python3`.
- For `--driver cu`: cu >= 1.10.0, which has three things: whole-phone `emulate mobile` (portrait, DPR 3, Android UA, kept
  across reloads), `upload --selector`, and JavaScript dialog handling (the editor's "leave page?").
  Use cu-slim (cu on slimium). The script exports `SLIM_USER_PROFILE`, so a cu-slim daemon it starts opens the
  profile. Every cu call names the script's own tab (`--tab`): cu gives each calling process its own page.

## Languages

The script knows Instagram's own labels in five UI languages, read from its phone layout:

| | own tray item | share | close / discard | logged out |
| --- | --- | --- | --- | --- |
| en | Your story | Share story | Close / Discard | Log in |
| es | Tu historia | Compartir historia | Cerrar / Descartar | Iniciar sesión |
| pt (PT) | A tua história | Partilhar história | Fechar / Descartar | Iniciar sessão |
| fr | Votre Story | Partager la story | Fermer / Abandonner | Se connecter |
| de | Deine Story | Story teilen | Schließen / Verwerfen | Anmelden |

- **Choosing the language:** `--lang auto` (the default) uses the account's language when it is one of these and
  English otherwise. `--lang es` (etc.) forces one through Instagram's `?hl=` parameter and restores the original
  language at the end.
- **Checks that do not depend on the language:** the editor's orientation, and the publish confirmation, which
  watches the account's own reel grow in Instagram's story tray (`/api/v1/feed/reels_tray/`, read inside the
  logged-in page).

## One-time login (persistent profile)

Website logins live in a named slimium user profile and survive browser, daemon and machine restarts.
The profile pins its cookie password store (`basic` by default, slimium >= 0.3.1). Older slimium let
`slimium start` (GNOME keyring) and cu (`--password-store=basic`) open the same profile with different
stores. Chromium then **deletes** the cookies it cannot decrypt, and the login is lost; log in again after
upgrading.

```sh
slimium profile create instagram
slimium --session ig start --user instagram
slimium --session ig nav https://www.instagram.com/accounts/login/
slimium --session ig snapshot --interactive     # find the username/password refs, e.g. @e1 @e2 and "Log In" @e3
# Credentials go on stdin, never on argv: argv is visible in ps and in shell history.
slimium --session ig act - <<'JSON'
[{"action":"fill","locator":"@e1","text":"<username>"},
 {"action":"fill","locator":"@e2","text":"<password>"},
 {"action":"click","locator":"@e3"}]
JSON
```

- If Instagram asks for a code, it has sent it to the account's email or phone. Ask the user for it, then fill
  and submit it the same way.
- Answer "Save your login info?" and "Turn on notifications" with **Not now**.
- Never repeat a password or a code back in chat or logs.
- Verify after a restart: `slimium --session ig stop && slimium --session ig start --user instagram`. Then the
  feed loads with no password field.

## Publish

Always do a dry run first and get the user's go-ahead: a story is public.

```sh
skills/instagram-story/post-story.sh --text "esto es una prueba de IA" --dry-run   # editor + preview, then discard
skills/instagram-story/post-story.sh --text "esto es una prueba de IA"             # publish (black bg, white text)
skills/instagram-story/post-story.sh --image story.png                              # publish your own 1080x1920 image
CU=cu-slim skills/instagram-story/post-story.sh --driver cu --lang es --text "hola"  # cu drives a slimium browser
```

| option | meaning |
| --- | --- |
| `--text TEXT [--bg black] [--fg white]` | render a 1080x1920 PNG with the text centred. It is drawn literally: `%`, quotes, `:` and `\\` are safe |
| `--image FILE` | publish an existing image; portrait 9:16 looks best |
| `--driver slimium\|cu` | slimium CLI (default), or the cu CLI (`CU=cu-slim` for cu on slimium) |
| `--lang auto\|en\|es\|pt\|fr\|de` | Instagram UI language (see Languages) |
| `--user NAME` | logged-in slimium user profile (default `instagram`) |
| `--session NAME` | slimium driver: session (default `instagram-story`); a running session on the same profile is reused |
| `--dry-run` | stop at the editor, save a preview screenshot, discard the draft |
| `--username NAME` | account name for the viewer check (detected when omitted) |
| `--close` | slimium driver: stop the browser afterwards (a cu daemon is shared and never stopped) |
| `PS_DEBUG=FILE` (env) | cu driver: append cu's raw replies to FILE |

Output is one JSON line, e.g. `{"ok":true,"published":true,"driver":"cu","lang":"es","reel":"1:1762..->2:1762..","preview":"~/.cache/post-story/preview-123.png"}`.
Images and previews live under `~/.cache/post-story`, not `/tmp`: a snap Chromium cannot read the
host's `/tmp`.

Exit codes:

| code | meaning |
| --- | --- |
| 0 | ok |
| 2 | usage, or slimium/ffmpeg missing or too old |
| 3 | the profile is not logged in |
| 4 | the editor or upload failed |
| 5 | the story tray shows no new story after 90 s |

The preview is the editor at 1170x2532 px. Check it before publishing. On a model without image input,
measure it: the background colour should fill the frame, with text pixels only where the text is.

## What the script does (manual equivalent)

```sh
S="slimium --session ig"
$S viewport mobile                                # 390x844 dpr 3, portrait, touch, Android Chrome UA
$S nav https://www.instagram.com/ --wait load     # load AFTER the preset: the tray now starts with "Your story"
$S click 'text="Your story"'                      # a real touch tap
$S upload 'css=input[type=file]' story.png        # opens /create/story/ with the image
$S wait 'role=button[name="Share story"]'
$S shot preview.png                               # 1170x2532 preview
$S click 'role=button[name="Share story"]'
$S wait 'text=Your story was added to Instagram'  # Instagram's toast (English); the script checks the tray instead
$S viewport reset
```

## Pitfalls

- **Desktop layout:** if you look for Story in the desktop menu, it is not there. Set `viewport mobile`
  first, then load the page.
- **"Rotate your device":** the emulation was lost. You are on slimium < 0.3.0, the keeper is disabled
  (`SLIM_HOLD=0`), or something set raw `Emulation.*` overrides from another DevTools client.
- **Mixed password stores:** see One-time login. Never open a profile with an older slimium and cu in turn.
- **Profile lock:** one browser per profile. Stop cu-slim first if it runs with
  `SLIM_USER_PROFILE=instagram`, or let the script reuse a running slimium session on that profile with
  `--session`.
- **Stickers, links, mentions and music** are not automated. Add them in the editor with
  `snapshot --interactive` plus `click`/`fill` before "Share story".
- **Instagram's own rules apply:** rate limits, checkpoints ("was this you?") and AI-content labels
  ("Add AI label" in the editor). The script never answers a checkpoint by itself.