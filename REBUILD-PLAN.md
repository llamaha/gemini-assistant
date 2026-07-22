# gemini-assistant — clean rebuild plan

Plan for rebuilding this tool from scratch in a new repo, carrying over what
we proved works and dropping the scaffolding we outgrew.

Copy this file into the new repo as the starting point.

---

## Context

The goal: press a hotkey, have a spoken conversation with Gemini, press it
again to stop. Fast enough for "what's the hotkey to switch editor modes in
Blender?" mid-task, and able to hold context ("I'm playing X with a level 60
Y") so follow-ups don't need re-explaining.

The first version was modelled on `../bug-dictation` (hotkey → record →
transcribe → respond), which turned out to be the wrong shape twice over:

1. **The daemon was cargo-culted.** `bug-dictation` runs a resident daemon to
   keep Whisper warm in ~10GB of VRAM. We have no local model, so ours sat
   idle holding nothing — while introducing a bug class where multiple daemons
   each held a mic, a speaker, and a session, all answering the same question
   at once.
2. **Question-at-a-time was the wrong interaction.** Opening a session per
   question meant reconstructing conversation context from stored transcripts,
   which cannot be made reliable (see *Input transcription* below).

Both are fixed by the same move: **the session is the unit of work, and the
process is the session.**

---

## Architecture

**One binary. No daemon. No socket. The process exists exactly as long as the
session does.**

```
hotkey #1  →  binary starts  →  claims pidfile  →  opens mic + WebSocket
                                                    ↕ streams both ways
hotkey #2  →  binary starts  →  finds live pidfile →  SIGTERM the first  →  exits
pause key  →  binary starts  →  finds live pidfile →  SIGUSR1 the first  →  exits
```

### Commands

| Command | Behaviour |
|---|---|
| `gemini-assistant` (default) / `toggle` | If a live session owns the pidfile, `SIGTERM` it and exit. Otherwise become the session. |
| `gemini-assistant pause` | `SIGUSR1` the session process (toggles mic on/off, session and context preserved). |
| `gemini-assistant status` | Print `live` / `paused` / `stopped` from the pidfile. |
| `gemini-assistant send-clip <wav>` | Diagnostic: one-shot WAV → API → speakers, no session. Keep this — it isolates "is the key/model/network OK?" from session machinery, and is the only way to test without a microphone. |

### Signals

- `SIGTERM` / `SIGINT` → graceful shutdown: close WebSocket, drop mic, remove
  pidfile, play the end chime.
- `SIGUSR1` → toggle pause. **Pause must drop the mic stream entirely**, not
  just discard samples, so the OS microphone indicator genuinely goes off.

### Pidfile — get this right

`$XDG_RUNTIME_DIR/gemini-assistant.pid`.

Two hotkey presses in quick succession must not both become sessions. Two live
sessions means two mics and two voices — the exact failure we're eliminating.

- Create with `O_CREAT | O_EXCL` so claiming it is atomic. Losing the race
  means another process just claimed it: signal that one instead.
- A pidfile whose process is gone (SIGKILL, crash) is stale. Verify the pid is
  alive **and** is actually our binary before trusting it — pids get recycled.
  Check `/proc/<pid>/comm`, but note the caveat in *Gotchas*.
- Remove it on every exit path, including signal handlers.

---

## Modules

| File | Responsibility |
|---|---|
| `main.rs` | CLI dispatch, pidfile claim/signal logic, signal handlers |
| `session.rs` | Session lifecycle: mic → socket pump, socket → speaker receive loop, pause/resume |
| `live_api.rs` | Live API wire protocol: connect, setup, `realtimeInput`, decoding `serverContent` into events |
| `audio.rs` | `cpal` capture/playback, resampling, chimes |
| `config.rs` | Config file + env var loading, including reading the API key |
| `transcript.rs` | Append-only local record of what was said (display only — see below) |

Keep it flat; the previous version's ~1700 lines had no need for deeper
structure.

---

## Gemini Live API — verified specifics

Endpoint (note: **`wss://`, HTTP/1.1** — an HTTP/2 upgrade attempt returns 404):

