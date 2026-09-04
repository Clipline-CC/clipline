// Library: selection, filter/search, gallery render, context menus.
/* ---- gallery: multi-select + bulk actions ---- */

// Update a single card's selection UI without a full re-render. Windows
// backslashes make `[data-clip-path="..."]` fragile as a CSS selector, so
// iterate the cards and match `dataset.clipPath` in JS instead.
function applySelectionToCard(card, on) {
  card.classList.toggle("selected", on);
}

function findClipCard(path) {
  for (const card of document.querySelectorAll("#gallery-grid .card[data-clip-path]")) {
    if (card.dataset.clipPath === path) return card;
  }
  return null;
}

function toggleClipSelection(path) {
  const on = selectedClipPaths.has(path);
  if (on) selectedClipPaths.delete(path);
  else selectedClipPaths.add(path);
  const card = findClipCard(path);
  if (card) applySelectionToCard(card, !on);
  syncBulkBar();
}

function selectClipFromContext(path) {
  if (!path || gallerySource !== "local") return;
  selectMode = true;
  selectedClipPaths.add(path);
  const card = findClipCard(path);
  if (card) applySelectionToCard(card, true);
  syncSelectionControls();
}

function clearSelection() {
  selectedClipPaths.clear();
  for (const card of document.querySelectorAll("#gallery-grid .card[data-clip-path]")) {
    applySelectionToCard(card, false);
  }
  syncBulkBar();
}

function selectAllVisible() {
  selectedClipPaths = new Set(selectedClipPaths);
  for (const card of document.querySelectorAll("#gallery-grid .card[data-clip-path]")) {
    if (!card.dataset.clipPath) continue;
    selectedClipPaths.add(card.dataset.clipPath);
    applySelectionToCard(card, true);
  }
  syncBulkBar();
}

function exitSelectMode() {
  selectMode = false;
  clearSelection();
  syncSelectionControls();
}

function syncSelectToggleLabel() {
  const toggle = $("gallery-select-toggle");
  if (toggle) toggle.textContent = selectMode ? "Done" : "Select multiple";
}

function syncSelectionControls() {
  if (gallerySource !== "local" && selectMode) {
    selectMode = false;
    clearSelection();
  }
  const toggle = $("gallery-select-toggle");
  if (toggle) {
    toggle.hidden = gallerySource !== "local";
    toggle.classList.toggle("active", selectMode);
    syncSelectToggleLabel();
  }
  const grid = $("gallery-grid");
  if (grid) grid.classList.toggle("select-mode", selectMode && gallerySource === "local");
  syncBulkBar();
}

function syncBulkBar() {
  const bar = $("gallery-bulk-bar");
  if (!bar) return;
  const count = selectedClipPaths.size;
  const visible = (selectMode || count > 0) && gallerySource === "local";
  bar.hidden = !visible;
  $("bulk-count").textContent = `${count} selected`;
  const del = $("bulk-delete");
  if (del) del.disabled = count === 0;
}

/* ---- gallery: filter / sort / group ---- */

function syncLeagueGameTypeFilter() {
  const present = [];
  const seen = new Set();
  for (const c of clipsCache) {
    if (!c.game || c.game.id !== "league_of_legends") continue;
    const category = c.game.queue && c.game.queue.category ? c.game.queue.category : "unknown";
    if (seen.has(category)) continue;
    seen.add(category);
    present.push(category);
  }
  leagueGameTypePresent = present;
  const optionsKey = present.slice().sort().join("|");
  leagueGameTypeOptionsKey = optionsKey;
  // Empty `present` still clears: a Replay chip with no League clips left
  // would otherwise hide every remaining non-League card.
  if (galleryGameType !== "all" && !seen.has(galleryGameType)) {
    clearGallerySearchToken({ render: false });
  }
  renderGallerySearchToken();
}

