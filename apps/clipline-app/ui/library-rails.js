// Library: game event/play rails, metadata panel.
var activeGameEventIndex = -1;
var selectedGameEventIndex = -1;
var selectedGameEventTime = null;
var gameEventRailCollapsed = false;
var gameEventRows = [];
var activeGamePlayIndex = -1;
var selectedGamePlayIndex = -1;
var selectedGamePlayPending = false;
var selectedGamePlayStart = null;
var selectedGamePlayEnd = null;
var gamePlayRows = [];

function eventRailPolicy(clip) {
  if (activeGroupName) return { enabled: true, group: true, title: "group clips" };
  const presentation = pluginPresentationForClip(clip);
  return presentation && presentation.event_rail ? presentation.event_rail : null;
}

function playRailPolicy(clip) {
  if (activeGroupName) return { enabled: false };
  const presentation = pluginPresentationForClip(clip);
  return presentation && presentation.play_rail ? presentation.play_rail : null;
}

function metadataPanelPolicy(clip) {
  if (activeGroupName) return { enabled: false };
  const presentation = pluginPresentationForClip(clip);
  return presentation && presentation.metadata_panel ? presentation.metadata_panel : null;
}

function syncReviewSideRailLayout() {
  const eventRail = $("game-event-rail");
  const playRail = $("game-play-rail");
  const reviewBody = eventRail
    ? eventRail.closest(".review-body")
    : (playRail ? playRail.closest(".review-body") : null);
  if (!reviewBody) return;
  const eventVisible = eventRail && !eventRail.hidden;
  const playVisible = playRail && !playRail.hidden;
  reviewBody.classList.toggle("has-event-rail", Boolean(eventVisible || playVisible));
  reviewBody.classList.toggle(
    "event-rail-collapsed",
    Boolean(eventVisible && !playVisible && gameEventRailCollapsed),
  );
}

function clearGameEventSelection() {
  selectedGameEventIndex = -1;
  selectedGameEventTime = null;
}

function selectGameEvent(index, markerTime) {
  selectedGameEventIndex = index;
  selectedGameEventTime = Number.isFinite(markerTime) ? markerTime : null;
}

function selectedGameEventIndexForTime(currentTime) {
  if (selectedGameEventIndex < 0 || selectedGameEventTime == null) return -1;
  if (currentTime >= selectedGameEventTime - 0.15) {
    clearGameEventSelection();
    return -1;
  }
  return selectedGameEventIndex;
}

function clearGamePlaySelection() {
  selectedGamePlayIndex = -1;
  selectedGamePlayPending = false;
  selectedGamePlayStart = null;
  selectedGamePlayEnd = null;
}

function selectGamePlay(index, playStart, playEnd) {
  selectedGamePlayIndex = index;
  selectedGamePlayPending = true;
  selectedGamePlayStart = Number.isFinite(playStart) ? playStart : null;
  selectedGamePlayEnd = Number.isFinite(playEnd) ? playEnd : selectedGamePlayStart;
}

function selectedGamePlayIndexForTime(currentTime, options = {}) {
  if (selectedGamePlayIndex < 0 || selectedGamePlayStart == null) return -1;
  const t = Number(currentTime);
  const start = Number(selectedGamePlayStart);
  const end = Number.isFinite(selectedGamePlayEnd) ? Number(selectedGamePlayEnd) : start;
  const inSelectedPlay = Number.isFinite(t)
    && Number.isFinite(start)
    && t >= start - 0.15
    && t <= Math.max(start, end) + 0.15;
  if (options.keepGamePlaySelection || selectedGamePlayPending) {
    if (!options.keepGamePlaySelection) {
      if (inSelectedPlay) selectedGamePlayPending = false;
    }
    return selectedGamePlayIndex;
  }
  if (inSelectedPlay) return selectedGamePlayIndex;
  clearGamePlaySelection();
  return -1;
}

function syncGroupClipRail() {
  const activePath = currentClip && currentClip.path;
  const next = gameEventRows.findIndex((row) => activePath
    && PlayerCore.sameClipPath(row.dataset.groupClipPath, activePath));
  if (next === activeGameEventIndex) return;
  activeGameEventIndex = next;
  gameEventRows.forEach((row, index) => {
    const active = index === next;
    row.classList.toggle("active", Boolean(active));
    row.setAttribute("aria-current", active ? "true" : "false");
    if (active) row.scrollIntoView({ block: "nearest", inline: "nearest" });
  });
}

