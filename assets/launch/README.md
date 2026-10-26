# Rowboat launch assets

## Theme screenshots

Open [`themes/index.html`](themes/index.html) for the browsable gallery, or
[`themes/contact-sheet.png`](themes/contact-sheet.png) for the overview.

- **22 native PNG screenshots**, one for each bundled palette, at **3840 × 2400**.
  The app renders at **2× pixel density** with the same 1920 × 1200 logical layout;
  these replace the original 1× captures and are not upscaled copies.
- Every image shows the full application, the same readable SQL query, the same
  **100 actual PostgreSQL result rows** (first ten visible), and a 22-table explorer.
- Fictional business: **Northstar Supply**, a home-goods retailer and wholesale
  supplier. The persistent demo has 240 orders from 30 fictional customers; the
  other 21 tables are intentionally empty. All data is invented.
- The customer column is resized through the UI so full names are readable.
  Images are direct Wayland captures: no fabricated grid contents or compositing.
- Theme differences, execution timing and the last-run clock are actual UI output.
  System is a theme-following setting, not a 23rd distinct bundled palette.
- `themes/manifest.json` records each palette, dimensions, successful result count,
  and the screenshot/query/executable SHA-256 values.

Use the individual PNGs for social posts and GitHub announcements. The contact
sheet is a selection aid; it is not the full-resolution source. Regenerating the
gallery writes a ZIP bundle beneath ignored `.local/dist/launch/`, containing
this guide, the gallery, manifest and screenshots. Generated bundles are not
committed alongside their source assets.
Gallery previews are reduced to fit the page; click one to open the full-resolution
image. The larger originals preserve text detail on high-DPI displays.

## Reproduce

From the Rowboat checkout on Linux/Hyprland:

```bash
bash scripts/launch-demo.sh up
cargo build --locked --release -p rowboat -p rowboat-cli
cargo build --locked -p rowboat-tests --bin rowboat-test-click
mise -E capture exec -- python scripts/capture-launch.py all
mise -E capture exec -- python scripts/capture-launch.py stop
```

Capture requirements: the Python runtime in `mise.capture.toml`, Hyprland, `hyprctl`, `grim`, `dbus-run-session`,
`dbus-daemon`, ImageMagick (`magick`), and Fontconfig (`fc-match`). No installation or
desktop configuration changes are performed by the script.

The script creates an isolated, task-owned compositor and per-theme Rowboat data
under `.local/launch/capture/`. It runs the read-only showcase query against only
the demo at `127.0.0.1:15436`, waits for a successful durable 100-row history entry,
then captures and normally closes each app. The user's open app, saved profiles
and preferences are separate. The compositor is stopped explicitly after review;
the PostgreSQL demo is intentionally left running with persistent Podman storage.

Capture just one palette or regenerate the gallery:

```bash
mise -E capture exec -- python scripts/capture-launch.py display
mise -E capture exec -- python scripts/capture-launch.py capture --theme tokyo-night
mise -E capture exec -- python scripts/capture-launch.py gallery
mise -E capture exec -- python scripts/capture-launch.py stop
```

Query: [`demo/launch/orders.sql`](../../demo/launch/orders.sql).
Database connection, schema and lifecycle: [`demo/launch/README.md`](../../demo/launch/README.md).
Stop the database when finished with `bash scripts/launch-demo.sh stop`;
`up` resumes it without losing demo edits.