function renderGallerySearchToken() {
  const root = $("gallery-search-tokens");
  if (!root) return;
  root.replaceChildren();
  if (!gallerySearchToken) {
    root.hidden = true;
    return;
  }
  const chip = document.createElement("span");
  chip.className = "gallery-search-chip";
  const label = document.createElement("span");
  label.textContent = GallerySearchCore.chipText(gallerySearchToken.key, gallerySearchToken.value);
  const clear = document.createElement("button");
  clear.type = "button";
  clear.className = "gallery-search-chip-clear";
  clear.title = "Clear filter";
  clear.setAttribute("aria-label", "Clear LoL Type filter");
  clear.textContent = "×";
  clear.addEventListener("click", (ev) => {
    ev.preventDefault();
    ev.stopPropagation();
    clearGallerySearchToken({ render: true });
    $("gallery-search").focus();
    updateGallerySearchMenu();
  });
  chip.append(label, clear);
  root.appendChild(chip);
  root.hidden = false;
}

function clearGallerySearchToken({ render = true } = {}) {
  gallerySearchToken = null;
  galleryGameType = "all";
  renderGallerySearchToken();
  if (render) renderClips();
}

function applyGallerySearchToken(key, value, { clearInput = true } = {}) {
  gallerySearchToken = { key, value: value || null };
  galleryGameType = value || "all";
  const input = $("gallery-search");
  if (clearInput && input) input.value = "";
  gallerySearch = input ? input.value.trim().toLowerCase() : "";
  renderGallerySearchToken();
  if (value) hideGallerySearchMenu();
  renderClips();
}

function hideGallerySearchMenu() {
  const menu = $("gallery-search-menu");
  if (!menu) return;
  menu.hidden = true;
  menu.replaceChildren();
  gallerySearchMenuIndex = 0;
}

function gallerySearchMenuItems() {
  return [...document.querySelectorAll("#gallery-search-menu .gallery-search-option")];
}

function highlightGallerySearchMenu(index) {
  const items = gallerySearchMenuItems();
  if (!items.length) return;
  gallerySearchMenuIndex = ((index % items.length) + items.length) % items.length;
  items.forEach((item, i) => item.classList.toggle("is-active", i === gallerySearchMenuIndex));
}

function updateGallerySearchMenu() {
  const menu = $("gallery-search-menu");
  const input = $("gallery-search");
  if (!menu || !input || gallerySource === "cloud") {
    hideGallerySearchMenu();
    return;
  }
  if (gallerySearchToken && gallerySearchToken.value) {
    hideGallerySearchMenu();
    return;
  }
  if (document.activeElement !== input) {
    hideGallerySearchMenu();
    return;
  }

  const pendingKey = gallerySearchToken && !gallerySearchToken.value ? gallerySearchToken.key : null;
  const inspected = GallerySearchCore.inspect(input.value);
  let mode = "none";
  let filterKey = pendingKey;
  let valueDraft = input.value;
  if (pendingKey) {
    mode = "values";
  } else if (inspected.kind === "empty" || inspected.kind === "filters") {
    mode = "filters";
  } else if (inspected.kind === "values") {
    mode = "values";
    filterKey = inspected.filterKey;
    valueDraft = inspected.valueDraft;
  }

  menu.replaceChildren();
  if (mode === "filters") {
    const filters = GallerySearchCore.matchingFilters(input.value);
    if (!filters.length) {
      hideGallerySearchMenu();
      return;
    }
    const head = document.createElement("div");
    head.className = "gallery-search-menu-head";
    head.textContent = "Filters";
    menu.appendChild(head);
    for (const filter of filters) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "gallery-search-option";
      button.dataset.searchAction = "filter";
      button.dataset.filterKey = filter.key;
      const title = document.createElement("strong");
      title.textContent = `${filter.chipLabel}:`;
      const hint = document.createElement("span");
      hint.textContent = filter.hint;
      button.append(title, hint);
      menu.appendChild(button);
    }
  } else if (mode === "values") {
    const values = GallerySearchCore.matchingValues(filterKey, valueDraft, leagueGameTypePresent);
    if (!values.length) {
      hideGallerySearchMenu();
      return;
    }
    const filter = GallerySearchCore.filterByKey(filterKey);
    const head = document.createElement("div");
    head.className = "gallery-search-menu-head";
    head.textContent = filter ? filter.chipLabel : "Filter";
    menu.appendChild(head);
    for (const item of values) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "gallery-search-option";
      button.dataset.searchAction = "value";
      button.dataset.filterKey = filterKey;
      button.dataset.filterValue = item.value;
      const title = document.createElement("strong");
      title.textContent = item.label;
      button.appendChild(title);
      menu.appendChild(button);
    }
  } else {
    hideGallerySearchMenu();
    return;
  }
  menu.hidden = false;
  highlightGallerySearchMenu(0);
}

