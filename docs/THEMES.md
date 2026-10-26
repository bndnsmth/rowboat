# Rowboat themes

## Choosing a theme

Open **Preferences → Appearance** and choose **System (default)** or a bundled
palette from the dropdown. **Save preferences** applies the choice immediately
across all Rowboat windows and persists it for the next launch. Selecting a draft
alone does not change the app; **Cancel** leaves the saved choice intact.

- **System** follows the current valid Omarchy color palette. Rowboat checks it
  off the UI thread approximately once per second, including replaced theme
  symlinks. The legacy Omarchy current directory is used only when the modern
  current directory is absent.
- Without a valid Omarchy palette, System follows the native desktop's reported
  light/dark appearance live: Flexoki Light for light, Tokyo Night for dark.
  Exact support depends on the desktop exposing appearance notifications; Rowboat
  follows the platform's reported/default appearance when none is configured.
- An explicit bundled selection stays selected through desktop/Omarchy changes.
  Choose System again to resume following the current system colors.
- The **Currently** line identifies the resolved palette and its source.
- Preferences from earlier releases default to System. A well-formed unknown
  saved palette ID is preserved and visibly falls back to System, so opening old
  or newer settings does not discard other preferences.

This changes Rowboat only; it never writes desktop theme settings. Palette choices
are stored in Rowboat's existing private app-data preferences, not in connection
profiles. Bundled choices work without an Omarchy installation or network access.

## Palette inventory

Pass 21 bundles the 22 stock Omarchy color palettes captured on 2026-10-26 from
`/usr/share/omarchy/themes`. Local custom themes are excluded by user request.
`crates/rowboat/assets/themes/*.toml` contains exact copies of the color files,
13,404 bytes total. This snapshot is application data; no wallpaper, font, desktop
configuration or executable theme hook is bundled or applied. Additional keys
unrelated to Rowboat rendering are ignored by the palette parser.

| Bundled ID | Source |
| --- | --- |
| `catppuccin` | Omarchy |
| `catppuccin-latte` | Omarchy |
| `ethereal` | Omarchy |
| `everforest` | Omarchy |
| `flexoki-light` | Omarchy |
| `gruvbox` | Omarchy |
| `hackerman` | Omarchy |
| `kanagawa` | Omarchy |
| `last-horizon` | Omarchy |
| `lumon` | Omarchy |
| `lupine` | Omarchy |
| `matte-black` | Omarchy |
| `miasma` | Omarchy |
| `nord` | Omarchy |
| `osaka-jade` | Omarchy |
| `retro-82` | Omarchy |
| `ristretto` | Omarchy |
| `rose-pine` | Omarchy (light palette) |
| `solitude` | Omarchy |
| `tokyo-night` | Omarchy |
| `vantablack` | Omarchy |
| `white` | Omarchy |

## Attribution

Omarchy palette source: <https://github.com/basecamp/omarchy>, MIT license,
Copyright (c) David Heinemeier Hansson. The original notice is retained in
`crates/rowboat/assets/themes/NOTICE.txt` and included in release packages.

## Verification

All 22 bundled palettes are embedded and parsed as TOML. Theme-selection
regressions live in the headless GUI suite; native light/dark and theme-change
acceptance is described in [TESTING.md](TESTING.md).
