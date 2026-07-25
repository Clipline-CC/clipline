const WindowLifecycleCore = globalThis.WindowLifecycleCore;
if (!WindowLifecycleCore) {
  throw new Error(
    "window-lifecycle-core.js must load before window-lifecycle-core.mjs",
  );
}

export { WindowLifecycleCore };