function activateGallerySearchMenuItem(button) {
  if (!button) return;
  if (button.dataset.searchAction === "filter") {
    applyGallerySearchToken(button.dataset.filterKey, null);
    $("gallery-search").focus();
    updateGallerySearchMenu();
    return;
  }
  if (button.dataset.searchAction === "value") {
    applyGallerySearchToken(button.dataset.filterKey, button.dataset.filterValue);
  }
}

function onGallerySearchInput() {
  const input = $("gallery-search");
  if (gallerySource === "cloud") {
    gallerySearch = input.value.trim().toLowerCase();
    hideGallerySearchMenu();
    renderClips();
    return;
  }
  if (gallerySearchToken && gallerySearchToken.value) {
    gallerySearch = input.value.trim().toLowerCase();
    hideGallerySearchMenu();
    renderClips();
    return;
  }
  if (gallerySearchToken && !gallerySearchToken.value) {
    gallerySearch = "";
    updateGallerySearchMenu();
    return;
  }
  const inspected = GallerySearchCore.inspect(input.value);
  if (inspected.kind === "values" && inspected.filterKey) {
    applyGallerySearchToken(inspected.filterKey, null, { clearInput: true });
    input.value = inspected.valueDraft;
    gallerySearch = "";
    const exact = GallerySearchCore.matchingValues(
      inspected.filterKey,
      inspected.valueDraft,
      leagueGameTypePresent,
    );
    if (exact.length === 1 && inspected.valueDraft.trim()) {
      const draft = inspected.valueDraft.trim().toLowerCase();
      if (draft === exact[0].label.toLowerCase() || draft === exact[0].value) {
        applyGallerySearchToken(inspected.filterKey, exact[0].value);
        return;
      }
    }
    updateGallerySearchMenu();
    return;
  }
  gallerySearch = inspected.kind === "query" ? inspected.remainder.trim().toLowerCase() : "";
  updateGallerySearchMenu();
  renderClips();
}

function onGallerySearchKeydown(ev) {
  const menu = $("gallery-search-menu");
  const items = gallerySearchMenuItems();
  if (ev.key === "ArrowDown" && menu && !menu.hidden && items.length) {
    ev.preventDefault();
    highlightGallerySearchMenu(gallerySearchMenuIndex + 1);
    return;
  }
  if (ev.key === "ArrowUp" && menu && !menu.hidden && items.length) {
    ev.preventDefault();
    highlightGallerySearchMenu(gallerySearchMenuIndex - 1);
    return;
  }
  if (ev.key === "Enter" && menu && !menu.hidden && items.length) {
    ev.preventDefault();
    activateGallerySearchMenuItem(items[gallerySearchMenuIndex]);
    return;
  }
  if (ev.key === "Escape") {
    if (menu && !menu.hidden) {
      ev.preventDefault();
      hideGallerySearchMenu();
      return;
    }
    if (gallerySearchToken) {
      ev.preventDefault();
      clearGallerySearchToken({ render: true });
    }
    return;
  }
  if (ev.key === "Backspace" && $("gallery-search").value === "" && gallerySearchToken) {
    ev.preventDefault();
    if (gallerySearchToken.value) {
      applyGallerySearchToken(gallerySearchToken.key, null);
    } else {
      clearGallerySearchToken({ render: true });
      updateGallerySearchMenu();
    }
  }
}

