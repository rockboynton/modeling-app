# kcl-lsp

The `kcl` [Language Server Protocol](https://microsoft.github.io/language-server-protocol)
implementation and VSCode extension.

The Rust language-server implementation, its executable, tests, benchmarks,
and VS Code extension live in this crate. It depends on the KCL compiler and
language tooling published as [kcl-lib](https://crates.io/crates/kcl-lib).

## VSCode

Install our extension: [KittyCAD Language Server](https://marketplace.visualstudio.com/items?itemName=KittyCAD.kcl-language-server)

## Neovim

You can add the following to your `vim` configuration if you are using `lspconfig`.

This is [@jessfraz's
setup](https://github.com/jessfraz/.vim/blob/master/vimrc#L935).

```vim
if executable('kcl-language-server')
lua << EOF
local lspconfig = require 'lspconfig'
local configs = require 'lspconfig.configs'

if not configs.kcl_lsp then
  configs.kcl_lsp = {
    default_config = {
      cmd = {'kcl-language-server', 'server', '--stdio'},
      filetypes = {'kcl'},
      root_dir = lspconfig.util.root_pattern('.git'),
      single_file_support = true,
    },
    docs = {
      description = [=[
https://github.com/KittyCAD/kcl-lsp
https://kittycad.io

The KittyCAD Language Server Protocol implementation for the KCL language.

To better detect kcl files, the following can be added:


    vim.cmd [[ autocmd BufRead,BufNewFile *.kcl set filetype=kcl ]]

]=],
      default_config = {
        root_dir = [[root_pattern(".git")]],
      },
    }
  }
end

lspconfig.kcl_lsp.setup{}
EOF
else
  echo "You might want to install kcl-language-server: https://github.com/KittyCAD/kcl-lsp/releases"
end
```

## Helix

Add this to your `languages.toml` file. Remember to change `/Users/adamchalmers` to your path.

Note that we don't currently have Treesitter parsers, so there won't be syntax highlighting.

```toml
[[language]]
name = "kcl"
scope = "source.kcl"
injection-regex = "kcl"
file-types = ["kcl"]
comment-tokens = "//"
indent = { tab-width = 2, unit = "  " }
language-servers = [ "kcl-lsp" ]
block-comment-tokens = { start = "/*", end = "*/"}


[language-server.kcl-lsp]
command = "/Users/adamchalmers/kc-repos/kcl-lsp/target/release/kcl-language-server"
args = ["server", "--stdio"]
```

## Editing features

The server provides go-to-definition and references for lexical KCL bindings,
including variables, functions, parameters, named imports and aliases, module
members, and unambiguous glob imports. Navigation uses the AST rather than
execution results, respects lexical scopes and export visibility, and prefers
open editor buffers over disk contents. Native reference searches include KCL
files in the project (nearest `project.toml`, or the containing workspace folder)
and imported modules. Built-in functions continue to provide hover documentation;
object fields and built-in definitions are not navigable source bindings.

Rename updates lexical bindings and their project references with targeted edits,
preserving formatting, comments, and explicit import aliases. Renaming an alias
updates its local binding; renaming an exported name updates import selectors
without changing explicit aliases. Renames that capture another binding or collide
with an existing name, including ambiguous parameter/type names, are rejected.
Parameter renames update keyword labels on
statically resolved calls; dynamic function-value flows are not resolved.
Workspace rename requires readable, parseable
KCL files. Implicit module names require an explicit import alias before renaming.

Completion uses the same lexical scopes and import resolution. It offers visible
parameters and local bindings, follows public module members and reexports,
and reads unsaved imports. Member completion supports incomplete `module.` and
`module::` expressions and replaces the full identifier at the cursor. Object
field inference and automatic imports are not currently provided.

The standalone native server checks each document with fresh mock execution,
using its actual file path and nearest optional `project.toml`. Imports resolve
from the entry point's directory and open imported buffers are read before disk.
Changes and closes refresh other open documents in that project; saving reloads
project settings. These diagnostics require no API token and do not establish
engine geometry correctness.

## Development

```bash
$ npm install
$ cargo build
$ code .
```

Once VSCode opens, go to the "Run and Debug" panel (cmd-shift-D on MacOS), and choose Run Extension (Debug Build).
This opens a new VSCode window with our KCL extension installed. Open a KCL file and check that the LSP is working.

- press <kbd>F5</kbd> or change to the Debug panel and click <kbd>Launch Client</kbd>

> **Note**
>
> If encountered errors like `Cannot find module '/xxx/xxx/dist/extension.js'`
> please try run command `tsc -b` manually
