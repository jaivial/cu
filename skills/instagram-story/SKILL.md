---
name: instagram-story
description: Publish an Instagram Story (an image, or text on a solid background) from the command line with slimium, the low-RAM Chromium that cu-slim uses as its browser. Use when the user asks to post, upload or test an Instagram story from an agent. Covers the one-time login into a persistent profile, a dry run, publishing, and verification.
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
- A slimium user profile that is already logged in to Instagram (below). The account's UI language must
  be English: the script looks for "Your story" and "Share story".

## One-time login (persistent profile)

Website logins live in a named slimium user profile and survive browser, daemon and machine restarts.

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
```

| option | meaning |
| --- | --- |
| `--text TEXT [--bg black] [--fg white]` | render a 1080x1920 PNG with the text centred. It is drawn literally: `%`, quotes, `:` and `\\` are safe |
| `--image FILE` | publish an existing image; portrait 9:16 looks best |
| `--user NAME` | logged-in slimium user profile (default `instagram`) |
| `--session NAME` | slimium session (default `instagram-story`); an already-running session on the same profile is reused |
| `--dry-run` | stop at the editor, save a preview screenshot, discard the draft |
| `--username NAME` | account name for the viewer check (detected when omitted) |
| `--close` | stop the browser afterwards |

Output is one JSON line: `{"ok":true,"published":true,"viewer":"portrait-media-visible","preview":"/tmp/..png"}`.

Exit codes:

| code | meaning |
| --- | --- |
| 0 | ok |
| 2 | usage, or slimium/ffmpeg missing or too old |
| 3 | the profile is not logged in |
| 4 | the editor or upload failed |
| 5 | Instagram did not confirm the story |

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
$S wait 'text=Your story was added to Instagram'  # Instagram's own confirmation
$S viewport reset
```

## Pitfalls

- **Desktop layout:** if you look for Story in the desktop menu, it is not there. Set `viewport mobile`
  first, then load the page.
- **"Rotate your device":** the emulation was lost. You are on slimium < 0.3.0, the keeper is disabled
  (`SLIM_HOLD=0`), or something set raw `Emulation.*` overrides from another DevTools client.
- **Profile lock:** one browser per profile. Stop cu-slim first if it runs with
  `SLIM_USER_PROFILE=instagram`, or let the script reuse a running slimium session on that profile with
  `--session`.
- **Stickers, links, mentions and music** are not automated. Add them in the editor with
  `snapshot --interactive` plus `click`/`fill` before "Share story".
- **Instagram's own rules apply:** rate limits, checkpoints ("was this you?") and AI-content labels
  ("Add AI label" in the editor). The script never answers a checkpoint by itself.