function filterGalleryClips(clips, { groupName = "" } = {}) {
  const items = [];
  let maxModifiedUnix = 0;
  for (const c of clips) {
    if (galleryFilter === "group" && !groupName) continue;
    const kind = clipKind(c);
    if ((galleryFilter === "replay" || galleryFilter === "session" || galleryFilter === "trim")
      && kind !== galleryFilter) continue;
    if (galleryFilter === "favorite" && !c.favorite) continue;
    if (galleryFilter === "marked" && !clipMarkers(c).length) continue;
    if (galleryGameType !== "all") {
      const category = c.game && c.game.id === "league_of_legends"
        ? (c.game.queue && c.game.queue.category || "unknown")
        : null;
      if (category !== galleryGameType) continue;
    }
    if (gallerySearch) {
      const champ = c.markers && c.markers.player_summary ? c.markers.player_summary.champion_name : "";
      const queue = c.game && c.game.queue ? c.game.queue.label : "";
      const hay = `${groupName} ${clipDisplayTitle(c)} ${c.name} ${champ} ${c.session || ""} ${c.game ? c.game.name : ""} ${queue}`.toLowerCase();
      if (!hay.includes(gallerySearch)) continue;
    }
    items.push(c);
    const modifiedUnix = Number(c && c.modified_unix);
    if (Number.isFinite(modifiedUnix)) {
      maxModifiedUnix = Math.max(maxModifiedUnix, modifiedUnix);
    }
  }
  return { items, maxModifiedUnix };
}

function sortGalleryClips(clips) {
  const out = clips.slice();
  const markerCount = (c) => clipMarkers(c).length;
  if (gallerySort === "old") out.sort((a, b) => a.modified_unix - b.modified_unix);
  else if (gallerySort === "big") out.sort((a, b) => b.size_mb - a.size_mb);
  else if (gallerySort === "marks") out.sort((a, b) => markerCount(b) - markerCount(a));
  else out.sort((a, b) => b.modified_unix - a.modified_unix);
  return out;
}

// Bucket clips by an arbitrary key. Clips keep the caller's incoming order
// (already sorted by the chosen gallery sort), so Largest / Most markers
// survives inside each group; only the group order is by recency. A
// null-prototype map keeps game names like "constructor" or "__proto__" from
// colliding with inherited Object properties and skipping bucket creation.
function bucketGroups(clips, keyFor, labelFor) {
  const order = [];
  const by = Object.create(null);
  for (const c of clips) {
    const key = keyFor(c);
    if (!by[key]) { by[key] = { label: labelFor(c), t: 0, clips: [] }; order.push(key); }
    by[key].clips.push(c);
    by[key].t = Math.max(by[key].t, c.modified_unix);
  }
  const groups = order.map((k) => by[k]);
  groups.sort((a, b) => b.t - a.t);
  return groups;
}

function galleryDayGroups(clips) {
  return bucketGroups(
    clips,
    (c) => { const d = new Date(c.modified_unix * 1000); return `${d.getFullYear()}-${d.getMonth()}-${d.getDate()}`; },
    (c) => PresentationCore.formatGalleryDay(new Date(c.modified_unix * 1000)),
  );
}

function galleryGameGroups(clips) {
  const label = (c) => (c.game && c.game.name ? c.game.name : "No game detected");
  return bucketGroups(clips, label, label);
}

// Relative-date buckets (Photos-style); only non-empty buckets are returned.
function gallerySmartGroups(clips) {
  const sod = new Date();
  sod.setHours(0, 0, 0, 0);
  const todayStart = sod.getTime() / 1000;
  const defs = [
    { label: "Today", test: (t) => t >= todayStart },
    { label: "Yesterday", test: (t) => t >= todayStart - 86400 && t < todayStart },
    { label: "Earlier this week", test: (t) => t >= todayStart - 7 * 86400 && t < todayStart - 86400 },
    { label: "Earlier", test: () => true },
  ];
  const out = defs.map((d) => ({ label: d.label, test: d.test, clips: [] }));
  // Clips keep the incoming gallery-sort order; only the buckets themselves
  // carry a fixed Today → Earlier reading order.
  for (const c of clips) out.find((o) => o.test(c.modified_unix)).clips.push(c);
  return out.filter((g) => g.clips.length).map(({ label, clips }) => ({ label, clips }));
}

