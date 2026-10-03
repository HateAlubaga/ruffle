# Ruffle Modkit

Ruffle Modkit is a fork of [Ruffle](https://github.com/ruffle-rs/ruffle) that adds live game-editing tools for
ActionScript 3 games. Everything sits behind the cargo feature `modkit`, so a normal Ruffle build is unchanged.

It is not affiliated with the Ruffle project. Ruffle is dual-licensed MIT / Apache-2.0 (see `LICENSE.md`);
the Modkit code is released under the same terms.

> Status: experimental. The editing logic has automated tests that run a real SWF headlessly, but the GUI has
> only had light manual testing. Expect rough edges and report them.

## What it does

* **Pause, edit, resume.** Open the Modkit window (`Ctrl+Shift+M`), pause the game, pick objects on stage,
  change their properties, and resume. Type checks reject values the game could not hold. Undo is available.
* **Save your edits.** `💾 Save edits (N)` turns every property you changed in this session (by hand or through
  the AI) into rules in a mod file for the current game.
* **Speed.** Game time runs from ×0.25 to ×4 (`Ctrl+[`, `Ctrl+]`, `Ctrl+\` for normal speed).
* **Overlay mods.** TOML files in the mods folder change game objects while the game runs, without touching the
  SWF. Rules are tied to a class, so they keep working as the game creates and destroys objects.
* **AI edits.** Describe a change in words ("make the boss weaker"). A JSON snapshot of the selected objects goes
  to an LLM, which answers with a strict JSON patch. The patch is validated and shown as a preview; nothing
  changes until you press `Apply` (or `Apply & save as rule`). The game pauses while the AI works and resumes when
  you apply or dismiss the patch (turn this off in the settings).
* **FFDec bridge.** Shows the decompiled source of the selected object's class, or opens it in
  [JPEXS Free Flash Decompiler](https://github.com/jindrapetrik/jpexs-decompiler). FFDec runs as a separate
  process and is not bundled.

## Build

```sh
cargo build --release -p ruffle_desktop --features modkit
```

The binary is `target/release/ruffle_desktop` (`.exe` on Windows). This is the normal Ruffle desktop build
with one extra feature; see Ruffle's own README for the toolchain.

## Use

1. Run `ruffle_desktop`, open a SWF, then use the **Modkit** menu or press `Ctrl+Shift+M`.
2. Click **Pick**, then click an object on stage (`Shift+click` adds to the selection).
3. Edit values in the property list. **Pause** first if you want a still picture while editing.
4. **Save edits** keeps them as a mod file, so they apply again the next time the game starts.

### Overlay mod files

Mod files live in `mods/` inside Ruffle's config folder (on Windows `%LOCALAPPDATA%\ruffle\mods`). They are
re-read when they change.

```toml
[mod]
name = "Weak bosses"
swf_sha256 = "…"       # optional; without it the mod applies to every SWF

[[rule]]
class = "Boss"         # dotted name, or just the class name
on = "create"          # "create", "first_frame" or "every_frame" (freezes the value)
target = "instance"    # default; "static" edits the class object itself
exact = false          # default: the class or any subclass
[rule.set]
hp = 10
speed = 0.5
```

Many games set their stats later than the constructor. If `create` is overwritten by the game, use
`first_frame`, or `every_frame` to hold a value.

### AI setup

Open the Modkit window, then Settings. Set a provider, base URL, model and key, or use environment variables:

| Setting | Environment variable |
| --- | --- |
| Model | `RUFFLE_MODKIT_MODEL` |
| API key (OpenAI-compatible) | `RUFFLE_MODKIT_API_KEY`, then `OPENAI_API_KEY`, then `OMNIROUTE_API_KEY` |
| API key (Anthropic) | `ANTHROPIC_API_KEY` |

Any OpenAI-compatible `/chat/completions` endpoint works (OpenAI, OpenRouter, Ollama, a local gateway), as does
the Anthropic API. The default base URL for the OpenAI-compatible provider is `http://127.0.0.1:20128/v1`;
change it in the settings. Settings are stored in `modkit.json` in the config folder. A key typed into the
settings is saved there in plain text, so prefer the environment variables on a shared machine.

Only a snapshot of the objects you selected is sent to the model, never the SWF itself.

### FFDec

Set the path to `ffdec-cli.exe`, `ffdec.sh` or `ffdec.jar` in the Modkit settings.

## Layout of the code

* `core/src/modkit.rs` and `core/src/modkit/` — editing model, inspector, mod runtime, speed, UI.
* `desktop/src/modkit.rs` and `desktop/src/modkit/` — settings, network, processes and files.
* A few one-line hooks in upstream files (`core/src/player.rs`, `core/src/avm2.rs`, `desktop/src/gui.rs`, …),
  all behind `#[cfg(feature = "modkit")]`.

## Tests

```sh
cargo test -p ruffle_core --features modkit modkit
cargo test -p ruffle_desktop --features modkit modkit
```

Some tests run a real SWF; point `RUFFLE_MODKIT_TEST_SWF` at one to enable them.
