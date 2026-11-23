# Phodder

A podcast player for Phosh and other GTK4/libadwaita Linux phones.

Phodder subscribes to podcasts, downloads new episodes in the background and
plays them from a queue ordered by group, so daily shows come ahead of the
rest. In the background it runs as a small service without the interface,
so it stays light on memory.

## Features

- A queue ordered by group, with episodes you've started kept in place
- Background refreshes and downloads, with filters, expiry and limits per podcast
- Delete after playing, and redownloading of missing or damaged files
- Chapters, automatic chapter skipping and transcripts
- Sleep timer, playback speed and adjustable skip intervals
- Speed and silence trimming per podcast
- Select several episodes to act on them together
- Sync with Nextcloud GPodder Sync and gpodder.net
- Podcast directories: Apple Podcasts, gpodder.net and Podcast Index, with episode previews
- Pauses for calls, follows Bluetooth audio and resumes when it reconnects
- Notifications for new episodes, per podcast
- OPML import and export, and full library backups

## Install

### Flatpak (recommended)

| <a href="https://ajdunevent.github.io/AJHub/"><picture><source media="(prefers-color-scheme: dark)" srcset="https://ajdunevent.github.io/AJHub/badges/button-dark.svg"><img src="https://ajdunevent.github.io/AJHub/badges/button-light.svg" alt="AJHub, published with FrogPak" height="48"></picture></a> | <a href="https://furihub.github.io/repo/"><picture><source media="(prefers-color-scheme: dark)" srcset="https://furihub.github.io/repo/badges/button-dark.svg"><img src="https://furihub.github.io/repo/badges/button-light.svg" alt="FuriHub, published with FrogPak" height="48"></picture></a> |
| --- | --- |
| x86_64 & aarch64 | aarch64 only |

## Download filters

- **Filter patterns use glob syntax, not plain keyword search.** A plain word with no special characters (e.g. `bonus`) matches as a case-insensitive substring anywhere in the text — `bonus` matches "Bonus Episode," "a bonus segment," etc. Beyond that: `*` matches zero or more characters (`Ju*` matches "June," "July," and bare "Ju" itself); `?` matches exactly one character; `[...]` matches one character from the listed set (`sta[lc]k` matches "stalk" or "stack," not just one of them); `[!...]` is the negated form (`sta[!l]k` matches "stack" but not "stalk"). All matching is case-insensitive throughout.
- **Filters are evaluated top to bottom, and the first matching rule wins** — not "block always wins" regardless of position. Once a rule matches, evaluation stops there; nothing below it is checked for that episode. If no rule matches at all, the result is "allowed" by default. This means the order of your rules is meaningful, not just cosmetic: `BLOCK "stalk"` → `ALLOW "Armchair Anonymous"` → `BLOCK "*"` behaves differently than the same three rules in a different order — swap the first two and a stalking-themed "Armchair Anonymous" episode would hit the Allow rule before the Block rule ever gets a chance to check it.
- **"Allow" filters carve out exceptions to "Block" rules. Without at least one overlapping "Block" rule below them, "Allow" filters have no effect. To create a strict "allowlist", a lower and widely-scoped "Block" rule such as "*" would be required.**

## Development

Developed on a FuriLabs FLX1S running FuriOS. `DESIGN.md` describes how
Phodder works and why. Issues and feature requests go to
https://dev.dunevent.net/aj/Phodder/issues.

Phodder has relied extensively on AI-assisted development, primarily
Anthropic's Claude (various models), under human direction and manual
testing.

## License

Phodder is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. See [COPYING](COPYING).
