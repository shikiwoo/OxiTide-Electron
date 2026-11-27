## What's new in 1.2.0

### Catppuccin themes

Settings → Theme has a new **Catppuccin** choice, with a **Flavor** row:

- **Latte** (light), **Frappé**, **Macchiato** and **Mocha** (dark), with Mauve as the accent.
- **Mocha OLED**: Mocha's text and accent colours on true-black backgrounds. The page, sidebar, queue and player bar are all black.

The themes work in Atmosphere and Studio; Hi-Fi Rack keeps its own finish. In Latte, the muted text, green and yellow are slightly darker than the official palette so that text stays readable.

### Linux

- Tauri shell on NVIDIA: sets `__NV_DISABLE_EXPLICIT_SYNC=1` at startup to fix slow scrolling on Wayland with high-refresh monitors. It is set only when the NVIDIA driver is present and you haven't set the variable yourself (`=0` opts out). This is experimental, and the Electron AppImage is not affected.
