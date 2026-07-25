const GalleryWindowCore = globalThis.GalleryWindowCore;
if (!GalleryWindowCore) {
  throw new Error("gallery-window-core.js must load before gallery-window-core.mjs");
}
export { GalleryWindowCore };
