// Pure region-selection math for the screenshot overlay. Keep DOM- and
// Tauri-free so the Boa tests in tests/region_core.rs run it unchanged.
var RegionCore = (() => {
  const MIN_DRAG = 3;

  const clamp = (value, lo, hi) => Math.min(Math.max(value, lo), hi);

  const dragResult = (start, end, frame) => {
    const x1 = clamp(start.x, frame.x, frame.x + frame.width);
    const y1 = clamp(start.y, frame.y, frame.y + frame.height);
    const x2 = clamp(end.x, frame.x, frame.x + frame.width);
    const y2 = clamp(end.y, frame.y, frame.y + frame.height);
    const x = Math.min(x1, x2);
    const y = Math.min(y1, y2);
    const width = Math.abs(x2 - x1);
    const height = Math.abs(y2 - y1);
    if (width < MIN_DRAG && height < MIN_DRAG) return null;
    return { x, y, width, height };
  };

  const escapeCancels = (key) => key === "Escape";

  const readout = (rect) => rect.width + " x " + rect.height;
  
  const snapRect = (point, candidates, frame) => {
    for (let i = candidates.length - 1; i >= 0; i--) {
      const c = candidates[i];
      if (
        point.x >= c.x && point.x < c.x + c.width &&
        point.y >= c.y && point.y < c.y + c.height
      ) {
        const x = clamp(c.x, frame.x, frame.x + frame.width);
        const y = clamp(c.y, frame.y, frame.y + frame.height);
        const right = clamp(c.x + c.width, frame.x, frame.x + frame.width);
        const bottom = clamp(c.y + c.height, frame.y, frame.y + frame.height);
        return { x, y, width: right - x, height: bottom - y };
      }
    }
    return null;
  };

  return Object.freeze({ dragResult, escapeCancels, readout, snapRect });
})();

globalThis.RegionCore = RegionCore;
