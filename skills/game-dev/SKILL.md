---
name: game-dev
description: >-
  Use when working on a game project in Godot, Unity, Unreal Engine, Roblox or Bevy: finding your way around the project, following the engine's conventions, producing and importing assets with the generate tools and sprite_sheet, and building, running and playtesting from the command line.
---
# Game development

Engines differ in where logic lives, how assets are imported and what may be edited as text. Identify the engine first, follow its conventions, and verify by running the game, not by reading the code.

## 1. Identify the project

| Marker | Engine | Language |
|---|---|---|
| `project.godot` | Godot 4 (check `config/features` for the version) | GDScript (`.gd`), C# when a `.csproj` exists |
| `ProjectSettings/ProjectVersion.txt`, `Assets/`, `Packages/manifest.json` | Unity | C# |
| `*.uproject`, `Source/`, `Content/` | Unreal Engine | C++ and Blueprints |
| `default.project.json`, `*.rbxl`, `*.rbxlx`, `wally.toml` | Roblox | Luau |
| `Cargo.toml` with a `bevy` dependency | Bevy | Rust |

Read the project's own notes first (`AGENTS.md`, README, a `docs/` folder), then the engine version, then the entry scene or main function.

## 2. Conventions by engine

### Godot
- Scenes (`.tscn`) and resources (`.tres`) are text and may be edited carefully; keep `uid` and `ext_resource` ids consistent. Prefer small edits; let the editor do large scene surgery.
- One script per node responsibility; signals for communication upward, direct calls downward; autoloads for global services only.
- Paths are `res://`. Never touch `.godot/` (the import cache) or `.import` files by hand.
- Typed GDScript (`var speed: float = 4.0`, `func move(delta: float) -> void`).
- Run headless: `godot --headless --path . --quit` loads the project and reports script errors; `godot --path . --import` reimports assets; `godot --path . <scene.tscn>` runs a scene; export with `godot --headless --export-release "<preset>" <output>`.
- Tests: GUT or GdUnit4 when the project has one (`addons/`); otherwise a headless scene that asserts and quits with a code.

### Unity
- Everything under `Assets/` has a `.meta` file holding its GUID. Create, move and delete assets together with their `.meta`; never regenerate a GUID, or references break.
- Scenes and prefabs are YAML but fragile; prefer changing scripts and ScriptableObject assets, and describe needed scene wiring for the user when it cannot be done safely as text.
- `MonoBehaviour` for components, ScriptableObjects for data, assembly definitions (`.asmdef`) for module boundaries. Avoid `Find` and per-frame allocations in `Update`.
- Batch mode: `Unity -batchmode -quit -projectPath . -executeMethod <Class.Method> -logFile -` runs an editor method; `-runTests -testPlatform EditMode|PlayMode -testResults results.xml` runs the test runner; builds go through a build script method.
- Never edit `Library/`, `Temp/`, `Obj/`.

### Unreal Engine
- C++ under `Source/<Module>/` with `Public/` and `Private/`; `UCLASS`, `UPROPERTY`, `UFUNCTION` macros expose things to the editor and Blueprints. Blueprints (`.uasset`) are binary: do not try to edit them; expose what is needed from C++ and tell the user what to wire.
- Follow the prefix rules (`A` actors, `U` objects, `F` structs, `E` enums, `I` interfaces) and the module's `*.Build.cs` for dependencies.
- Build: `<Engine>/Build/BatchFiles/Build.bat <Project>Editor Win64 Development -Project=<path.uproject>` (or `RunUAT BuildCookRun` for a packaged build). Automation tests: `UnrealEditor-Cmd <project> -ExecCmds="Automation RunTests <filter>; Quit" -unattended -nullrhi -log`.
- Never edit `Intermediate/`, `Saved/`, `DerivedDataCache/`, `Binaries/`.

### Roblox
- With Rojo, source lives in the repository and `default.project.json` maps folders into the data model. `.server.luau` runs on the server, `.client.luau` on the client, plain `.luau` is a ModuleScript.
- Server authority: validate everything a client sends through RemoteEvents and RemoteFunctions; never trust client-reported positions, currency or damage.
- Shared code in `ReplicatedStorage`, server-only code in `ServerScriptService`, client code in `StarterPlayerScripts`. Use strict Luau types (`--!strict`).
- Tooling: `rojo build -o game.rbxlx`, `rojo serve`; `selene` and `stylua` for lint and format; `wally install` for packages; tests with TestEZ or Jest-Lua through `run-in-roblox` or Lune where set up.
- Images must be uploaded to get an asset id; generated assets are files until the user uploads them.

