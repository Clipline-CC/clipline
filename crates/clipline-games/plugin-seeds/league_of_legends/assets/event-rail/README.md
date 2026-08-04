# Event rail icons

`kill.png` and `death.png` are first-party generated Clipline silhouettes used
only in the right-side League event rail.

`dragon.png`, `baron.png`, `turret.png`, `minion-100.png`, and
`minion-200.png` are League client match-history assets mirrored by
CommunityDragon for the right-side League event rail. Riot Games owns the
underlying League of Legends artwork and trademarks; Clipline does not claim
third-party rights over these images.

Timeline marker icons live separately in `assets/markers/`.

Keep the objective-kind event rail PNGs on a 320x320 transparent canvas with a
280px-tall visible alpha box, centered vertically. The match events sidebar
relies on matching alpha bounds instead of per-icon CSS sizing overrides.