function galleryGroups(clips) {
  switch (galleryGroup) {
    case "session": return sessionGroups(clips);
    case "day": return galleryDayGroups(clips);
    case "game": return galleryGameGroups(clips);
    case "none": return [{ label: null, clips: clips.slice() }];
    default: return gallerySmartGroups(clips);
  }
}

function galleryIdentityKey(item) {
  return String(
    item && (item.path || item.remote_clip_id || item.name) || "",
  );
}

function groupedGalleryIdentity(
  prefix,
  total,
  firstItem,
  lastItem,
  maxModifiedUnix,
) {
  const modified = Number(maxModifiedUnix);
  return JSON.stringify([
    prefix,
    total,
    galleryIdentityKey(firstItem),
    galleryIdentityKey(lastItem),
    Number.isFinite(modified) ? modified : 0,
  ]);
}

function syncGalleryPagination(info) {
  const pagination = $("gallery-pagination");
  if (!pagination) return;
  const page = info || {
    page: 0,
    pageCount: 0,
    total: 0,
    start: 0,
    end: 0,
    hasPrevious: false,
    hasNext: false,
  };
  galleryPageTotal = page.total;
  pagination.hidden = page.pageCount <= 1;
  $("gallery-page-prev").disabled = !page.hasPrevious;
  $("gallery-page-next").disabled = !page.hasNext;
  $("gallery-page-label").textContent = page.pageCount
    ? `${page.start + 1}\u2013${page.end} of ${page.total} \u00b7 page ${page.page + 1} of ${page.pageCount}`
    : "";
}

function changeGalleryPage(delta) {
  const requested = galleryPageState.page + delta;
  const next = GalleryWindowCore.setPage(
    galleryPageState,
    requested,
    galleryPageTotal,
  );
  if (next.page === galleryPageState.page) return;
  galleryPageState = next;
  renderClips();
  const root = gallerySource === "cloud"
    ? $("cloud-gallery-grid")
    : $("gallery-grid");
  if (root) root.scrollTop = 0;
}

function releaseGalleryRoot(root) {
  if (!root) return;
  root.querySelectorAll("img").forEach((img) => {
    img.removeAttribute("src");
  });
  root.replaceChildren();
}

function beginBoundedGalleryRender() {
  if (posterObserver) posterObserver.disconnect();
  posterRenderGeneration += 1;
  posterQueue = new WeakMap();
  posterWorkQueue = [];
  releaseGalleryRoot($("gallery-grid"));
  releaseGalleryRoot($("cloud-gallery-grid"));
}