### Bevy
- An ECS: data in components, behaviour in systems, shared state in resources, features grouped into plugins. Query for what a system needs and nothing more.
- Schedule systems explicitly (`Startup`, `Update`, `FixedUpdate`), order them with sets, and gate them with states and run conditions.
- Check the Bevy version in `Cargo.toml` before writing any API; it changes between minor versions. Read the project's existing systems for the idioms of that version.
- Assets load through the `AssetServer` from `assets/`; handles are asynchronous.
- `cargo run` (enable `bevy/dynamic_linking` in development for faster rebuilds when the project already does); `cargo test` for logic kept in plain functions and for systems run in a minimal `App`.

## 3. General practice

- Separate simulation from presentation. Logic that can be a pure function should be, so it can be tested without the engine.
- Frame-rate independence: scale movement by delta time; put physics and gameplay rules in the fixed step.
- Data-driven content: values in resources, ScriptableObjects, data assets or config files, not constants spread through code.
- Avoid work per frame that can be done on change: searching the scene tree, allocating, string formatting.
- Input goes through the engine's action mapping, never raw key codes scattered in gameplay code.
- Keep generated and imported artefacts out of version control as the engine recommends; check `.gitignore` before adding files.

## 4. Assets

Where assets go:

| Engine | Location | Import |
|---|---|---|
| Godot | Anywhere under the project, commonly `assets/` or `art/` | Automatic on next editor run or `--import`; settings in the `.import` file |
| Unity | `Assets/…` | Automatic; settings in the `.meta`. Sprites need Texture Type "Sprite" |
| Unreal | Source files outside `Content/`; imported into `Content/` through the editor or an import script | Editor import creates `.uasset` |
| Roblox | Source in the repository; uploaded to Roblox for an asset id | Manual or through an upload tool |
| Bevy | `assets/` | Loaded at run time |

Producing assets with the harness (when media models are configured; `/media` shows what is available):

- `generate_image { prompt, size?, aspect_ratio?, transparent?, reference_images?, out? }` for concept art, textures, UI, single sprites.
- `generate_3d { prompt? | image?, format? }` for models (`glb` by default; check what the engine imports).
- `generate_audio { prompt, kind: "sfx" | "music" | "speech", duration_secs? }`.
- `generate_video { prompt, image?, duration_secs? }` for trailers and cutscene drafts.
- `sprite_sheet` turns frames into an engine-ready sheet. It needs no model unless frames are generated from a prompt.

Sprite sheets:

```json
{
  "source": { "frames": ["art/walk_0.png", "art/walk_1.png", "art/walk_2.png", "art/walk_3.png"] },
  "name": "hero",
  "animations": [{ "name": "walk", "from": 0, "to": 3, "fps": 10 }],
  "target": "godot",
  "out_dir": "assets/sprites/hero"
}
```

`source` may also be `{ "sheet": path, "cols", "rows" }` to cut an existing grid, or `{ "prompt", "cols", "rows" }` to generate one. `key_color` removes a flat background (`auto`, or a hex colour). Targets and what they write:

| `target` | Output | Use |
|---|---|---|
| `godot` | `.png` + `.tres` (`SpriteFrames`) | Assign to an `AnimatedSprite2D` |
| `unity` | `.png` + `.png.meta` (sliced) + `.animations.json` | Drop into `Assets/`; build clips from the JSON |
| `unreal` | `.png` + `.paper2dsprites` | Import with Paper2D |
| `roblox` | `.png` + `.luau` module with offsets and sizes | Upload the image, set its asset id in the module, drive `ImageRectOffset`/`ImageRectSize` |
| `json` | `.png` + TexturePacker-style `.json` | Bevy and custom loaders |

For style consistency across generated assets, see the `media-generation` skill. Record prompts and models beside the assets; the tools write a `.json` sidecar for this.

## 5. Build, run, verify

1. Find the project's own commands (README, CI workflow, `Makefile`, `justfile`). Use them.
2. After changing code, at least load or compile the project headlessly (commands above) and read the full log for script and import errors.
3. Run the automated tests the project has.
4. Run the game: start it as a background `shell` job and use the `playtest` skill (or `/playtest <launch command>`) to exercise the change through `screen`, or `browser` for web builds. A feature that compiles is not a feature that works.
5. Report what you ran, what you observed, the frames that show it, and anything that needs doing in the editor by hand (scene wiring, Blueprint changes, asset uploads).
