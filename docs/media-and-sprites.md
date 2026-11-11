# Media generation and sprite sheets

Azazel can generate images, video, 3D models and audio through hosted services and save them into the project, and it can pack animation frames into a sprite sheet with a descriptor for Godot, Unity, Unreal, Roblox or generic JSON. The tools are `generate_image`, `generate_video`, `generate_3d`, `generate_audio` and `sprite_sheet` (cargo feature `media`, on by default). No model ids ship with azazel: which model generates what is configuration.

## Backends and credentials

| Backend | Generates | Credentials |
|---|---|---|
| `fal` | image, video, 3D, audio | env `FAL_KEY`, or an entry `"fal": {"type": "api_key", "key": "..."}` in `~/.azazel/auth.json` |
| `openai` | image | The `openai` provider's credentials (`OPENAI_API_KEY`, or `azazel login openai`) |
| `google` | image | The `google` provider's credentials (`GEMINI_API_KEY` or `GOOGLE_API_KEY`, or `azazel login google`) |

A backend is usable when its credentials are present at startup. The four `generate_*` tools are registered only when at least one backend is usable; `sprite_sheet` and `/media` are always there. See [models and logins](models-and-logins.md) for provider credentials.

## Configuration

```toml
[media]
output_dir = "assets/generated"     # relative to the project root

[media.models]                      # "<backend>:<model id>"
image   = "fal:<fal model path>"
video   = ""
model3d = ""
audio   = ""
```

| Key | Default | Meaning |
|---|---|---|
| `media.output_dir` | `assets/generated` | Where generated files and sprite sheets go |
| `media.models.image` / `video` / `model3d` / `audio` | empty | The model for each kind, as `<backend>:<model id>` |

The part before the first `:` is the backend (`fal`, `openai`, `google`); the rest is that service's own model id, passed through unchanged. Every tool also takes a `model` input in the same form, which overrides the configured one for that call. With neither set the tool answers `no <kind> model configured: set [media.models] <kind> = "<backend>:<model id>"`.

`/media` shows the usable backends, the configured model per kind and the output directory.

## Generation tools

| Tool | Inputs |
|---|---|
| `generate_image` | `prompt`, `size?` (`1024x1024`), `aspect_ratio?` (`16:9`), `count?` (1 to 8), `transparent?`, `reference_images?` (paths), `model?`, `out?` |
| `generate_video` | `prompt`, `image?` (path of the first frame), `duration_secs?`, `aspect_ratio?`, `model?`, `out?` |
| `generate_3d` | `prompt?` or `image?` (at least one), `format?` (`glb` default, `obj`, `fbx`), `model?`, `out?` |
| `generate_audio` | `prompt`, `kind` (`sfx`, `music`, `speech`), `duration_secs?`, `voice?`, `model?`, `out?` |

Input paths are relative to the project root. Each backend translates the inputs to its service's parameter names; a model may ignore options it does not support.

### Output files

Without `out`, a file is written to

```
<project>/<media.output_dir>/<kind>/<slug>-<UTC timestamp>[-<n>].<ext>
```

where `<kind>` is `image`, `video`, `model3d` or `audio`, `<slug>` is the first six words of the prompt in lower case joined by hyphens, and `-<n>` appears when one call produced several files. For example:

```
assets/generated/image/a-wooden-treasure-chest-lid-half-20261111T093000Z.png
```

With `out`, that path (relative to the project root) is used. It must stay inside the project; a missing extension is filled in from the generated file's type, and `-<n>` is inserted before the extension when there are several files.

Beside every file a sidecar `<file>.json` records how it was made:

```json
{
  "kind": "image",
  "prompt": "A wooden treasure chest, lid half open",
  "model": "fal:<fal model path>",
  "parameters": { "size": "1024x1024", "transparent": true },
  "inputs": [],
  "seed": 7,
  "created_at": "2026-11-11T09:30:00Z"
}
```

`seed` is the seed the service reports, or `null`. The tool result lists each path with its size and type; generated images are also returned inline (long side at most 1024 pixels) so a model with vision can look at them.

### Permissions and cost

The generation tools use the network, create project files and may cost money at the provider. They prompt in `ask` and `accept-edits` mode, with the note `this creates files in the project and may cost money at the media provider`, and run without asking in `auto`. To allow one permanently, add its name as a rule, for example `allow = ["generate_image"]`. See [permissions](permissions.md).

A fal job is polled every 2 seconds and given up after 30 minutes. Aborting the agent cancels the wait.

## `sprite_sheet`