function renderClips() {
  if (!captureForegroundWork()) {
    requestWindowRefresh();
    return;
  }
  syncUploadClipButton();
  syncReviewLocalActions();
  // Keep the home in sync: empty library shows the capture preview, otherwise
  // the gallery. (Editor/settings arbitration lives in updateViews.)
  updateViews();
  const root = $("gallery-grid");
  const cloudRoot = $("cloud-gallery-grid");
  if (!root) return;
  const showingCloud = gallerySource === "cloud";
  root.hidden = showingCloud;
  if (cloudRoot) cloudRoot.hidden = !showingCloud;
  $("gallery-filter").hidden = showingCloud;
  $("gallery-group").hidden = showingCloud;
  $("gallery-sort").hidden = showingCloud;
  syncSelectionControls();
  document.querySelectorAll("#gallery-source-tabs .source-tab").forEach((tab) => {
    tab.classList.toggle("active", tab.dataset.gallerySource === gallerySource);
  });
  if (showingCloud) {
    renderCloudClips();
    loadCloudClips();
    return;
  }
  syncLeagueGameTypeFilter();
  beginBoundedGalleryRender();
  pruneLocalPosterCache(clipsCache);
  const topLevelClips = topLevelLocalClips(clipsCache);
  const allLibraryGroups = localGroups();
  const filteredResult = filterGalleryClips(topLevelClips);
  const filtered = filteredResult.items;
  const libraryGroups = visibleLocalGroups();
  const sorted = sortGalleryClips(filtered);
  const groups = galleryGroups(sorted);
  const firstGroup = groups[0];
  const lastGroup = groups[groups.length - 1];
  const firstItems = firstGroup && firstGroup.clips || [];
  const lastItems = lastGroup && lastGroup.clips || [];
  const identity = groupedGalleryIdentity(
    `local|${galleryFilter}|${galleryGameType}|${gallerySort}|${galleryGroup}|${gallerySearch}|${libraryGroups.map((group) => `${group.name}:${group.members.map((clip) => clip.group.order).join(",")}`).join("|")}`,
    filtered.length,
    firstItems[0],
    lastItems[lastItems.length - 1],
    filteredResult.maxModifiedUnix,
  );
  galleryPageState = GalleryWindowCore.updateState(galleryPageState, {
    identity,
    total: filtered.length,
  });
  const page = GalleryWindowCore.windowGroups(groups, galleryPageState);
  syncGalleryPagination(page);
  const visibleItems = filtered.length + libraryGroups.length;
  const totalItems = topLevelClips.length + allLibraryGroups.length;
  $("gallery-count").textContent = galleryFilter === "group"
    ? `${libraryGroups.length} of ${allLibraryGroups.length} group${allLibraryGroups.length === 1 ? "" : "s"}`
    : totalItems ? `${visibleItems} of ${totalItems}` : "";
  if (!clipsCache.length) {
    const empty = document.createElement("div");
    empty.className = "gallery-empty";
    empty.textContent = `No clips yet - press ${saveHotkeyLabel()} while something plays.`;
    root.appendChild(empty);
    return;
  }
  if (!filtered.length && !libraryGroups.length) {
    const empty = document.createElement("div");
    empty.className = "gallery-empty";
    empty.textContent = "No clips match those filters.";
    root.appendChild(empty);
    return;
  }
  if (galleryPageState.page === 0) renderGroupCards(root, libraryGroups);
  if (galleryFilter === "group") {
    return;
  }
  for (const group of page.groups) {
    if (group.label !== null) {
      const head = document.createElement("div");
      head.className = "gallery-group-head";
      const label = document.createElement("span");
      label.textContent = group.label;
      const count = document.createElement("span");
      count.className = "gcount";
      count.textContent = group.totalCount;
      head.append(label, count);
      root.appendChild(head);
    }
    for (const c of group.items) root.appendChild(clipCard(c));
  }
}

