# Mimikiku Capture

Terminal app that captures system audio (whatever your speakers play) or a mic, streams it to a Mimikiku server, and shows the speaker-labeled transcript as it arrives.

## Features

- Live transcript, colored by speaker, with optional timestamps and scrollback
- Level meter, so you can see audio is flowing
- Pause and resume without disconnecting
- Automatic reconnect with a countdown
- Server status in the header (for example `loading` while models start)
- Optional raw JSON output to a file

## Build

Needs a Rust toolchain with edition 2024 support. On Linux, the audio library needs the ALSA headers (`libasound2-dev` on Debian and Ubuntu).

```shell
cargo build --release
```

## Setup

Settings live in `.env` in this folder:

```shell
cp .env.example .env    # then set MIMIKIKU_WS_URL and MIMIKIKU_TOKEN
```

Every option has a `MIMIKIKU_*` variable. Environment variables override `.env`, and flags override both. Only `./.env` is read.

Create a personal API token in the web app (Account → API tokens) and put it in `MIMIKIKU_TOKEN`. Transcripts then save to your account and show up in the web app. A bad or revoked token stops the app with a message.

## Run

```shell
./start.sh                          # builds the release binary and runs it with .env
./start.sh --device monitor --no-save
```

The app starts idle: it connects and shows the level meter, but sends nothing until you press `Space`. Pass `--autostart` to skip that.

Without the script:

```shell
cargo run --release
```

## Options

| Option | Default | Description |
|---|---|---|
| `--ws-url` | `ws://localhost:7777/listen` | Server WebSocket URL |
| `--token` | none | Personal API token |
| `--no-save` | off | Keep the transcript local, not on your account |
| `--transcript` | none | Add recordings to an existing transcript (id from `/#/t/<id>`) |
| `--device` | first "monitor" or "pipewire" device, then the default input | Input device name, part of it is enough, case-insensitive |
| `--list-devices` | | Print input devices and exit |
| `--channels` | `1` | Input channels, downmixed to mono before sending |
| `--rate` | `16000` | Sample rate in Hz |
| `--chunk` | `2048` | Frames per buffer |
| `--silence-threshold` | `0` | Skip chunks at or below this RMS level |
| `--autostart` | off | Start listening immediately |
| `--output` | none | Append raw transcript JSON to this file |
| `--save-dir` | `.` | Folder for transcripts saved with `s` |

If no matching device is found, the program exits before the UI starts.

## Keys

| Key | Action |
|---|---|
| `q`, `Esc`, `Ctrl-C` | Quit |
| `Space`, `p`, `Enter` | Start listening, then pause or resume |
| `↑` `↓`, `k` `j` | Scroll one line |
| `PgUp` `PgDn` | Scroll one page |
| `Home`, `g` | Top |
| `End`, `G` | Bottom, follow live output |
| `t` | Toggle timestamps |
| `s` | Save the transcript as text to `mimikiku-<UTC time>.txt` in `--save-dir` |
| `c` | Clear the view (the output file is not touched) |

Scrolling up holds your place while new text arrives. The footer shows segment and word counts, and chunks sent and dropped. Dropped chunks mean the connection can't keep up.

## Output format

With `--output`, each server message that contains segments becomes one line of JSON:

```json
[{"language": "en", "start": 0.0, "end": 1.5, "speaker": 0, "text": "Hello world"}]
```

Follow it from another terminal:

```shell
tail -f out/transcription.jsonl | jq -r '.[] | "[\(.speaker)] \(.text)"'
```

## System audio

### Linux (PulseAudio or PipeWire)

Capture needs a "monitor" input, which carries speaker output. List them with:

```shell
pactl list sources short | grep monitor
```

By default the app picks the first device whose name contains "monitor" or "pipewire". If it picks the wrong one, run `--list-devices` and pass part of the right name to `--device`.

### macOS

macOS has no monitor input, so capture doesn't work out of the box. See [MACOS-NOTES.md](MACOS-NOTES.md).

## Development

```shell
cargo test
cargo clippy
```

Layout: a library crate plus a thin binary.

- `src/lib/audio.rs`: capture and device selection
- `src/lib/net.rs`: WebSocket session, reconnect, message types
- `src/lib/ui.rs`: TUI state and drawing
- `src/bin/mimikiku-capture.rs`: arguments and the event loop

## License

Copyright 2026 codingthings. Licensed under the [Apache License 2.0](LICENSE).
