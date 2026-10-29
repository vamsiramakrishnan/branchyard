# Branchyard assets

The mark shows one track splitting into parallel work and returning to a shared point. Keep its shape and proportions intact.

Use [icon.svg](icon.svg) for the app, favicon and README. It has a green background and works on light or dark surfaces. The README uses it at 56 × 56 pixels; 16 pixels is the minimum size. Leave space around it equal to one quarter of its width.

Use [icon-mono.svg](icon-mono.svg) where only one color is available. It uses `currentColor` when inlined; an image element renders it in the SVG's default black. Choose a contrasting color and keep the center open.

[wordmark.svg](wordmark.svg) pairs the icon with dark lettering for light surfaces. [wordmark-light.svg](wordmark-light.svg) uses light lettering for dark surfaces. The letters are paths, so neither version needs a font installed.

[social-card.svg](social-card.svg) is the editable 1200 × 630 sharing image. [social-card.png](social-card.png) is its raster export. Edit the SVG source and render it with an established SVG tool such as CairoSVG or Inkscape.

| Color | Value | Use |
| --- | --- | --- |
| Green | `#1f6f4a` | Icon background and primary actions |
| Ink | `#172b23` | Text and dark surfaces |
| Off-white | `#f5f3ed` | Light surfaces and reversed lettering |
| Muted ink | `#45584c` | Supporting text on light surfaces |
| Border | `#d8dfd7` | Dividers on light surfaces |
| White | `#ffffff` | Mark on green |

The companion icon at `crates/branchyard-server/src/companion/assets/icon.svg` is an exact copy of the primary icon. Update both when changing the mark.

The mark is original Branchyard artwork under the repository's Apache-2.0 license. Lettering in the wordmarks and sharing card uses outlined DejaVu Sans; its [font notice](LICENSE-DejaVu.txt) is included. No asset loads scripts, fonts or external resources.