function clearGroupDragIndicators() {
  document.querySelectorAll(".group-clip-row.drag-before, .group-clip-row.drag-after")
    .forEach((row) => row.classList.remove("drag-before", "drag-after"));
}

function renderGroupClipRail() {
  const group = activeGroup();
  if (!group) return;
  activeGameEventIndex = -1;
  const rail = $("game-event-rail");
  const list = $("game-event-list");
  rail.classList.add("group-clip-rail");
  $("game-event-rail-title").textContent = group.name;
  $("game-event-rail-summary").textContent = `${group.members.length} clip${group.members.length === 1 ? "" : "s"} · drag to reorder`;
  list.replaceChildren();
  gameEventRows = [];
  group.members.forEach((clip, index) => {
    const item = document.createElement("li");
    item.className = "group-clip-row";
    item.draggable = true;
    item.dataset.groupClipPath = clip.path;

    const open = document.createElement("button");
    open.type = "button";
    open.className = "group-clip-open";
    open.dataset.groupClipPath = clip.path;
    const poster = document.createElement("span");
    poster.className = "group-clip-poster";
    poster.style.cssText = thumbGradient(clip);
    observePoster(clip.path, poster);
    const body = document.createElement("span");
    body.className = "group-clip-body";
    const title = document.createElement("strong");
    title.textContent = clipDisplayTitle(clip) || clip.name;
    const duration = Number(clip.duration_s ?? clip.markers?.duration_s);
    const meta = document.createElement("span");
    meta.textContent = [
      `${index + 1}`.padStart(2, "0"),
      Number.isFinite(duration) ? fmtDur(duration) : "",
      fmtMegabytes(clip.size_mb),
    ].filter(Boolean).join(" · ");
    body.append(title, meta);
    open.append(poster, body);
    open.title = "Click to play · Alt+Up/Down to reorder";
    open.addEventListener("click", () => openGroupMember(clip));
    open.addEventListener("keydown", (event) => {
      if (!event.altKey || !["ArrowUp", "ArrowDown"].includes(event.key)) return;
      const direction = event.key === "ArrowUp" ? "up" : "down";
      if ((direction === "up" && index === 0)
          || (direction === "down" && index === group.members.length - 1)) return;
      event.preventDefault();
      moveGroupClip(clip, direction);
    });
    gameEventRows.push(open);
    item.appendChild(open);
    item.addEventListener("dragstart", (event) => {
      groupDragSourcePath = clip.path;
      item.classList.add("dragging");
      if (event.dataTransfer) {
        event.dataTransfer.effectAllowed = "move";
        event.dataTransfer.setData("text/plain", "clipline-group-member");
      }
    });
    item.addEventListener("dragover", (event) => {
      if (!groupDragSourcePath) return;
      event.preventDefault();
      clearGroupDragIndicators();
      const sourceIndex = group.members.findIndex((member) =>
        PlayerCore.sameClipPath(member.path, groupDragSourcePath));
      if (sourceIndex === index) return;
      item.classList.add(index < sourceIndex ? "drag-before" : "drag-after");
      if (event.dataTransfer) event.dataTransfer.dropEffect = "move";
    });
    item.addEventListener("drop", (event) => {
      event.preventDefault();
      const sourcePath = groupDragSourcePath;
      clearGroupDragIndicators();
      groupDragSourcePath = "";
      dropGroupClip(sourcePath, clip.path);
    });
    item.addEventListener("dragend", () => {
      groupDragSourcePath = "";
      item.classList.remove("dragging");
      clearGroupDragIndicators();
    });
    item.addEventListener("contextmenu", (event) => showGroupClipContextMenu(event, clip));
    list.appendChild(item);
  });
  rail.hidden = false;
  syncReviewSideRailLayout();
  setGameEventRailCollapsed(gameEventRailCollapsed);
  syncGroupClipRail();
}

