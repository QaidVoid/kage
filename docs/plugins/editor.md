# editor setup

kage ships a `lua-language-server` definition stub for the whole
`kage.*` API. Point your editor's Lua language server at it and you
get, inside any `~/.config/kage/plugins/*.lua` file:

- completion on `kage.<Ctrl-Space>` and inside spec tables
  (`kage.register_tool({ na<Ctrl-Space>` suggests `name`,
  `description`, `schema`, `execute`),
- hover docs pulled from the same text as this site,
- argument and field type checking, with diagnostics for typos,
- literal-string completion for event names
  (`kage.on("agent_st<Ctrl-Space>")` finishes `agent_start`).

## what `kage init` does

`kage init` sets this up for you. It:

1. writes the stub to `~/.local/share/kage/types/kage.lua`, and
2. writes (or merges into) `~/.config/kage/plugins/.luarc.json`:

   ```json
   {
     "workspace": {
       "library": ["/home/you/.local/share/kage/types"]
     }
   }
   ```

`.luarc.json` is `lua-language-server`'s per-directory project file.
`workspace.library` tells the server to load extra definition files
when editing anything under that directory, so the stub applies to
every plugin without per-file `---@module` lines.

The merge is idempotent and non-destructive: rerunning `kage init`
never duplicates the library entry, keeps any other keys you added
(`runtime`, `diagnostics`, ...), and if your `.luarc.json` is not
valid JSON it is left untouched and the step is reported as skipped
rather than overwriting your file.

Rerun `kage init` after upgrading kage to refresh the stub; it is a
generated artifact, so it is always overwritten in place (do not
hand-edit `~/.local/share/kage/types/kage.lua`).

## manual setup

If you skipped `kage init`, or keep plugins outside the default
directory, add the library path yourself. The stub lives in the kage
repo at `plugins/types/kage.lua`; copy it anywhere stable, or
reference the repo path directly.

`.luarc.json` next to your plugins:

```json
{ "workspace": { "library": ["/abs/path/to/kage/types"] } }
```

Per-editor notes follow. All of them assume `lua-language-server`
(the de-facto Lua LSP) is installed and that a `.luarc.json` like the
above sits in the plugin directory.

### Neovim

With `nvim-lspconfig`:

```lua
require("lspconfig").lua_ls.setup({})
```

`lua-language-server` discovers `.luarc.json` automatically when you
open a file in that directory. Nothing else is needed; the
`workspace.library` entry is honored from the project file.

### VS Code

Install the **sumneko.lua** extension. It reads the same
`.luarc.json`. Open the plugins folder as a workspace (or trust the
folder) so the project file is picked up.

### Helix

Helix uses `lua-language-server` out of the box for Lua. Ensure the
binary is on `PATH`; the `.luarc.json` in the plugin directory is
read automatically. No `languages.toml` change is required for the
library path.

### Zed

Zed's Lua extension wraps `lua-language-server`. Install the Lua
extension; it honors `.luarc.json` in the worktree root. Open the
plugin directory as the project so the file is found.

## verifying it works

Open `~/.config/kage/plugins/anything.lua` and type:

```lua
kage.
```

You should see the full API listed. Then:

```lua
kage.register_tool({ })
```

Put the cursor inside the braces and trigger completion: `name`,
`description`, `schema`, `risk`, `execute` are offered with their
types. Hover any `kage.*` function for its doc text. If none of this
appears, confirm `lua-language-server` is running
(`:LspInfo` in Neovim, the output panel in VS Code) and that
`.luarc.json` resolves the library path to an existing directory
containing `kage.lua`.
