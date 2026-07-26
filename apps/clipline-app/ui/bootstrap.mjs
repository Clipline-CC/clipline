import { PresentationCore } from "./presentation.mjs";
import { PlayerCore } from "./player-core.mjs";
import { CloudCore } from "./cloud-core.mjs";
import { GalleryWindowCore } from "./gallery-window-core.mjs";
import { WindowLifecycleCore } from "./window-lifecycle-core.mjs";

// Small explicit module surface for diagnostics and gradual controller
// migration. Legacy DOM scripts remain compatibility adapters during the
// incremental conversion, but startup no longer relies on main.js script order.
globalThis.CliplineModules = Object.freeze({
  PresentationCore,
  PlayerCore,
  CloudCore,
  GalleryWindowCore,
  WindowLifecycleCore,
});

await import("./main.js");
