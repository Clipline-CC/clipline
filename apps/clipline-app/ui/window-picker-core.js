// DOM-free preview queue for the custom-game window picker. One native
// capture runs at a time: a refresh or close abandons the queue, but the next
// capture still waits until the running one finishes.
(function (root) {
  "use strict";

  function freeze(state) {
    return Object.freeze({ ...state, queue: Object.freeze([...state.queue]) });
  }

  function initial() {
    return freeze({ generation: 0, open: false, queue: [], inFlight: null });
  }

  // A fresh listing: queue every window that can be captured (minimized
  // windows keep their icon), in list order.
  function begin(state, windows) {
    const queue = (windows || [])
      .filter((win) => win && !win.minimized)
      .map((win) => ({ handle: win.handle, processId: win.process_id }));
    return freeze({ ...state, generation: state.generation + 1, open: true, queue });
  }

  // The next request to send, if one may start now.
  function next(state) {
    if (!state.open || state.inFlight || state.queue.length === 0) {
      return { state, request: null };
    }
    const [head, ...rest] = state.queue;
    const request = Object.freeze({ generation: state.generation, ...head });
    return { state: freeze({ ...state, queue: rest, inFlight: request }), request };
  }

  // Record a finished request. `apply` says whether its preview still
  // belongs on screen.
  function finish(state, request) {
    if (!request || state.inFlight !== request) {
      return { state, apply: false };
    }
    const apply = state.open && request.generation === state.generation;
    return { state: freeze({ ...state, inFlight: null }), apply };
  }

  // Closing or picking a window: drop everything queued; a running capture
  // finishes quietly.
  function abandon(state) {
    return freeze({ ...state, generation: state.generation + 1, open: false, queue: [] });
  }

  function iconProcessIds(windows) {
    const seen = new Set();
    for (const win of windows || []) {
      if (win && win.process_id) seen.add(win.process_id);
    }
    return [...seen];
  }

  root.WindowPickerCore = Object.freeze({
    initial,
    begin,
    next,
    finish,
    abandon,
    iconProcessIds,
  });
})(globalThis);
