# Omarchy palette snapshot

Uniterm bundles 178 Omarchy palettes alongside its 12 original presets.
Select them through the existing Settings theme control, or set `theme = NAME` in `~/.config/uniterm/uniterm.conf`.
For example, `theme = omarchy-local-minimal` selects the user's Minimal palette.
The existing default and unprefixed names retain their previous meanings.

The 2026-10-01 snapshot includes all 22 themes from [Omarchy](https://github.com/omacom/omarchy) at revision `f45461a38f2b0a12ba39837eb41b08c012051780`, 145 of the 146 entries in the [community gallery](https://omarchy.org/themes/), and 11 populated local theme directories.
Gruvu and the local Aether theme are intentionally excluded at the user's request.
The snapshot retains the original discovery records (Gruvu returned HTTP 404 and the local `aether` directory was empty) for provenance; neither is pending implementation.
All 22 installed bundled palettes matched the upstream snapshot, so they are not duplicated.

Names use `omarchy-` for bundled themes, `omarchy-community-` for gallery themes, and `omarchy-local-` for installed additions.
Local variants remain separate even when also represented in the gallery, so local changes are preserved.
The complete selectable-name and attribution list is in [CATALOG.md](CATALOG.md).

## Local Minimal and active theme

The user's `~/Work/omarchy-minimal-theme/colors.toml` is bundled as `omarchy-local-minimal`.
Minimal is the user's Felix-derived theme, based on [Felix by TyRichards](https://github.com/TyRichards/omarchy-felix-theme).
Its black background, near-white foreground, light-grey accent and monochrome semantic colors are preserved.
Its source matches the installed Minimal palette and the active palette at the time of capture.
This machine records the active name in `~/.local/state/omarchy/current/theme.name`, which read `minimal`; the legacy `~/.config/omarchy/current/theme` path was absent.
Desktop files were only read, and selecting a Uniterm theme does not switch the desktop theme.
Uniterm does not automatically track future local edits or desktop theme changes.

## Mapping and runtime cost

The importer prefers `colors.toml`, with `alacritty.toml` as the legacy fallback used in this snapshot.
Background and foreground map directly; accent uses the explicit accent, then blue/color4.
Success, warning and error use green/color2, yellow/color3 and red/color1.
Surface uses `lighter_background` when supplied; otherwise it blends 8 percent foreground into background using integer RGB arithmetic.
Muted uses `muted`, then color8, then foreground; Alacritty uses bright black.
A surface equal to foreground is replaced with the same blend, and an accent equal to surface uses foreground so controls remain visible.
Uniterm derives its remaining semantic roles with the existing `Theme::semantic` logic, including its secondary accent blend.
These are adaptations for Uniterm chrome, not wallpaper, font, GTK, editor, desktop, or child-terminal theme imports.

Only static names and eight RGB values per palette are compiled into Uniterm.
Parsing upstream files, fetching sources and generating code happen offline during maintenance, never during startup, rendering or idle time.
Existing Settings snapshots enumerate `ThemePreset::ALL`, so the same catalog reaches the Settings control without client changes.

## Reproduction and attribution

`sources.json` contains the gallery URL and HTML checksum, every discovered theme name, repository, pinned revision where available, source path, exact palette text and SHA-256, explicit fetch failures, and available upstream license notices.
Minimal has no Git revision; its exact local source and hash provide the reproducible snapshot.
Upstream notices are retained verbatim when found; a missing notice is recorded as null and does not imply an upstream license grant.
The palette text is data only; no upstream installers, hooks, scripts or application configs are executed.

Generate or verify the Rust table with Python 3.11+ and rustfmt:

```sh
python themes/omarchy/generate.py
python themes/omarchy/generate.py --check
python -m unittest discover -s themes/omarchy -p 'test_*.py'
cargo test -p uniterm-core config::tests --lib
```

Generation is deterministic and network-free, validates source hashes, and rejects duplicate names or malformed RGB values.
`python themes/omarchy/generate.py --check --verify-upstream` optionally checks captured sources against their pinned public revisions over HTTPS.
A local modification relative to its recorded repository revision is intentionally reported as a mismatch by this optional check; the captured text remains authoritative for reproduction.
To refresh the catalog, review new gallery entries and upstream revisions, replace the corresponding captured data and hashes, retain attribution and notices, regenerate, and update the coverage tests and this document.
Failures remain explicit in the snapshot rather than silently reducing claimed coverage.
