# Campaign provenance and reproduction

Harness commit: `76026d6233515d1e568ba60f5837bbb529e626cb`.
Harness executable SHA-256: `84403f6d03d1cfb22d7523c2ca0a385659f887a64f01e06acd60b3d2c6061f7f`.
Configuration SHA-256: `824a1318cbbeb93b91bd8f58141107079dd034dbe603ae4e548ab4b1184273bb`.
Compiler: `rustc 1.97.1 (8bab26f4f 2026-07-14)`.

| Product | Source commit | Artifact SHA-256 |
| --- | --- | --- |
| Uniterm 1.0.4 | `a961519c5ed965c91a9f354554bfdcaba154810a` | `c3c62e53cdd16be1f68dfff518cf4b33c2cf5bc6691ccd2b00344455aceb7b85` |
| Herdr 0.8.2 | `9eb521456ac0d19d3ab3d9d7cea3cca10baa8a4c` | `85122e080e0a9d40e8e618bad2b5b491f6b58f1be1dfbb66b2898b74bf2464f3` |
| tmux 3.7c | `e476c1230b958df0cb12977517d24b3dc931375b` | `365e1283883c342f6bbd3272d62a496f37fcc51680f0c845386873910152d517` |

All binaries were built from clean official release-tag clones, then verified against the passing smoke and standard runs.
Use comparison.toml plus tmux.contender.toml, with local source and release artifact paths.
The marketing profile uses a 160x50 PTY, POSIX /bin/sh, 300-second idle windows, 20 startup trials, 50 control trials, 100 latency trials, ten 50,000-line output bursts, 16 panes, 40 resizes, and two extra clients.
Herdr network checks are disabled during timing and its detached grid matches the profile geometry.
Every trial uses private HOME/XDG directories and sockets, and latency/output measurements are gated by the screen oracle.

```sh
ut-compare run --config comparison.local.toml --profile marketing --seed 0 --output marketing-1.json --markdown marketing-1.md
ut-compare run --config comparison.local.toml --profile marketing --seed 1 --output marketing-2.json --markdown marketing-2.md
ut-compare run --config comparison.local.toml --profile marketing --seed 2 --output marketing-3.json --markdown marketing-3.md
ut-compare report --output report.md marketing-1.json marketing-2.json marketing-3.json
```

The generated report retains trials separately. SUMMARY.md and statistics.json summarize run medians without pooling samples.
Original schema-7 JSON remains untouched and can reproduce the generated report.
campaign-context.local.json records local operating conditions and is not intended for website publication.
Existing desktop applications remained running. No builds were launched by the campaign during measurements.
Power settings were unchanged and suspend was inhibited during collection.

Build commands:

- Uniterm: cargo build --release --locked --offline -p uniterm-cli --bin ut
- Herdr: cargo build --release --locked --offline --bin herdr; Zig 0.15.2; default release settings
- tmux: sh autogen.sh; ./configure with local prefix; make -j4; default -O2

Public exports replace machine-local benchmark paths with `/path/to/uniterm-benchmark`; SHA256SUMS describes this sanitized copy.
