# Voice

Azazel can take a prompt by voice and read its replies aloud (cargo feature `voice`, on by default). It records
from the microphone, sends the recording to a speech-to-text service, and puts the words in the input. For
spoken replies it sends the text of a reply to a text-to-speech service and plays the audio.

Nothing ships with a speech model. Both directions use the OpenAI-style audio endpoints of a
[provider](models-and-logins.md) you already have, or a program on your machine.

## Set-up

```toml
# ~/.azazel/config.toml
[voice]
stt = "openai/whisper-1"       # speech to text: "<provider>/<model>"
tts = "openai/tts-1"           # text to speech: "<provider>/<model>"
voice = "alloy"                # the voice the speech model reads with
```

| Key | Default | Meaning |
|---|---|---|
| `voice.stt` | empty | Provider and model for transcription. Needed to dictate, unless `stt_command` is set |
| `voice.stt_command` | empty | A program that transcribes a WAV file instead; wins over `stt` |
| `voice.language` | empty | Language of what you dictate, as a code such as `en` or `de`; empty lets the service detect it |
| `voice.auto_send` | `false` | Send the transcript as a prompt at once instead of leaving it in the input |
| `voice.max_secs` | `120` | A recording stops by itself after this many seconds; `0` leaves only the size limit |
| `voice.record_command` | empty | A program that records instead of the built-in recorder |
| `voice.tts` | empty | Provider and model for spoken replies. Needed for `/speak on` |
| `voice.voice` | empty | Voice name; empty asks for `alloy` |
| `voice.play_command` | empty | A program that plays the speech instead of the built-in player |

`record_command`, `stt_command` and `play_command` name programs to run, so they are
[trust-gated](configuration.md#trust): a project's config can only set them once you trust the project.

`azazel doctor` reports the recorder, both speech settings, the player and what is missing.

### Services

`stt` and `tts` name any provider from [models and logins](models-and-logins.md) with the model after the first
`/`. The provider's address and credential are used as they are; only the path differs:

| Step | Request |
|---|---|
| Transcription | `POST {base_url}/audio/transcriptions`, multipart: `file` (`recording.wav`), `model`, and `language` when set. The reply is JSON with a `text` field |
| Speech | `POST {base_url}/audio/speech`, JSON: `model`, `voice`, `input`, `response_format: "wav"`. The reply is the WAV audio |

That is the API of OpenAI and of Groq (`stt = "groq/whisper-large-v3-turbo"`), and of local servers that copy
it. A local Whisper server is a provider of its own, for example in `~/.azazel/models.yml`:

```yaml
providers:
  whisper:
    api: openai-chat
    base_url: http://localhost:8000/v1
```

```toml
[voice]
stt = "whisper/whisper-large-v3"
```

A request may take 60 seconds. The subscription logins (Claude, ChatGPT) do not serve these endpoints; name a
provider you use with an API key, or a local server.

### Programs instead of a service

`stt_command` runs a program on the recording and reads the transcript from what it prints. `{file}` is replaced
by the path of a temporary 16 kHz mono WAV file; without `{file}` the path is added as the last argument. The
program has 60 seconds.

```toml
[voice]
stt_command = "whisper-cli -m /models/ggml-base.en.bin -nt -np -f {file}"   # whisper.cpp
```

All three commands are run directly, not through a shell: quotes group words, but there are no pipes, variables
or redirections. Put anything more involved in a script and name the script.

## Recording

| System | Recorder |
|---|---|
| Windows, macOS | The default input device, through the system's audio API |
| Linux and others | A recording program: the first installed of `arecord`, `pw-record`, `sox`, `ffmpeg` |

Linux records through a program because linking its audio library into azazel would stop the binary from
starting on machines without that library. On macOS the terminal asks for microphone access the first time.

`record_command` replaces the recorder on every system. The program must write audio to its standard output
until it is stopped: either WAV (any sample rate, mono or stereo) or headerless 16 kHz mono signed 16-bit
little-endian samples.

```toml
[voice]
record_command = "arecord -q -D plughw:1 -f S16_LE -r 16000 -c 1 -t wav"
```

Whatever records, the result is converted to 16 kHz mono 16-bit WAV before it is sent. A recording ends by
itself after `voice.max_secs`, at 10 MiB (a little over five minutes), or when the recording program exits.

## Dictating

| | |
|---|---|
| `Ctrl+Space` or `/voice` | Start recording. Again: stop, transcribe, and put the words in the input at the cursor |
| `Esc` | Cancel the recording or the transcription; nothing is inserted |

The key is the action `voice` in [`keybindings.toml`](tui.md#changing-keys); some terminals do not pass
`Ctrl+Space` on, so bind another key there or use `/voice`. Press and let go: a key that is held down repeats,
and presses less than 0.7 seconds apart are taken for such repeats and ignored.

While you record, the rule above the input shows `● rec 0:07`, a level meter that moves with your voice, and the
keys. After the second press it shows `transcribing` until the text arrives. A flat meter means the microphone
hears nothing: check the input device.

The transcript goes into the input, after a space when it follows a word, so you can dictate in pieces and edit
before sending. With `voice.auto_send = true` it is sent as soon as it arrives. Dictating works while the agent
is busy; what you send then steers it, like typed text.

## Spoken replies

| Command | Effect |
|---|---|
| `/speak on` | Read replies aloud from now on, until azazel exits |
| `/speak off` | Stop reading replies aloud |
| `/speak stop` | Stop the reply being read now |
| `/speak` | Show whether spoken replies are on |

What is spoken:

- only the lead agent's final reply of a run; nothing while it works, and nothing from sub-agents
- not a run you interrupted
- code blocks are replaced by "Code omitted."; Markdown markers, link targets and web addresses are left out
- at most 1,200 characters, cut at the end of a sentence

`Esc` or any key stops the playback, and so does starting a recording.

| System | Player |
|---|---|
| Windows, macOS | The default output device, through the system's audio API |
| Linux and others | A player program: the first installed of `ffplay`, `afplay`, `paplay`, `aplay` |

`play_command` replaces the player on every system. It gets the path of a temporary WAV file where it says
`{file}`, or as its last argument, and must exit when the audio has played.

```toml
[voice]
play_command = "ffplay -nodisp -autoexit -loglevel quiet"
```

## Messages

| Message | What to do |
|---|---|
| `voice input is not set up: …` | Set `voice.stt` or `voice.stt_command` |
| `cannot record: no microphone found` | Connect or enable an input device; `azazel doctor` names the one in use |
| `cannot record: no recording program found: …` | Install one of the listed programs, or set `voice.record_command` |
| `<program> recorded nothing: …` | The recording program failed; the text after the colon is its own last message |
| `heard nothing` | The service returned no words: the recording was silent or too short |
| `transcription failed: <provider> answered 401 …` | The provider's credential is missing or wrong; see `azazel doctor` |
| `spoken replies are not set up: …` | Set `voice.tts` |

Error messages never contain the provider's address or key.