function renderGameEventRail(clip = currentClip) {
  const rail = $("game-event-rail");
  const title = $("game-event-rail-title");
  const summary = $("game-event-rail-summary");
  const list = $("game-event-list");
  const presentation = pluginPresentationForClip(clip);
  const eventRail = eventRailPolicy(clip);
  const markers = clipMatchEventMarkers(clip);
  activeGameEventIndex = -1;
  clearGameEventSelection();
  if (eventRail && eventRail.group) {
    renderGroupClipRail();
    return;
  }
  rail.classList.remove("group-clip-rail");
  if (!eventRail || !eventRail.enabled || !markers.length) {
    rail.hidden = true;
    rail.classList.remove("is-collapsed");
    syncReviewSideRailLayout();
    title.textContent = "";
    summary.textContent = "";
    list.replaceChildren();
    gameEventRows = [];
    return;
  }
  title.textContent = eventRail.title || (clip && clip.game ? `${clip.game.name} events` : "Game events");
  summary.textContent = markerSummary(markers);
  list.replaceChildren();
  gameEventRows = [];
  const playerSummary = clip && clip.markers ? clip.markers.player_summary : null;
  markers.forEach((marker, index) => {
    const item = document.createElement("li");
    const button = document.createElement("button");
    const view = gameEventRailItem(marker, playerSummary, presentation, {
      data_dragon: presentation && presentation.data_dragon,
    });
    button.type = "button";
    button.setAttribute("data-game-event-index", String(index));
    button.setAttribute("data-game-event-time", String(marker.t_s || 0));
    button.className = `marker-${view.category} game-event-row-${view.allegiance || "neutral"}`;
    const time = document.createElement("span");
    time.className = "game-event-time";
    time.textContent = fmtDur(marker.t_s || 0);
    button.title = markerDisplayLabel(marker, presentation);
    if (view.layout === "duel" && view.actor && view.victim) {
      button.classList.add("game-event-duel");
      button.append(
        time,
        gameEventPortrait(view.actor),
        gameEventIcon(view, marker, presentation),
        gameEventPortrait(view.victim),
      );
    } else if (view.layout === "actor_event") {
      const icon = gameEventIcon(view, marker, presentation);
      icon.classList.add("game-event-objective-icon");
      button.classList.add("game-event-actor-event");
      if (view.actor) {
        button.append(
          time,
          gameEventPortrait(view.actor),
          icon,
        );
      } else {
        const label = document.createElement("span");
        label.className = "game-event-label";
        label.textContent = view.text || markerEventText(marker, presentation);
        button.append(time, label, icon);
      }
    } else {
      const label = document.createElement("span");
      label.className = "game-event-label";
      label.textContent = view.text || markerEventText(marker, presentation);
      button.append(time, label);
    }
    button.addEventListener("click", () => {
      const markerTime = marker.t_s || 0;
      selectGameEvent(index, markerTime);
      seekTo(markerTime - MARKER_LEAD_S, { keepGameEventSelection: true });
      video.play().catch(() => syncPlayState());
    });
    gameEventRows.push(button);
    item.appendChild(button);
    list.appendChild(item);
  });
  rail.hidden = false;
  syncReviewSideRailLayout();
  setGameEventRailCollapsed(gameEventRailCollapsed);
}

function setGameEventRailCollapsed(collapsed) {
  gameEventRailCollapsed = Boolean(collapsed);
  const rail = $("game-event-rail");
  const toggle = $("game-event-rail-toggle");
  if (!rail) return;
  rail.classList.toggle("is-collapsed", gameEventRailCollapsed);
  syncReviewSideRailLayout();
  if (toggle) {
    const policy = eventRailPolicy(currentClip);
    const subject = policy && policy.group ? policy.title : "match events";
    const label = `${gameEventRailCollapsed ? "Expand" : "Collapse"} ${subject}`;
    toggle.title = label;
    toggle.setAttribute("aria-label", label);
    toggle.setAttribute("aria-expanded", gameEventRailCollapsed ? "false" : "true");
  }
  if (!gameEventRailCollapsed) {
    syncGameEventRail(video.currentTime || 0, { force: true });
  }
}

