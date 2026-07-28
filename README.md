# dprintx

A wrapper around [dprint](https://dprint.dev/) that adds multi-config support and several missing features.

## Features

- **[Per-file config profiles](#how-it-works)** — select dprint config by file path using glob rules
  ([dprint#996](https://github.com/dprint/dprint/issues/996))
- **[Content-based matching](#content-based-matching)** — override profile by file content (e.g. skip generated files)
- **[Local config overrides](#local-config-overrides)** — project-level `dprint.json` that merges with the matched
  profile via `extends`
- **[Unified diff output](#diff_pager)** — `dprint check` with real unified diff and optional pager
  ([dprint#1092](https://github.com/dprint/dprint/issues/1092))
- **[LSP proxy](#lsp-proxy)** — spawns per-profile `dprint lsp` backends, routes requests by file URI
- **[LSP gitignore handling](#lsp-gitignore-handling)** — format gitignored files in the editor, where opening a file is
  already an explicit request ([dprint#1124](https://github.com/dprint/dprint/issues/1124))
- **[LSP URI rewriting](#lsp-uri-rewriting)** — format extensionless files (shell scripts, etc.) by appending the
  correct extension based on editor's `languageId`
- **[Directory arguments](#directory-arguments)** — pass directories to `fmt`/`check`, scoped via dprint's file
  discovery
- **[Transparent drop-in](#transparent-dprint-replacement)** — symlink as `dprint`, all unknown commands passthrough to
  the real binary

## How it works

Config file: `~/.config/dprint/dprintx.jsonc`

```jsonc
{
  "dprint": "~/.cargo/bin/dprint",

  "diff_pager": "delta -s",
  "lsp_rewrite_uris": true,
  "lsp_no_gitignore": true, // default
  "lsp_timeout_ms": 30000, // default

  "profiles": {
    "maintainer": "~/.config/dprint/dprint-maintainer.jsonc",
    "default": "~/.config/dprint/dprint-default.jsonc",
    "ignore": null, // null = skip file entirely
  },

  "match": {
    "**/noc/cmdb/**": "maintainer",
    "**/noc/invapi/**": "maintainer",
    "**": "default",
  },

  "match_content": {
    "^// Code generated .+ DO NOT EDIT": "ignore",
  },
}
```

Rules in `match` are evaluated top-to-bottom, first match wins. Files not matching any rule are skipped. Use
`"**": "profile"` as a catch-all. Profiles set to `null` cause the file to be skipped (passed through unchanged).

All paths in the config (`dprint`, profile paths) support `~` expansion and relative paths. Relative paths are resolved
against the directory containing `dprintx.jsonc`:

```jsonc
{
  "dprint": "bin/dprint", // → ~/.config/dprint/bin/dprint
  "profiles": {
    "main": "profiles/main.jsonc", // → ~/.config/dprint/profiles/main.jsonc
  },
}
```

### Content-based matching

`match_content` lets you override the path-matched profile based on file content. This is useful for skipping generated
files that live alongside hand-written code.

```jsonc
{
  "match_content": {
    "^// Code generated .+ DO NOT EDIT": "ignore",
    "^# Code generated .+ DO NOT EDIT": "ignore",
  },
}
```

**How it works:**

1. File is first matched by path (`match` rules) — if no path match, the file is skipped entirely
2. If `match_content` is configured, the file is scanned in line-aligned blocks (~8KB each)
3. Each regex pattern is tested against each block, first match wins (stops scanning early)
4. The matched profile overrides the path-based result

Patterns are regular expressions (Rust `regex` syntax, multi-line mode: `^` matches start of any line).

### diff_pager

When `diff_pager` is set, `dprint check` produces unified diff output instead of dprint's default format:

- **stdout is TTY** → pipes through the pager (e.g. `delta -s`)
- **stdout is pipe/redirect** → raw unified diff

```bash
dprintx check              # pretty diff via delta
dprintx check > fix.patch  # unified diff to file
```

Without `diff_pager`, `dprint check` behaves exactly like the original dprint.

### Local config overrides

Projects can define local formatting rules that override the matched profile.

**How it works:**

1. For each file being formatted, dprintx walks up the directory tree looking for `dprint.json` or `dprint.jsonc` (stops
   at the first one found)
2. If found, it reads the local config and injects the matched profile path into `extends`
3. A temporary merged config is written and passed to dprint instead of the profile config
4. The temp file is auto-deleted when the command finishes (RAII guard)

Since dprint applies `extends` first and then overlays local settings on top, the local config takes precedence.

**Example:**

```jsonc
// ~/projects/my-app/dprint.json — only the overrides you care about
{
  "yaml": {
    "commentSpacing": "ignore",
  },
}
```

When formatting files under `~/projects/my-app/`, dprintx generates a temporary config equivalent to:

```jsonc
{
  "extends": "/home/user/.config/dprint/dprint-default.jsonc",
  "yaml": {
    "commentSpacing": "ignore",
  },
}
```

**`extends` handling:**

| Local config `extends`       | Result                                          |
| ---------------------------- | ----------------------------------------------- |
| absent                       | set to profile path                             |
| `"https://example.com/base"` | `["/profile/path", "https://example.com/base"]` |
| `["a.json", "b.json"]`       | `["/profile/path", "a.json", "b.json"]`         |

The profile path is always prepended so that local settings win.

**Temp file location:** `$XDG_RUNTIME_DIR/dprintx/` (per-user, mode 700). Falls back to `$TMPDIR/dprintx/` if
`XDG_RUNTIME_DIR` is unavailable. Files are named `merged-{pid}-{seq}.json` and cleaned up automatically. Files left
behind by a killed process are removed on the next run, matched by their pid.

If no local config is found, the profile config is used directly — no temp file is created.

### Directory arguments

dprint doesn't support directories as arguments (`dprint check src/` gives "Is a directory" error). dprintx handles
directory arguments by using dprint's own file discovery (`output-file-paths`) filtered to the specified directories:

```bash
dprintx fmt src/                    # format all matched files under src/
dprintx check pkg/internal/drafts   # check all matched files under the directory
dprintx fmt a.go src/ b.rs          # mix of files and directories works too
```

Files are passed through as-is. Directories use the same pipeline as `dprintx fmt`/`dprintx check` without arguments —
dprint discovers files via its own includes/excludes, then dprintx filters by profile match rules. This naturally skips
binary files, build artifacts, and anything dprint wouldn't process on its own.

### LSP proxy

`dprintx lsp` speaks LSP to the editor and runs one `dprint lsp` backend per effective config, spawned on first use and
keyed by config path. Requests are routed by file URI, so one editor session can span projects with different profiles.
[Merged configs](#local-config-overrides) are built once per project directory and reused, which keeps that count at one
backend per project rather than one per file.

Each backend is told which directory it serves: the project root for a local config, otherwise the workspace folder the
editor reported. That directory anchors the profile's `includes`/`excludes` globs, which is what lets a config stored in
`~/.config/dprint/` format files anywhere on disk.

### LSP gitignore handling

> **Enabled by default.** Disable with `"lsp_no_gitignore": false`.

dprint skips gitignored files. That is right for `dprint fmt *`, where a shell glob may sweep in `node_modules`, but
wrong in an editor: opening a file is already an explicit request to format it.

When enabled, the proxy passes `--no-gitignore` to each backend, so gitignored files format normally. Config `excludes`
still apply — they describe what dprint owns, which is a separate question from what git tracks.

```jsonc
{
  "lsp_no_gitignore": false, // let backends respect .gitignore
}
```

This needs a dprint that accepts `--no-gitignore` on `lsp`, which upstream does not have yet. The proxy checks
`lsp --help` once per run and leaves the flag off when the backend doesn't support it, so stock dprint keeps working —
the option simply has no effect.

### LSP request timeout

Backends compile wasm plugins on their first request, which takes seconds on a cold cache. The proxy waits 30s for a
response before giving up; lower it if you prefer a fast failure:

```jsonc
{
  "lsp_timeout_ms": 5000, // default: 30000
}
```

### LSP URI rewriting (opt-in)

> **Disabled by default** for compatibility. Enable explicitly with `"lsp_rewrite_uris": true`.

dprint matches files by extension, so extensionless files (e.g. shell scripts named `myscript`, Lua scripts without
`.lua`) are silently skipped during LSP formatting.

When `lsp_rewrite_uris` is enabled, the proxy tracks `languageId` from `textDocument/didOpen` and rewrites URIs
forwarded to the dprint backend by appending the correct extension (e.g. `file:///path/myscript` →
`file:///path/myscript.sh` for `languageId=sh`). If the file already has the correct extension, no rewrite happens.

```jsonc
{
  "lsp_rewrite_uris": true,
}
```

Default: `false` (transparent passthrough).

Supported languages:

| languageId      | Extension   |
| --------------- | ----------- |
| go              | .go         |
| lua             | .lua        |
| json            | .json       |
| jsonc           | .jsonc      |
| yaml            | .yaml       |
| markdown        | .md         |
| python          | .py         |
| rust            | .rs         |
| typescript      | .ts         |
| typescriptreact | .tsx        |
| javascript      | .js         |
| javascriptreact | .jsx        |
| sh / bash / zsh | .sh         |
| toml            | .toml       |
| css             | .css        |
| html            | .html       |
| sql             | .sql        |
| dockerfile      | .Dockerfile |
| graphql         | .graphql    |

## CLI

```bash
# stdin — single file, filename is used for config matching (input is read from stdin)
dprintx fmt --stdin path/to/file.yaml < input.yaml

# fmt/check — groups files by profile, calls dprint per group
dprintx fmt
dprintx check
dprintx fmt file1.go file2.yaml   # explicit file list
dprintx check src/                # directory → recursively expanded

# list all files that would be formatted (merged from all profiles)
dprintx output-file-paths

# show which config is used
dprintx config              # all profiles and rules
dprintx config path/to/file # resolved config for a file

# LSP proxy — spawns dprint lsp per profile, routes by file URI
dprintx lsp
```

`dprintx check` exits with code 1 if any files need formatting.

Use `--config <PATH>` to override the config location (default: `~/.config/dprint/dprintx.jsonc`):

```bash
dprintx --config /path/to/custom.jsonc fmt
```

All unknown commands and flags are passed through to the real dprint (`--help`, `-V`, `license`, `completions`, etc.).

## Install

```bash
cargo install --git https://github.com/mocksoul/dprintx
```

### Transparent dprint replacement

Symlink `dprintx` as `dprint` earlier in your `PATH` — it becomes a fully transparent drop-in replacement. All unknown
commands and flags are forwarded to the real dprint binary (configured via `"dprint"` in `dprintx.jsonc`):

```bash
ln -sf ~/.cargo/bin/dprintx ~/.local/bin/dprint
```

Now `dprint fmt`, `dprint check`, `dprint lsp` etc. all go through dprintx automatically. No changes needed in editor
configs, CI scripts, or muscle memory.