function renderCloudClips() {
  if (!captureForegroundWork()) {
    requestWindowRefresh();
    return;
  }
  const root = $("cloud-gallery-grid");
  if (!root) return;
  beginBoundedGalleryRender();
  const entries = cloudLibraryRecords();
  const filtered = [];
  let maxModifiedUnix = 0;
  for (const entry of entries) {
    if (!cloudEntryMatchesSearch(entry)) continue;
    filtered.push(entry);
    const modifiedUnix = Number(entry && (entry.updated_at_unix || entry.modified_unix));
    if (Number.isFinite(modifiedUnix)) {
      maxModifiedUnix = Math.max(maxModifiedUnix, modifiedUnix);
    }
  }
  const groups = [{ label: null, clips: filtered }];
  const identity = groupedGalleryIdentity(
    `cloud|${cloudAccountKey()}|${gallerySearch}`,
    filtered.length,
    filtered[0],
    filtered[filtered.length - 1],
    maxModifiedUnix,
  );
  galleryPageState = GalleryWindowCore.updateState(galleryPageState, {
    identity,
    total: filtered.length,
  });
  const page = GalleryWindowCore.windowItems(filtered, galleryPageState);
  syncGalleryPagination(page);
  $("gallery-count").textContent = entries.length
    ? `${filtered.length} of ${entries.length}`
    : "";
  if (cloudClipsLoading && !entries.length) {
    const empty = document.createElement("div");
    empty.className = "gallery-empty";
    empty.textContent = "Loading cloud clips...";
    root.appendChild(empty);
    return;
  }
  if (cloudClipsError) {
    const error = document.createElement("div");
    error.className = "gallery-empty cloud-error";
    error.textContent = cloudClipsError;
    root.appendChild(error);
    if (!entries.length) return;
  }
  if (!entries.length) {
    const empty = document.createElement("div");
    empty.className = "gallery-empty";
    empty.textContent = cloudConnected() ? "No cloud clips yet." : "Not connected to Clipline Cloud.";
    root.appendChild(empty);
    return;
  }
  if (!filtered.length) {
    const empty = document.createElement("div");
    empty.className = "gallery-empty";
    empty.textContent = "No cloud clips match that search.";
    root.appendChild(empty);
    return;
  }
  for (const entry of page.items) {
    const localClip = cloudLocalClipForEntry(entry);
    root.appendChild(localClip ? clipCard(localClip) : cloudClipCard(entry));
  }
}

function clearHeavyGalleryDom() {
  if (posterObserver) posterObserver.disconnect();
  posterRenderGeneration += 1;
  posterQueue = new WeakMap();
  posterWorkQueue = [];
  cloudThumbnailInflight.clear();
  for (const id of ["gallery-grid", "cloud-gallery-grid"]) {
    releaseGalleryRoot($(id));
  }
  syncGalleryPagination(null);
}

$("gallery-page-prev").addEventListener("click", () => changeGalleryPage(-1));
$("gallery-page-next").addEventListener("click", () => changeGalleryPage(1));

function showClipContextMenu(ev, clip) {
  ev.preventDefault();
  ev.stopPropagation();
  hideRegionMenu();
  clipContextTarget = clip;
  cloudContextTarget = null;
  gamePlayContextTarget = null;
  const record = clipCloudRecord(clip);
  const busy = record && ["queued", "uploading", "processing", "retrying"].includes(record.upload_status);
  const uploaded = cloudRecordUploaded(record);
  const shareable = !!cloudShareUrl(record);
  $("clip-menu-select").hidden = false;
  $("clip-menu-play").hidden = true;
  $("clip-menu-open-cloud-page").hidden = true;
  $("clip-menu-copy-cloud-link").hidden = true;
  $("clip-menu-export-play").hidden = true;
  $("clip-menu-copy").hidden = false;
  $("clip-menu-copy-shareable").hidden = false;
  $("clip-menu-remove-group").hidden = true;
  const upload = $("clip-menu-upload");
  upload.hidden = !cloudUploadControlVisible(uploaded);
  upload.textContent = shareable ? "Copy cloud link" : uploaded ? "Open cloud page" : "Upload";
  upload.disabled = busy || (uploaded ? !record.remote_clip_id : !cloudConnected());
  $("clip-menu-rename").hidden = false;
  $("clip-menu-rename-file").hidden = false;
  const favorite = $("clip-menu-favorite");
  favorite.hidden = false;
  favorite.textContent = clip.favorite ? "Remove from favorites" : "Add to favorites";
  $("clip-menu-delete").hidden = false;
  const menu = $("clip-context-menu");
  menu.hidden = false;
  positionContextMenu(menu, ev.clientX, ev.clientY);
}

