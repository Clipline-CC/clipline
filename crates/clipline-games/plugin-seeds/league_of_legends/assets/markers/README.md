# Timeline marker icons

`kill.png`, `death.png`, `assist.png`, `dragon.png`, `baron.png`, and
`turret.png` are first-party silhouette icons created or provided for
Clipline's review-timeline markers. They were not extracted, traced, or copied
from any game's assets, and no third-party copyright is claimed over them.

As first-party project assets they are covered by Clipline's own license
(**MIT OR Apache-2.0**); they are not third-party material, so they are not
listed in `THIRD-PARTY-NOTICES.md`.

They are loaded through the plugin manifest and rendered in League review
timeline surfaces. Right-side event rail icons live separately in
`assets/event-rail/`.

Keep each marker PNG on a 320x320 transparent canvas with a 280px-tall visible
alpha box, centered vertically. The timeline relies on plugin marker assets
having the same alpha bounds as the fallback UI marker assets.