function syncGameEventRail(currentTime = video.currentTime || 0, options = {}) {
  const rail = $("game-event-rail");
  if (!rail || rail.hidden || rail.classList.contains("is-collapsed")) return;
  if (!gameEventRows.length) return;
  const eventRail = eventRailPolicy(currentClip);
  if (eventRail && eventRail.group) {
    syncGroupClipRail();
    return;
  }
  const markers = clipMatchEventMarkers();
  const selectedIndex = selectedGameEventIndexForTime(currentTime);
  const next = gameEventActiveIndex(markers, currentTime, selectedIndex);
  if (next === activeGameEventIndex && !options.force) return;
  activeGameEventIndex = next;
  gameEventRows.forEach((row) => {
    const active = Number(row.dataset.gameEventIndex) === next;
    row.classList.toggle("active", active);
    row.setAttribute("aria-current", active ? "true" : "false");
    if (active) row.scrollIntoView({ block: "nearest", inline: "nearest" });
  });
}

function renderGamePlayRail(clip = currentClip) {
  const rail = $("game-play-rail");
  const title = $("game-play-rail-title");
  const summary = $("game-play-rail-summary");
  const list = $("game-play-list");
  const playRail = playRailPolicy(clip);
  const plays = clipPlays(clip);
  activeGamePlayIndex = -1;
  clearGamePlaySelection();
  if (!rail || !title || !summary || !list) return;
  if (!playRail || !playRail.enabled || !plays.length) {
    rail.hidden = true;
    title.textContent = "Set plays";
    summary.textContent = "";
    list.replaceChildren();
    gamePlayRows = [];
    syncReviewSideRailLayout();
    return;
  }

  const duration = clip && Number.isFinite(clip.duration_s)
    ? clip.duration_s
    : (clip && clip.markers && Number.isFinite(clip.markers.duration_s) ? clip.markers.duration_s : 0);
  title.textContent = playRail.title || "Set plays";
  summary.textContent = playSummary(plays);
  list.replaceChildren();
  gamePlayRows = [];
  playBlocks(plays, duration).forEach((play, index) => {
    const view = playRailItem(play.play);
    const item = document.createElement("li");
    const button = document.createElement("button");
    button.type = "button";
    button.setAttribute("data-game-play-index", String(index));
    button.setAttribute("data-game-play-time", String(play.start || 0));
    button.title = [view.title, view.meta, view.time].filter(Boolean).join("\n");
    const thumbnail = document.createElement("span");
    thumbnail.className = "game-play-thumb";
    if (view.coverUrl) {
      const image = document.createElement("img");
      image.src = view.coverUrl;
      image.alt = "";
      image.loading = "lazy";
      image.decoding = "async";
      thumbnail.appendChild(image);
    } else {
      thumbnail.textContent = "osu!";
    }
    const text = document.createElement("span");
    text.className = "game-play-body";
    const playTitleEl = document.createElement("span");
    playTitleEl.className = "game-play-title";
    const song = document.createElement("span");
    song.className = "game-play-song";
    song.textContent = view.artistTitle || view.title;
    playTitleEl.appendChild(song);
    if (view.difficulty) {
      const difficulty = document.createElement("span");
      difficulty.className = "game-play-difficulty";
      difficulty.textContent = `[${view.difficulty}]`;
      playTitleEl.appendChild(difficulty);
    }
    if (view.mods) {
      const mods = document.createElement("span");
      mods.className = "game-play-mods";
      mods.textContent = view.mods;
      playTitleEl.appendChild(mods);
    }
    if (view.starRating) {
      const starRating = document.createElement("span");
      starRating.className = "game-play-stars";
      starRating.textContent = view.starRating;
      playTitleEl.appendChild(starRating);
    }
    const meta = document.createElement("span");
    meta.className = "game-play-meta";
    meta.textContent = view.meta || view.time;
    text.append(playTitleEl, meta);
    button.append(thumbnail, text);
    button.addEventListener("click", () => {
      selectGamePlay(index, play.start, play.end);
      seekTo(play.start, { keepGamePlaySelection: true });
      video.play().catch(() => syncPlayState());
    });
    button.addEventListener("contextmenu", (ev) => {
      const exportRange = playExportRange(play.play);
      showGamePlayContextMenu(ev, {
        title: view.artistTitle || view.title,
        range: exportRange ? { start: play.start, end: play.end } : null,
      });
    });
    gamePlayRows.push(button);
    item.appendChild(button);
    list.appendChild(item);
  });
  rail.hidden = false;
  syncReviewSideRailLayout();
  syncGamePlayRail(video.currentTime || 0, { force: true });
}

