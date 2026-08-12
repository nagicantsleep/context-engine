# Spec ADE embed fonts (self-hosted)

woff2 files used by the admin UI when the app is embedded in Spec ADE.

| Family | Files | Source | License |
|---|---|---|---|
| IBM Plex Sans | `IBMPlexSans-{latin,vietnamese}-{400,500,600,700}.woff2` (+ latin 400 italic) | [@fontsource/ibm-plex-sans](https://www.npmjs.com/package/@fontsource/ibm-plex-sans) (IBM Plex) | SIL OFL 1.1 |
| Lilex | `Lilex-latin-{400,500,600,700}.woff2` | [@fontsource/lilex](https://www.npmjs.com/package/@fontsource/lilex) (mishamyrt/Lilex) | SIL OFL 1.1 |

Served at `/assets/fonts/<filename>` via `include_bytes!` (see `src/assets/mod.rs`).
Do not rename files without updating the allow-list there and the `@font-face`
rules in `index.html`.