```
wss://generativelanguage.googleapis.com/ws/google.ai.generativelanguage.v1beta.GenerativeService.BidiGenerateContent?key=$GEMINI_API_KEY
```

Setup message — must be the first frame, and wait for `{"setupComplete":{}}`
before sending audio:

```json
{"setup": {
  "model": "models/gemini-2.5-flash-native-audio-preview-12-2025",
  "generationConfig": {"responseModalities": ["AUDIO"]},
  "systemInstruction": {"parts": [{"text": "..."}]},
  "tools": [{"google_search": {}}],
  "inputAudioTranscription": {},
  "outputAudioTranscription": {},
  "contextWindowCompression": {"slidingWindow": {}}
}}
```

Audio in — send continuously; **do not send `audioStreamEnd`** in a live
session. The server's own voice-activity detection decides where your turns
end:

```json
{"realtimeInput": {"audio": {"data": "<base64 PCM16LE>", "mimeType": "audio/pcm;rate=16000"}}}
```

Audio formats are fixed: **input mono PCM16LE @ 16kHz, output mono PCM16LE @
24kHz.** Resample at the boundary — linear interpolation is fine for speech.

Server messages, all under `serverContent`:

- `modelTurn.parts[].inlineData.data` — base64 response audio
- `outputTranscription.text` — what the model said (**reliable**)
- `inputTranscription.text` — what you said (**not reliable**, see below)
- `interrupted: true` — you talked over it; **flush the playback queue**
- `generationComplete: true` — model finished producing
- `turnComplete: true` — official end of turn

### `generationComplete` vs `turnComplete`

For long answers `turnComplete` can lag far behind `generationComplete` or not
arrive within any reasonable window — this caused hard timeouts on detailed
questions. All response audio has arrived by `generationComplete`.

In a one-shot request: treat `generationComplete` as "audio is done", then keep
draining briefly for trailing messages rather than returning immediately.
In a live session this is mostly moot — just keep reading until the socket
closes.

### Input transcription is unreliable — do not build on it

Measured across several clips: sometimes complete, sometimes truncated
mid-sentence (`"what level might"` for *"what level am I"*), and sometimes
**entirely absent** — zero messages, reproducibly, for the same clip, even
after waiting 20s and receiving `turnComplete`.

This killed the original design, which replayed stored transcripts as context:
a question saved as `""` meant the model answered *"I don't know your
character class or level"* to a follow-up about a character just described.

**Consequence:** conversation context must live in the live session, never be
reconstructed from transcripts. Store transcripts only as a human-readable
record, and treat the user's side as a possibly-incomplete label.

### Session limits

Without `contextWindowCompression` a session caps around 15 minutes of audio.
With sliding-window compression it can stay open indefinitely — which is the
whole point of the hotkey-opens-a-session model. Session resumption handles
exist but expire 2h after a session ends and a single connection lives ~10
minutes; not worth using for this.

### Google Search grounding

`"tools": [{"google_search": {}}]` works and is worth keeping on by default —
verified returning genuinely current results, not training data. Without it
the model has no search capability at all, and asking it to "web search this"
does nothing.

---

## Gotchas that cost real time

- **rustls needs an explicit crypto provider.** Without
  `rustls::crypto::ring::default_provider().install_default()` at startup, the
  first TLS connection panics with *"Could not automatically determine the
  process-level CryptoProvider"*. Requires the `ring` + `std` features.
- **`cpal` 0.18 API differences:** `config.sample_rate()` returns a plain
  `u32` (not `.0`), and `build_input_stream`/`build_output_stream` take the
  config **by value**.
- **`cpal::Stream` is not `Send` on Linux.** Whatever owns it must stay on one
  thread — a `LocalSet` with `spawn_local`, or plain threads.
- **Linux truncates process names to 15 chars.** `gemini-assistant` is 16, so
  `pkill -x gemini-assistant` silently matches nothing. This is why several
  "cleanup" steps during development did nothing and left daemons running.
  Use the pidfile, or match on the full command line.
- **`pgrep -f '<pattern>'` matches the shell running your script**, because the
  pattern appears in its command line — so a cleanup script can kill its own
  shell. Cost a confusing run of exit-code-144s.
