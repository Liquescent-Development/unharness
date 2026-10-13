# `unharness update`: status notes

`unharness update` (`src/update.rs`) uses axoupdater 0.10 on the
receipt dist 0.32.0's shell installer writes to
`$XDG_CONFIG_HOME/unharness/unharness-receipt.json` (or
`~/.config/unharness/`); it writes one also with `install-updater =
false` unless `UNHARNESS_DISABLE_UPDATE=1`. Its `install_prefix` is the
root above `bin` (`install_layout: "cargo-home"`), which is what
axoupdater compares the running binary against, so a receipt left by
another copy is not used. Checked on Linux with a scratch prefix and
`XDG_CONFIG_HOME`: 0.2.9 → 0.3.0 and a 0.3.0 → 0.2.0 downgrade through
the release's installer, receipt rewritten, no rc file touched. A
binary under a `Cellar` directory is Homebrew's; one listed in
`.crates2.json` beside its `bin` is cargo's, unless the receipt is
newer. Both are told what to run. It runs before the config is read
and never on its own. Unverified: macOS, a real Homebrew install.
