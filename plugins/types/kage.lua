---@meta
---
--- Type definitions for the `kage` plugin API.
---
--- This file is a `lua-language-server` definition stub: it declares
--- the shape of the `kage` global so editors give hover docs,
--- completion, and argument checking inside `*.lua` plugins. It ships
--- with the kage repo and is copied to
--- `~/.local/share/kage/types/kage.lua` by `kage init`, which also
--- writes a `.luarc.json` pointing the language server at it.
---
--- Nothing here runs: `---@meta` marks the whole file definitions-only.
--- The single source of truth for behavior is the Rust implementation
--- in `crates/kage-plugin`; entry points are annotated in the
--- companion sections (PD.1.2 / PD.1.3).

---@class kage
local kage = {}

return kage