Packs frames into one or more sheet images and writes a descriptor for the target engine. It needs no backend unless the frames are generated from a prompt.

| Input | Default | Meaning |
|---|---|---|
| `source` | required | Exactly one of `{ "frames": [paths] }`, `{ "sheet": path, "cols": n, "rows": n }`, `{ "prompt": text, "cols": n, "rows": n }` |
| `name` | required | Name of the sheet and its files: 1 to 64 letters, digits, `_` or `-` |
| `target` | required | `json`, `godot`, `unity`, `roblox` or `unreal` |
| `animations` | none | `[{ "name", "from", "to", "fps" }]`; frame numbers are 0-based in source order and `to` is inclusive |
| `key_color` | `none` (`auto` for prompt sources) | `none`, `auto` or `#RRGGBB`: the background colour to make transparent |
| `tolerance` | 24 | How far (Euclidean RGB distance) a pixel may be from the key colour and still be removed |
| `trim` | `true` | Cut transparent borders off each frame |
| `padding` | 2 | Pixels between frames |
| `max_size` | 4096 | Largest sheet side in pixels |
| `out_dir` | `<media.output_dir>/sprites/<name>` | Output directory, relative to the project root and inside it |

```json
{
  "source": { "sheet": "art/hero_walk.png", "cols": 4, "rows": 2 },
  "name": "hero",
  "animations": [
    { "name": "walk", "from": 0, "to": 3, "fps": 8 },
    { "name": "idle", "from": 4, "to": 7, "fps": 4 }
  ],
  "key_color": "auto",
  "target": "godot"
}
```

This writes `assets/generated/sprites/hero/hero.png` and `hero.tres`.

### Pipeline

1. **Frames.** Listed files are used in order. A sheet is cut into `cols` × `rows` equal cells, row by row from the top left (at most 1024 cells; leftover pixels at the right and bottom are dropped). A prompt source asks the configured image model for such a grid on a flat `#FF00FF` background and cuts it the same way; it needs `media.models.image`.
2. **Colour key.** `auto` takes the most common colour of the first frame's four corner pixels. Every pixel within `tolerance` of the key becomes fully transparent.
3. **Trim.** Transparent borders are removed per frame; the offset and original size are kept for the descriptor.
4. **Naming.** A frame inside an animation is `<animation>_<i>` (counting from 0); any other frame is `<name>_<4-digit index>`.
5. **Pack.** MaxRects, best-short-side fit, no rotation, tallest frames first, so the same input always gives the same sheet. A page is square, starts at 64 pixels and doubles up to `max_size`; frames that do not fit go to further pages `<name>-2.png`, `<name>-3.png`, and so on.
6. **Export.** Descriptor files are written beside the image(s).

### Targets

| Target | Files | Notes |
|---|---|---|
| `json` | `<name>.png`, `<name>.json` | TexturePacker JSON-hash: `frames` (with `frame`, `rotated`, `trimmed`, `spriteSourceSize`, `sourceSize`), `animations`, `meta` (`image`, `size`, `scale`). One document per page; the first lists the others in `meta.related_multi_packs` |
| `godot` | `<name>.png`, `<name>.tres` | Godot 4 `SpriteFrames` text resource with one `AtlasTexture` per frame. The image is referenced as `res://<out_dir>/<name>.png`, so the azazel project root should be the Godot project root. Frames in no animation go into an animation `default` at 5 fps |
| `unity` | `<name>.png`, `<name>.png.meta`, `<name>.animations.json` | `TextureImporter` meta with sprite mode Multiple and one rect per frame (measured from the bottom left). Frames are never trimmed for Unity. Animations are listed as JSON for an editor script to build clips from |
| `roblox` | `<name>.png`, `<name>.luau` | Sheet side limited to 1024. A ModuleScript returning `image`, `size`, `frames` (`offset` and `size` for `ImageRectOffset` and `ImageRectSize`) and `animations`. `image` is `rbxassetid://0` until you upload the sheet and fill in its asset id |
| `unreal` | `<name>.png`, `<name>.paper2dsprites` | The JSON-hash document with a pivot per frame, for the Paper2D sprite-sheet importer |

`sprite_sheet` writes files, so it prompts in `ask` and `accept-edits` mode unless the rule `sprite_sheet` allows it.

## Related

The built-in `artist` agent has exactly these tools plus `read`, `write` and `shell`; delegate asset work to it with `task` (see [team](team.md)). The built-in skills `media-generation` and `game-dev` hold prompt patterns and engine conventions (see [skills and commands](skills-and-commands.md)). Other tools are listed in [tools](tools.md).
