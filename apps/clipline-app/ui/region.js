// Region overlay controller: drag to select, Esc or click cancels.
// All geometry math lives in region-core.js (Boa-tested); this file only
// translates DOM events into core calls and Tauri commands.
(function () {
  const { invoke, convertFileSrc } = window.__TAURI__.core;

  const overlay = document.getElementById("region-overlay");
  const frame = document.getElementById("region-overlay-frame");
  const selectionBox = document.getElementById("region-selection");
  const readout = document.getElementById("region-readout");

  let stateInfo = null;
  let scale = 1; // CSS px -> physical px on the frozen frame
  let dragStart = null;

  function cancel() {
    invoke("cancel_region_screenshot").catch(function () {});
  }

  async function init() {
    try {
      stateInfo = await invoke("get_region_overlay_state");
    } catch (e) {
      cancel();
      return;
    }
    if (!stateInfo) {
      cancel();
      return;
    }
    frame.src = convertFileSrc(stateInfo.frame_path);
    frame.addEventListener("load", function () {
      // The frozen frame is exactly the monitor in physical pixels and the
      // window is sized to the monitor, so one ratio covers both axes.
      if (frame.naturalWidth > 0 && overlay.clientWidth > 0) {
        scale = frame.naturalWidth / overlay.clientWidth;
      }
    });
  }

  function cssRectToPhysical(rect) {
    return {
      x: rect.x * scale,
      y: rect.y * scale,
      width: rect.width * scale,
      height: rect.height * scale,
    };
  }

  function showSelection(physicalRect) {
    selectionBox.style.display = "block";
    selectionBox.style.left = physicalRect.x / scale + "px";
    selectionBox.style.top = physicalRect.y / scale + "px";
    selectionBox.style.width = physicalRect.width / scale + "px";
    selectionBox.style.height = physicalRect.height / scale + "px";
    readout.style.display = "block";
    readout.textContent = RegionCore.readout(physicalRect);
    const belowTop = (physicalRect.y + physicalRect.height) / scale + 8;
    readout.style.left = physicalRect.x / scale + "px";
    readout.style.top = Math.min(belowTop, overlay.clientHeight - 24) + "px";
  }

  function hideSelection() {
    selectionBox.style.display = "none";
    readout.style.display = "none";
  }

  function dragRect(event) {
    return RegionCore.dragResult(
      dragStart,
      { x: event.clientX, y: event.clientY },
      { x: 0, y: 0, width: overlay.clientWidth, height: overlay.clientHeight },
    );
  }

  overlay.addEventListener("mousedown", function (event) {
    if (event.button !== 0) return;
    dragStart = { x: event.clientX, y: event.clientY };
  });

  overlay.addEventListener("mousemove", function (event) {
    if (!dragStart) return;
    const rect = dragRect(event);
    if (rect) {
      showSelection(cssRectToPhysical(rect));
    } else {
      hideSelection();
    }
  });

  overlay.addEventListener("mouseup", function (event) {
    if (event.button !== 0 || !dragStart) return;
    const start = dragStart;
    dragStart = null;
    const rect = RegionCore.dragResult(
      start,
      { x: event.clientX, y: event.clientY },
      { x: 0, y: 0, width: overlay.clientWidth, height: overlay.clientHeight },
    );
    if (!rect) {
      cancel();
      return;
    }
    invoke("complete_region_screenshot", {
      selection: {
        x: Math.round(rect.x * scale),
        y: Math.round(rect.y * scale),
        width: Math.round(rect.width * scale),
        height: Math.round(rect.height * scale),
      },
    }).catch(function (error) {
      console.error(error);
      cancel();
    });
  });

  window.addEventListener("keydown", function (event) {
    if (RegionCore.escapeCancels(event.key)) cancel();
  });

  window.addEventListener("blur", function () {
    cancel();
  });

  init();
})();
