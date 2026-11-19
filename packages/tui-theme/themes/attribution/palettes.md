# Bundled palette attribution

Bcode's bundled palettes take their colors from the color schemes below.
Palettes converted to their upstream scheme (`status = "upstream"` in
`sources.toml`) declare that scheme's exact colors, and `upstream/<id>.toml`
records the source repository, pinned revision, license, and values; roles the
scheme does not define are derived by Bcode. Palettes still `pending` record a
small set of adapted seed colors. See `UPSTREAM.md` for the conversion
procedure. Original Bcode palettes (`neon-noir`, `phosphor`, `amber`,
`paper`, `blueprint`, `aurora`, `ember`, `glacier`, `moss`, `dusk`) are authored
for Bcode; `claude-code` is measured from Claude Code's renderer.

| Palette ids | Source scheme | Upstream | License |
|---|---|---|---|
| `catppuccin-*` | Catppuccin (frappe, latte, macchiato, mocha) | github.com/catppuccin/nvim | MIT |
| `tokyonight*` | Tokyo Night (night, day, moon, storm) | github.com/folke/tokyonight.nvim | Apache-2.0 |
| `gruvbox` | Gruvbox (dark, light; medium contrast) | github.com/ellisonleao/gruvbox.nvim | MIT |
| `gruvbox-material` | Gruvbox Material (medium background, material foreground) | github.com/sainnhe/gruvbox-material | MIT |
| `dracula` | Dracula / Alucard | github.com/dracula/vim | MIT |
| `rose-pine*` | Rosé Pine (main, moon, dawn) | github.com/rose-pine/neovim | MIT |
| `kanagawa*` | Kanagawa (wave, lotus, dragon) | github.com/rebelot/kanagawa.nvim | MIT |
| `everforest` | Everforest (medium background) | github.com/sainnhe/everforest | MIT |
| `one` | One Dark (dark, light) | github.com/navarasu/onedark.nvim | MIT |
| `solarized` | Solarized | github.com/altercation/solarized | MIT |
| `github*` | GitHub (dark default, light default, dark dimmed) | github.com/projekt0n/github-nvim-theme | MIT |
| `ayu*` | Ayu (dark, mirage, light) | github.com/Shatur/neovim-ayu (palette: github.com/ayu-theme/ayu-colors, MIT) | GPL-3.0 (review required) |
| `nightfox` | Nightfox / Dawnfox | github.com/EdenEast/nightfox.nvim | MIT |
| `oxocarbon` | Oxocarbon | github.com/nyoom-engineering/oxocarbon.nvim | MIT |
| `modus` | Modus Vivendi / Operandi | github.com/protesilaos/modus-themes | GPL-3.0 (review required) |
| `flexoki` | Flexoki | github.com/kepano/flexoki | MIT |
| `poimandres` | Poimandres | github.com/olivercederborg/poimandres.nvim (port of github.com/drcmda/poimandres-theme, MIT) | None declared (review required) |
| `vesper` | Vesper | github.com/raunofreiberg/vesper | MIT |
| `melange` | Melange | github.com/savq/melange-nvim | MIT |
| `zenburn` | Zenburn | github.com/jnurmine/Zenburn | GNU GPL, version unstated (GPL-1.0-or-later; review required) |
| `monokai-classic` | Monokai | monokai.pro (classic palette) | Review required |
| `night-owl` | Night Owl | github.com/oxfist/night-owl.nvim | BSD-3-Clause |

Entries marked "review required" must be confirmed (or replaced with original
palettes) before release. Copyright notices for MIT and Apache-2.0 sources are
reproduced from their upstream LICENSE files in release packaging.
