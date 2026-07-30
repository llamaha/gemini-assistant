# gemini-assistant

Hotkey-driven, real-time spoken conversation with Gemini Live. No daemon:
the process exists exactly as long as the session does, so two hotkey
presses can never race into two live mic sessions.

## Build

```
cargo build --release
```

## API key

A KDE global shortcut runs the binary directly with no shell profile
sourced, so `GEMINI_API_KEY` in your shell environment won't be visible to
it. Put the key in a file the binary reads itself:

```
mkdir -p ~/.config/gemini-assistant
echo 'GEMINI_API_KEY=your-key-here' > ~/.config/gemini-assistant/env
chmod 600 ~/.config/gemini-assistant/env
```

(Falls back to the `GEMINI_API_KEY` env var if that file is absent. Handy
for terminal or manual runs.)

## Config (optional)

`~/.config/gemini-assistant/config.json`, all fields optional:

```json
{
  "model": "models/gemini-3.1-flash-live-preview",
  "voice": "Puck",
  "system_instruction": "You are a fast, concise voice assistant...",
  "google_search": true,
  "reminder_secs": 600,
  "context_window_target_tokens": 16000,
  "vad_threshold": 400.0,
  "screenshot_max_edge": 1024,
  "screenshot_quality": 80,
  "ask_max_secs": 60
}
```

Screenshots are downscaled to `screenshot_max_edge` on the longest side before
sending — a real 2560×1440 capture goes from a 594 KB PNG to a 123 KB JPEG,
which is still ample to read a dialog and much cheaper in tokens and upload
time. Raise it if fine print ever gets lost.

A session stays alive indefinitely. The Live API recycles the underlying
connection on a timer (roughly every 10–15 minutes) regardless of activity —
so pausing doesn't stave it off — but the assistant captures the server's
session-resumption handle and transparently reconnects with it when that
happens, carrying the conversation across the recycle. You may hear a brief gap
mid-session; it keeps going. Only `end` (Meta+F2) actually stops it. If the
network is genuinely down, it retries a few times with backoff and then ends.

`reminder_secs: 0` disables the "still open" reminder. Env vars
(`GEMINI_LIVE_MODEL`, `GEMINI_ASSISTANT_REMINDER_SECS`,
`GEMINI_ASSISTANT_VAD_THRESHOLD`) override individual fields for a one-off
run. `GEMINI_ASSISTANT_DEBUG=1` prints raw session events to stderr.

## Commands

| Command | Behaviour |
|---|---|
| `gemini-assistant` / `talk` | Start a session if idle; otherwise pause/resume the mic. **Never ends the session** — bind your main hotkey to this. |
| `gemini-assistant end` | End the session. The only command that does. Bind your second hotkey to this. |
| `gemini-assistant pause` | Explicit pause/resume toggle. Same effect as `talk` on a running session. |
| `gemini-assistant ask` | One-shot spoken question, separate from any session: press to start talking, press again to send, hear a single answer, done. No session, no kept context. |
| `gemini-assistant look` | Drag out a rectangle and send it to the running session, so she can see what you're looking at. Then just ask about it out loud. `--window` grabs the focused window with no interaction; `--full` grabs the whole desktop. |
| `gemini-assistant status` | Print `live` / `paused` / `stopped`. |
| `gemini-assistant last` | Print (and clipboard-copy) the model's most recent answer. Useful for grabbing a command or plan it just gave you. |
| `gemini-assistant send-clip <wav>` | Diagnostic: send a WAV straight to the API and play back the reply, bypassing the mic/pidfile entirely. Good for checking the key/model/network without a microphone. |

## KDE hotkeys

System Settings → Shortcuts → Custom Shortcuts → New → Global Shortcut →
Command/URL. Three shortcuts (the keys below are what this setup uses — pick
whatever you like):

| Key | Command | Does |
|---|---|---|
| **Meta+F1** | `…/gemini-assistant talk` | Start a session; on a running one, pause/resume the mic |
| **Meta+F2** | `…/gemini-assistant end` | End the session |
| **Meta+F3** | `…/gemini-assistant look` | Drag a rectangle and show it to her |
| **Meta+F4** | `…/gemini-assistant ask` | One-shot spoken question (press to talk, press to send) |

Ending is deliberately on its own key. The first key gets pressed constantly
and by reflex, so it must never be able to throw away a conversation — a
mistimed press just toggles the mic. `toggle` still works as an alias for
`talk`, so an existing binding keeps working with the new (safe) meaning.

**Showing her your screen (Meta+F3):** a session must already be running
(Meta+F1). Press F3, drag a rectangle around whatever you want her to see,
and it's captured and sent into the session as an image. The screenshot is
*context, not a question* — sending it doesn't prompt a reply on its own.
Send it, then just ask about it out loud ("what's this error mean?"). Use
`look --window` for the focused window with no dragging, or `look --full` for
the whole desktop.

**One-shot question (Meta+F4):** for a quick question when you don't want a
whole session. Press F4 to start talking, press again to send; you hear one
spoken answer and it disconnects — nothing kept. Unlike a session, it turns
off server-side voice detection and marks the turn boundaries explicitly
(`activityStart`/`activityEnd`), so the model answers the instant you press
send rather than waiting to guess you've paused. It's independent of a live
session (its own pidfile), and if you never press send a second time the
question is committed automatically after `ask_max_secs` (default 60) so the
mic can't stay open indefinitely.

Note this runs the **live** model (it speaks the answer), not the text-only
`gemini-3.6-flash`. The non-Live API returns text, not speech, so a spoken
one-shot necessarily uses the live model.

## Verifying it works

`send-clip` needs no microphone and exercises the whole pipeline (key,
model, TLS, wire protocol, playback):

```
espeak -w /tmp/clip.wav "what's the capital of France"
./target/release/gemini-assistant send-clip /tmp/clip.wav
```

Full mic/pause/barge-in behavior needs real hardware, headphones
specifically, since the mic stays live while the model talks, and on
speakers it can hear (and interrupt) itself.
