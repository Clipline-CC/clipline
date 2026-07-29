// Pure foreground/background request arbitration. Keep DOM- and Tauri-free for
// Boa tests. Native lifecycle snapshots carry a monotonically increasing
// revision so a late startup response cannot override a newer hide event.
var WindowLifecycleCore = (() => {
  const makeState = (known, backgrounded, nativeRevision, generation, dirty) =>
    Object.freeze({
      known,
      backgrounded,
      nativeRevision,
      generation,
      dirty,
    });

  const initialState = () => makeState(false, true, null, 0, true);

  const transition = (
    state,
    accepted,
    enteredBackground = false,
    enteredForeground = false,
    refreshRequired = false,
    missedBackground = false,
  ) => {
    const result = {
      state,
      accepted,
      enteredBackground,
      enteredForeground,
      refreshRequired,
    };
    // Omit the false case so existing consumers and serialized test fixtures
    // keep their compact shape.
    if (missedBackground) result.missedBackground = true;
    return result;
  };

  const validSnapshot = (snapshot) => {
    if (!snapshot || typeof snapshot.backgrounded !== "boolean") return false;
    const revision = Number(snapshot.revision);
    return Number.isSafeInteger(revision) && revision >= 0;
  };

  const applySnapshot = (state, snapshot) => {
    if (!state) state = initialState();
    if (!validSnapshot(snapshot)) return transition(state, false);

    const revision = Number(snapshot.revision);
    const backgrounded = snapshot.backgrounded;
    if (state.known && revision < state.nativeRevision) {
      return transition(state, false);
    }
    if (state.known && revision === state.nativeRevision) {
      if (backgrounded !== state.backgrounded) return transition(state, false);
      return transition(state, true);
    }

    const enteredBackground = state.known && !state.backgrounded && backgrounded;
    const enteredForeground = !backgrounded && (!state.known || state.backgrounded);
    // Native revisions advance only when the mode changes. Returning to
    // foreground more than one revision later means the intervening background
    // event was not observed, so reconcile teardown and refresh explicitly.
    const missedBackground =
      state.known
      && !state.backgrounded
      && !backgrounded
      && revision > state.nativeRevision + 1;
    const refreshRequired = !backgrounded && (state.dirty || missedBackground);
    const next = makeState(
      true,
      backgrounded,
      revision,
      state.generation + 1,
      backgrounded,
    );
    return transition(
      next,
      true,
      enteredBackground,
      enteredForeground,
      refreshRequired,
      missedBackground,
    );
  };

  const requestRefresh = (state) => {
    if (!state) state = initialState();
    if (!state.known || state.backgrounded) {
      const next = state.dirty
        ? state
        : makeState(
            state.known,
            state.backgrounded,
            state.nativeRevision,
            state.generation,
            true,
          );
      return { state: next, refreshNow: false };
    }
    return { state, refreshNow: true };
  };

  const captureWork = (state) => {
    if (!state || !state.known || state.backgrounded) return null;
    return Object.freeze({
      generation: state.generation,
      nativeRevision: state.nativeRevision,
    });
  };

  const isWorkCurrent = (state, work) => (
    !!state
    && state.known
    && !state.backgrounded
    && !!work
    && work.generation === state.generation
    && work.nativeRevision === state.nativeRevision
  );

  return Object.freeze({
    initialState,
    applySnapshot,
    requestRefresh,
    captureWork,
    isWorkCurrent,
  });
})();

globalThis.WindowLifecycleCore = WindowLifecycleCore;
