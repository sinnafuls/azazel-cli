---
name: media-generation
description: Use when producing images, video, 3D models or audio with the generate tools, or sprite sheets from generated frames. Prompt patterns for each kind, how to keep a set of assets in one style, and how to check results before using them.
---
# Media generation

Generated media is cheap to make and expensive to make consistent. Decide the style once, write it down, reuse it in every prompt, and look at every result before it goes into the project.

## Before generating

1. `/media` shows which backends have credentials, the configured model per kind and the output directory. A tool that reports `no <kind> model configured` needs `[media.models] <kind> = "<backend>:<model id>"` in the config, or a `model` argument.
2. Calls cost money and create files; the user is asked to approve them unless the session runs in `auto`.
3. Find what the project already has: existing art, a style guide, palette, resolution, naming scheme. Match it.
4. Write a **style block**: three or four sentences that fix medium, palette, line, lighting, perspective and level of detail. Save it in the project (for example `assets/STYLE.md`) and paste it into every prompt of the set.

Example style block: *Flat vector illustration, thick dark-navy outlines of even weight, limited palette of teal, coral, cream and navy, no gradients, soft single-direction shadow under objects, three-quarter top-down view, plain background.*

## Images (`generate_image`)

Prompt structure, in this order:

1. **Subject**: what it is, specifically ("a wooden treasure chest with iron bands, lid half open").
2. **Composition**: framing, angle, what is centred, how much margin.
3. **Style block**.
4. **Technical constraints**: background, absence of text, absence of extra objects.

Arguments: `size` (`WxH`) or `aspect_ratio`; `count` for variations; `transparent: true` when the model supports alpha; `reference_images` to hold a character or style; `out` to choose the path.

Patterns by purpose:

| Purpose | Add to the prompt |
|---|---|
| Game sprite or icon | "single object, centred, full object visible with margin, plain flat background, no ground, no text" |
| Tileable texture | "seamless tiling texture, top-down, even lighting, no shadows, no perspective" (then check the seams) |
| UI element | "flat, front-facing, exact rectangle, no perspective, no drop shadow outside the shape" |
| Concept art | Mood, time of day, palette; allow composition freedom |
| Character sheet | "same character, front, side and back views, neutral pose, even spacing, plain background" |
| Photo-like | Lens and light in ordinary words: "soft window light from the left, shallow depth of field" |

What goes wrong and what to do:

- Text in images is unreliable: generate without text and add it in the engine or with code.
- Hands, small repeated details and exact counts drift: ask for less, or crop.
- For an exact background colour to key out later, ask for "flat solid #FF00FF background" and remove it with `sprite_sheet`'s `key_color`.
- Change one thing per retry, so you learn what the model responds to.

## Sprite sheets

Two routes:

**Frames you already have** (drawn, generated one by one, or cut from an existing sheet): pass them to `sprite_sheet` with `source.frames` or `source.sheet`. This is deterministic and needs no model.

**Generated grid**: `source: { prompt, cols, rows }` asks the image model for one image holding `cols × rows` equal cells on flat magenta, then cuts, keys and packs it.

Prompts for a generated grid:

- Describe the character or object and the style block.
- Name the action and its phases in order: "walk cycle, 8 frames: contact, down, passing, up, contact, down, passing, up, facing right".
- Demand consistency: "same character, same size and position in every cell, no cell borders, no numbers, no text".
- Keep grids small. 4×1 or 4×2 holds together far better than 8×8.

Always inspect the result: frames aligned, same scale, no body part cut by a cell edge, background fully removed. Animation from generated grids is often uneven; for anything important, generate key poses separately with a reference image and assemble them from files.

Choose `target` for the engine (`godot`, `unity`, `unreal`, `roblox`, `json`) and give `animations` as `{ name, from, to, fps }` ranges over the frame order.

## Video (`generate_video`)

- Describe one shot: subject, action, camera ("slow push in", "static wide shot", "orbit left"), setting, light, mood.
- Short durations hold together; ask for the shortest that shows the idea.
- Start from an image (`image`) when the look must match existing art: generate or pick the still first, then animate it.
- One action per clip. Sequence clips in an editor rather than asking for cuts.
- Expect drift in faces, text and fine geometry; avoid them as the focus.

## 3D models (`generate_3d`)

- Image-to-3D is more controllable than text-to-3D: produce a clean image of the object first (single object, neutral pose, plain background, even light, three-quarter view), check it, then pass it as `image`.
- For text prompts: one object, its material and proportions, "game-ready, single mesh".
- `format`: `glb` unless the engine or pipeline needs `obj` or `fbx`.
- Check the result in the engine or a viewer: scale, orientation (which axis is up), triangle count, texture resolution, holes and floating parts. Generated topology is rarely animation-ready; plan on retopology or use it for static props.

## Audio (`generate_audio`)

| `kind` | Prompt | Notes |
|---|---|---|
| `sfx` | The source, the action, the space: "heavy wooden door slamming shut in a stone hall, short reverb tail" | Keep `duration_secs` tight; ask for one event |
| `music` | Genre, tempo, instruments, mood, structure: "calm loopable ambient, 70 bpm, soft pads and plucked kalimba, no drums, no vocals" | Say "loopable" and check the loop point |
| `speech` | The exact words; `voice` to choose a voice | Check pronunciation of names; keep lines short |

Listen before using when you can; at least check duration, format and loudness with a tool the project has (`ffprobe`). Normalise levels across a set so effects sit together.

## Keeping a set consistent

- Same style block, same model, same size or aspect ratio for the whole set.
- Use `reference_images` with one approved asset as the anchor for the rest.
- Generate related assets in one sitting, compare them side by side (view the files), and regenerate the outliers rather than accepting them.
- Keep the prompt, model and parameters of everything you keep. The tools write a `.json` sidecar beside each file; do not delete it.
- Name files by what they are (`chest_open.png`, `door_slam.ogg`), not by prompt fragments. Use `out` to place them where the project expects.

## Checking results

1. Look at each image (`read` the file) before using it. Check subject, framing, style match, artefacts, background, and unwanted text or watermarks.
2. Check technical fit: dimensions, transparency, file format, size on disk.
3. Put it in the project and look at it in context: in the engine, in the page (`browser` screenshot), at the size it will be shown.
4. Tell the user what was generated, with paths, prompts and models, and what you rejected and why.

## Rights and content

- Do not ask for a living artist's named style, a real person's likeness, or a trademarked character, unless the user has the right to it and says so.
- Check the chosen model's licence terms for the intended use when the user has not already; say so if unknown.
- Mark generated assets as such where the project tracks provenance.
