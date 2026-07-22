# gemini-assistant

Hotkey-driven, real-time spoken conversation with Gemini Live. No daemon —
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

(Falls back to the `GEMINI_API_KEY` env var if that file is absent — handy
for terminal/manual runs.)

## Config (optional)

`~/.config/gemini-assistant/config.json`, all fields optional:

```json
{
  "model": "models/gemini-2.5-flash-native-audio-preview-12-2025",
  "voice": "Puck",
  "system_instruction": "You are a fast, concise voice assistant...",
  "google_search": true,
  "reminder_secs": 30,
  "context_window_target_tokens": 16000,
  "vad_threshold": 400.0
}
```

`reminder_secs: 0` disables the "still open" reminder. Env vars
(`GEMINI_LIVE_MODEL`, `GEMINI_ASSISTANT_REMINDER_SECS`,
`GEMINI_ASSISTANT_VAD_THRESHOLD`) override individual fields for a one-off
run. `GEMINI_ASSISTANT_DEBUG=1` prints raw session events to stderr.

## Commands

| Command | Behaviour |
|---|---|
| `gemini-assistant` / `toggle` | Start a session if idle, end it if one's running. Bind your main hotkey to this. |
| `gemini-assistant pause` | Toggle mic on/off without ending the session — context is kept. Bind a second hotkey to this. |
| `gemini-assistant status` | Print `live` / `paused` / `stopped`. |
| `gemini-assistant send-clip <wav>` | Diagnostic: send a WAV straight to the API and play back the reply, bypassing the mic/pidfile entirely. Good for checking the key/model/network without a microphone. |

## KDE hotkeys

System Settings → Shortcuts → Custom Shortcuts → New → Global Shortcut →
Command/URL. Bind two:

- Session on/off → `/full/path/to/target/release/gemini-assistant toggle`
- Pause/resume → `/full/path/to/target/release/gemini-assistant pause`

## Verifying it works

`send-clip` needs no microphone and exercises the whole pipeline (key,
model, TLS, wire protocol, playback):

```
espeak -w /tmp/clip.wav "what's the capital of France"
./target/release/gemini-assistant send-clip /tmp/clip.wav
```

Full mic/pause/barge-in behavior needs real hardware — headphones
specifically, since the mic stays live while the model talks, and on
speakers it can hear (and interrupt) itself.