function showGroupClipContextMenu(ev, clip) {
  ev.preventDefault();
  ev.stopPropagation();
  hideRegionMenu();
  clipContextTarget = clip;
  cloudContextTarget = null;
  gamePlayContextTarget = null;
  for (const id of [
    "clip-menu-select",
    "clip-menu-play",
    "clip-menu-open-cloud-page",
    "clip-menu-copy-cloud-link",
    "clip-menu-export-play",
    "clip-menu-upload",
    "clip-menu-rename",
    "clip-menu-rename-file",
    "clip-menu-copy",
    "clip-menu-copy-shareable",
  ]) $(id).hidden = true;
  $("clip-menu-remove-group").hidden = false;
  $("clip-menu-delete").hidden = false;
  const menu = $("clip-context-menu");
  menu.hidden = false;
  positionContextMenu(menu, ev.clientX, ev.clientY);
}

function showCloudClipContextMenu(ev, entry) {
  ev.preventDefault();
  ev.stopPropagation();
  hideRegionMenu();
  clipContextTarget = null;
  cloudContextTarget = entry;
  gamePlayContextTarget = null;
  $("clip-menu-select").hidden = true;
  $("clip-menu-play").hidden = false;
  $("clip-menu-play").disabled = false;
  $("clip-menu-open-cloud-page").hidden = false;
  $("clip-menu-open-cloud-page").disabled = !entry.remote_clip_id;
  $("clip-menu-copy-cloud-link").hidden = !cloudShareUrl(entry);
  $("clip-menu-copy-cloud-link").disabled = !cloudShareUrl(entry);
  $("clip-menu-export-play").hidden = true;
  $("clip-menu-copy").hidden = true;
  $("clip-menu-copy-shareable").hidden = true;
  $("clip-menu-remove-group").hidden = true;
  $("clip-menu-upload").hidden = true;
  $("clip-menu-rename").hidden = true;
  $("clip-menu-rename-file").hidden = true;
  $("clip-menu-favorite").hidden = true;
  $("clip-menu-delete").hidden = true;
  const menu = $("clip-context-menu");
  menu.hidden = false;
  positionContextMenu(menu, ev.clientX, ev.clientY);
}

function showGamePlayContextMenu(ev, play) {
  ev.preventDefault();
  ev.stopPropagation();
  hideRegionMenu();
  clipContextTarget = currentClip;
  cloudContextTarget = null;
  gamePlayContextTarget = play;
  $("clip-menu-select").hidden = true;
  $("clip-menu-play").hidden = true;
  $("clip-menu-open-cloud-page").hidden = true;
  $("clip-menu-copy-cloud-link").hidden = true;
  const exportPlay = $("clip-menu-export-play");
  exportPlay.hidden = false;
  exportPlay.disabled = !play || !play.range;
  exportPlay.textContent = "Export play as clip";
  $("clip-menu-copy").hidden = true;
  $("clip-menu-copy-shareable").hidden = true;
  $("clip-menu-remove-group").hidden = true;
  $("clip-menu-upload").hidden = true;
  $("clip-menu-rename").hidden = true;
  $("clip-menu-rename-file").hidden = true;
  $("clip-menu-favorite").hidden = true;
  $("clip-menu-delete").hidden = true;
  const menu = $("clip-context-menu");
  menu.hidden = false;
  positionContextMenu(menu, ev.clientX, ev.clientY);
}

function hideClipContextMenu() {
  const menu = $("clip-context-menu");
  if (menu) menu.hidden = true;
  clipContextTarget = null;
  cloudContextTarget = null;
  gamePlayContextTarget = null;
}

function clipContextRecord() {
  return clipContextTarget ? clipCloudRecord(clipContextTarget) : null;
}
async function bulkDeleteSelected() {
  const paths = [...selectedClipPaths];
  if (!paths.length) return;
  if (!(await confirmBulkDelete(paths.length))) return;
  try {
    const report = await invoke("delete_clips", { paths });
    await applyDeletion(report.deleted);
    const notice = deletionNotice(report.deleted.length);
    if (notice) setNotice(notice, { transient: true });
    $("error").textContent = formatDeletionFailures(report.failed);
    if (report.deleted.length > 0) exitSelectMode();
    else clearSelection();
  } catch (e) {
    $("error").textContent = String(e);
  }
}