function syncGamePlayRail(currentTime = video.currentTime || 0, options = {}) {
  const rail = $("game-play-rail");
  if (!rail || rail.hidden) return;
  const selectedIndex = selectedGamePlayIndexForTime(currentTime, options);
  const next = playActiveIndex(clipPlays(), currentTime, selectedIndex);
  if (next === activeGamePlayIndex && !options.force) return;
  activeGamePlayIndex = next;
  gamePlayRows.forEach((row) => {
    const active = Number(row.dataset.gamePlayIndex) === next;
    row.classList.toggle("active", active);
    row.setAttribute("aria-current", active ? "true" : "false");
    if (active) row.scrollIntoView({ block: "nearest", inline: "nearest" });
  });
  document.querySelectorAll(".play-block").forEach((block) => {
    const active = Number(block.dataset.gamePlayIndex) === next;
    block.classList.toggle("active", active);
    block.setAttribute("aria-current", active ? "true" : "false");
  });
}

function metadataIconFallbackText(value) {
  const letters = String(value || "").match(/[A-Za-z0-9]/g) || [];
  return (letters.slice(0, 2).join("").toUpperCase() || "?").slice(0, 2);
}

function renderMetadataIcon(entry, className) {
  const icon = document.createElement("span");
  icon.className = className;
  icon.title = entry.value || "";
  icon.setAttribute("aria-label", entry.value || "Metadata icon");
  if (entry.asset) {
    const img = document.createElement("img");
    img.src = entry.asset;
    img.alt = entry.value || "";
    img.addEventListener("error", () => {
      img.remove();
      icon.textContent = metadataIconFallbackText(entry.value || entry.assetKey);
    }, { once: true });
    icon.appendChild(img);
  } else {
    icon.textContent = metadataIconFallbackText(entry.value || entry.assetKey);
  }
  return icon;
}

function renderMetadataIconList(field) {
  const list = document.createElement("div");
  list.className = `game-metadata-icons ${field.type}`;
  list.setAttribute("aria-label", field.label || field.type);
  for (const entry of field.items || []) {
    list.appendChild(renderMetadataIcon(entry, "game-metadata-icon"));
  }
  return list;
}

function renderGameMetadataPanel(clip = currentClip) {
  const panel = $("game-metadata-panel");
  const fieldsRoot = $("game-metadata-fields");
  if (!clip) {
    panel.hidden = true;
    fieldsRoot.replaceChildren();
    return;
  }
  const presentation = pluginPresentationForClip(clip);
  const metadataPanel = metadataPanelPolicy(clip);
  const summary = clip && clip.markers ? clip.markers.player_summary : null;
  const fields = metadataPanel && metadataPanel.fields
    ? playerSummaryFields(summary, metadataPanel.fields, {
      data_dragon: presentation && presentation.data_dragon,
    })
    : [];
  if (!metadataPanel || !metadataPanel.enabled || !fields.length) {
    panel.hidden = true;
    fieldsRoot.replaceChildren();
    return;
  }
  fieldsRoot.replaceChildren();
  for (const field of fields) {
    if (field.type === "portrait") {
      const portrait = document.createElement("div");
      portrait.className = "game-metadata-portrait";
      portrait.title = field.value;
      if (field.asset) {
        const img = document.createElement("img");
        img.src = field.asset;
        img.alt = field.value;
        img.addEventListener("error", () => {
          img.remove();
          portrait.textContent = String(field.value || "?").slice(0, 2).toUpperCase();
        }, { once: true });
        portrait.appendChild(img);
      } else {
        portrait.textContent = String(field.value || "?").slice(0, 2).toUpperCase();
      }
      fieldsRoot.appendChild(portrait);
      continue;
    }
    if (field.type === "summoner_spells" || field.type === "item_build") {
      fieldsRoot.appendChild(renderMetadataIconList(field));
      continue;
    }
    const item = document.createElement("div");
    item.className = `game-metadata-field ${field.type}`;
    if (field.label) {
      const label = document.createElement("strong");
      label.textContent = field.label;
      item.appendChild(label);
    }
    const value = document.createElement("span");
    value.textContent = field.value;
    item.appendChild(value);
    if (field.secondary) {
      const secondary = document.createElement("small");
      secondary.textContent = field.secondary;
      item.appendChild(secondary);
    }
    fieldsRoot.appendChild(item);
  }
  panel.hidden = false;
}