- **Use headphones.** The mic is live while the model speaks. On speakers it
  hears itself, treats that as barge-in, and cuts itself off.
- **Acoustic loopback isn't a test.** On this machine the mic can't hear the
  speakers, so playing a WAV aloud during a session proves nothing. Use
  `send-clip` for automated testing; speech→reply needs a real voice.
- `espeak -w out.wav "text"` generates test clips; `ffmpeg` concat with
  `anullsrc` builds padded/silence variants.

---

## Config

`~/.config/gemini-assistant/config.json`:

```json
{
  "model": "models/gemini-2.5-flash-native-audio-preview-12-2025",
  "system_instruction": "You are a fast, concise voice assistant...",
  "google_search": true,
  "reminder_secs": 30
}
```

Env vars override the file for one-off runs (`GEMINI_LIVE_MODEL`,
`GEMINI_ASSISTANT_REMINDER_SECS`, `GEMINI_ASSISTANT_DEBUG=1` for raw protocol
logging).

**The API key must be readable without a shell.** A KDE shortcut does not
source your profile or any env file, so the binary has to load
`~/.config/gemini-assistant/env` (or equivalent) itself and fall back to the
`GEMINI_API_KEY` environment variable. This was previously handled by the
systemd unit's `EnvironmentFile`, which no longer exists.

Model ids for preview Live models rotate; keep it configurable so a rename
doesn't need a rebuild.

---

## Behaviour details worth keeping

- **Session-open reminder.** Every `reminder_secs` (default 30), notify that
  the session is still open, stating whether the mic is live or paused. A
  forgotten session holds the mic and keeps billing. Set to 0 to disable.
- **Chimes** on session start / end / error, played through the *same* output
  stream as the response audio — a second stream can race the reply and cut
  into it.
- **Notifications** via `notify-send` on each state change.
- **Barge-in**: on `interrupted`, clear the playback queue immediately.

---

## Hotkey binding (KDE)

System Settings → Shortcuts → Custom Shortcuts → New → Global Shortcut →
Command/URL. Bind two:

- Session on/off → `/path/to/gemini-assistant toggle`
- Pause/resume → `/path/to/gemini-assistant pause`

No systemd unit needed — that existed only to start the daemon on demand.

---

## Dependencies

`tokio` (rt, macros, signal, time), `tokio-tungstenite` (rustls-tls-webpki-roots),
`rustls` (ring + std), `futures-util`, `cpal`, `serde`, `serde_json`, `base64`,
`clap` (derive), `anyhow`, `hound` (WAV, for `send-clip` only).

---

## Verification

1. `cargo build --release`, then `send-clip` an espeak clip — confirms key,
   model, network, TLS, and playback independently of sessions.
2. `toggle` → confirm one process appears and claims the pidfile.
3. `toggle` again → confirm the process exits and the pidfile is gone.
4. **Press the hotkey twice rapidly** → confirm you never end up with two live
   processes. This is the failure mode being designed out.
5. `pause` mid-session → confirm the OS mic indicator goes off and the session
   survives; resume and confirm context is retained (ask a follow-up that
   depends on something said before the pause).
6. Real voice test: ask a question, then a context-dependent follow-up, then
   talk over a reply to confirm barge-in.
7. Leave a session open past `reminder_secs` → confirm the nag fires and stops
   on session end.
8. `kill -9` the session, then `toggle` → confirm the stale pidfile is
   detected and cleared rather than blocking startup.

---

## Deferred

The overlay (egui/eframe: transcript with selectable/copyable text,
conversation list, model config) is still wanted, but was explicitly deferred.

Note it can no longer talk to a daemon over a socket. It should read the
transcript files directly and the pidfile for state. If it needs to *control*
a session (start/stop/pause), it can send the same signals the CLI does.

Conversation switching ("go back to an old one") has a real constraint worth
deciding on deliberately: a past conversation's context is gone once its
session ends, and replaying stored transcripts to rebuild it is exactly the
unreliable path that caused the original problem. Resuming an old conversation
will be best-effort — decide whether that's acceptable before building UI that
implies otherwise.
