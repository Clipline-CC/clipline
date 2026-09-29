// Library: cloud/local clip cards and posters.
function clipGalleryCardPreview(clip, kind, fallbackTitle) {
  const presentation = pluginPresentationForClip(clip);
  return galleryCardPreview(
    clip,
    kind,
    fallbackTitle,
    presentation,
    { data_dragon: presentation && presentation.data_dragon },
  );
}

function cloudVisibilityEl(record) {
  const visibility = clipCloudVisibility(record);
  if (!visibility) return null;
  const el = document.createElement("span");
  el.className = `clip-cloud-visibility ${visibility}`;
  el.title = CLOUD_VISIBILITY_LABELS[visibility];
  el.setAttribute("aria-label", CLOUD_VISIBILITY_LABELS[visibility]);
  el.innerHTML = CLOUD_VISIBILITY_ICONS[visibility]; // static markup, safe
  return el;
}

const CLOUD_CARD_ICON =
  '<svg viewBox="0 0 24 24" aria-hidden="true"><path d="M7.2 18h10.2a4.1 4.1 0 0 0 .4-8.2A6.2 6.2 0 0 0 5.9 8.1 5 5 0 0 0 7.2 18zm.2-2a3 3 0 0 1-.5-5.9l.8-.1.3-.8A4.2 4.2 0 0 1 16 10.4l.2 1.2 1.2.1A2.1 2.1 0 0 1 17.4 16H7.4z"/></svg>';

function cloudClipCard(entry) {
  const el = document.createElement("article");
  el.className = "card cloud-card";
  el.title = entry.title;

  const thumb = document.createElement("div");
  thumb.className = "card-thumb";
  thumb.style.cssText = thumbGradient({ name: entry.title, session: entry.remote_clip_id });
  const placeholder = document.createElement("span");
  placeholder.className = "cloud-card-placeholder";
  placeholder.innerHTML = CLOUD_CARD_ICON; // static markup, safe
  thumb.appendChild(placeholder);
  observeCloudThumbnail(entry, thumb);

  const play = document.createElement("div");
  play.className = "card-play";
  play.innerHTML = '<svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg>'; // static markup, safe

  const kindChip = document.createElement("span");
  kindChip.className = "card-kind session";
  kindChip.title = "Cloud clip";
  kindChip.innerHTML =
    '<svg viewBox="0 0 24 24"><path d="M7.2 18h10.2a4.1 4.1 0 0 0 .4-8.2A6.2 6.2 0 0 0 5.9 8.1 5 5 0 0 0 7.2 18z"/></svg>';
  const kindLabel = document.createElement("span");
  kindLabel.className = "card-kind-label";
  kindLabel.textContent = "Cloud";
  kindChip.appendChild(kindLabel);
  thumb.appendChild(kindChip);

  const meta = document.createElement("div");
  meta.className = "card-meta";
  const nameRow = document.createElement("div");
  nameRow.className = "card-name";
  const name = document.createElement("span");
  name.className = "t";
  name.textContent = entry.title;
  nameRow.appendChild(name);
  const visibility = cloudVisibilityEl(entry);
  if (visibility) nameRow.appendChild(visibility);

  const info = document.createElement("div");
  info.className = "card-sub";
  const updated = entry.updated_at_unix ? fmtAgo(Date.now() / 1000, entry.updated_at_unix) : "";
  const parts = [cloudStatusLabel(entry.upload_status)];
  if (updated) parts.push(updated);
  parts.push(entry.local_available ? "local copy available" : "cloud only");
  info.textContent = parts.join(" · ");

  const localState = document.createElement("div");
  localState.className = "cloud-local-state";
  localState.textContent = entry.remote_url || "No public share link";
  localState.title = entry.remote_url || "This clip is not publicly shareable.";

  meta.append(nameRow, info, localState);
  thumb.appendChild(play);
  el.append(thumb, meta);
  el.addEventListener("click", () => openCloudEntryInApp(entry));
  el.addEventListener("contextmenu", (ev) => showCloudClipContextMenu(ev, entry));
  return el;
}

// Clip names come from disk; build rows with textContent, never innerHTML.
const CARD_KIND_LABELS = { replay: "Replay", session: "Session", trim: "Trim", compilation: "Compilation" };
// Marker categories → tint var, matching the timeline glyph colors.
const MARKER_CATEGORY_TICK_VARS = {
  kill: "--mc-kill",
  assist: "--mc-assist",
  death: "--mc-death",
  spree: "--mc-spree",
  objective: "--mc-objective",
  structure: "--mc-structure",
  info: "--mc-info",
};
const MARKER_TICK_VARS = {
  ChampionKill: "--mc-kill", FirstBlood: "--mc-kill",
  ChampionAssist: "--mc-assist",
  ChampionDeath: "--mc-death",
  Multikill: "--mc-spree", Ace: "--mc-spree",
  DragonKill: "--mc-objective", HeraldKill: "--mc-objective", BaronKill: "--mc-objective",
  TurretKilled: "--mc-structure", InhibKilled: "--mc-structure", FirstBrick: "--mc-structure",
};

// Stable gradient placeholder per clip, shown until the poster loads (and the
// fallback if poster extraction fails).
function thumbGradient(c) {
  const key = (c.name || "") + (c.session || "");
  let h = 0;
  for (let i = 0; i < key.length; i++) h = (h * 31 + key.charCodeAt(i)) % 360;
  return `--g1:hsl(${h} 30% 18%); --g2:hsl(${(h + 38) % 360} 34% 8%);`;
}

function insertThumbMedia(thumb, media) {
  const firstOverlay = thumb.querySelector(".card-play, .card-kind, .card-dur, .card-markers, .card-del");
  thumb.insertBefore(media, firstOverlay || null);
}

function makePosterImg(url, onError = null) {
  const img = document.createElement("img");
  img.className = "card-thumb-img";
  img.src = url;
  img.alt = "";
  img.addEventListener("error", () => {
    img.remove();
    if (onError) onError();
  });
  return img;
}

function markPosterUnavailable(path) {
  posterCacheSet(path, POSTER_UNAVAILABLE);
}

function showPosterRuntimeWarning(error) {
  const warning = $("poster-runtime-warning");
  if (warning) warning.hidden = false;
  if (posterRuntimeWarningReported) return;
  posterRuntimeWarningReported = true;
  reportFrontendDiagnostic("warn", "poster_ffmpeg_unavailable", error);
}

function clearPosterRuntimeWarning() {
  const warning = $("poster-runtime-warning");
  if (warning) warning.hidden = true;
}

function retryUnavailablePosters() {
  for (const [key, value] of [...posterCache.entries()]) {
    if (
      value === POSTER_UNAVAILABLE
      && !String(key).startsWith(CLOUD_POSTER_CACHE_PREFIX)
    ) {
      posterCacheDelete(key);
    }
  }
  clearPosterRuntimeWarning();
  posterRuntimeWarningReported = false;
  renderClips();
}

// Lazily fetch + cache a clip's poster, then drop it into its card thumbnail.
// The backend caches the JPEG, so repeat calls are cheap after the first.
function loadCardPoster(path, thumb) {
  const lifecycleWork = captureForegroundWork();
  if (!lifecycleWork) return Promise.resolve();
  const cached = posterCacheGet(path);
  if (cached === POSTER_UNAVAILABLE) return Promise.resolve();
  if (cached) {
    if (thumb.isConnected && !thumb.querySelector(".card-thumb-img")) {
      insertThumbMedia(thumb, makePosterImg(cached, () => markPosterUnavailable(path)));
    }
    return Promise.resolve();
  }
  return invoke("clip_poster", { path })
    .then((posterPath) => {
      if (!isForegroundWorkCurrent(lifecycleWork)) return;
      if (!localClipPaths(clipsCache).has(GalleryWindowCore.clipPathKey(path))) return;
      if (!posterPath) {
        markPosterUnavailable(path);
        return;
      }
      const url = convertFileSrc(posterPath);
      posterCacheSet(path, url);
      if (thumb.isConnected && !thumb.querySelector(".card-thumb-img")) {
        insertThumbMedia(thumb, makePosterImg(url, () => markPosterUnavailable(path)));
      }
    })
    .catch((error) => {
      if (!isForegroundWorkCurrent(lifecycleWork)) return;
      markPosterUnavailable(path);
      if (GalleryWindowCore.posterRuntimeUnavailable(error)) {
        showPosterRuntimeWarning(error);
      }
    });
}

// Extracting a poster is an ffmpeg spawn, so we only request one once its card
// scrolls near the viewport — otherwise a library of hundreds of clips would
// queue an extraction for every clip on the first render and peg CPU/disk.
var posterQueue = new WeakMap();
var cloudThumbnailInflight = new Map();
var posterWorkQueue = [];
var posterWorkActive = 0;
var posterRenderGeneration = 0;
const POSTER_WORK_LIMIT = 2;

function pumpPosterWork() {
  while (posterWorkActive < POSTER_WORK_LIMIT && posterWorkQueue.length) {
    const job = posterWorkQueue.shift();
    if (
      job.generation !== posterRenderGeneration
      || !job.thumb.isConnected
      || !captureForegroundWork()
    ) {
      continue;
    }
    posterWorkActive += 1;
    const work = job.request.type === "local-poster"
      ? loadCardPoster(job.request.path, job.thumb)
      : loadCloudThumbnail(job.request.entry, job.thumb);
    Promise.resolve(work)
      .catch(() => {})
      .finally(() => {
        posterWorkActive -= 1;
        pumpPosterWork();
      });
  }
}

function queuePosterWork(request, thumb) {
  if (!request || !thumb || !thumb.isConnected) return;
  posterWorkQueue.push({
    request,
    thumb,
    generation: posterRenderGeneration,
  });
  pumpPosterWork();
}

var posterObserver =
  typeof IntersectionObserver === "function"
    ? new IntersectionObserver(
        (entries, obs) => {
          for (const entry of entries) {
            if (!entry.isIntersecting) continue;
            const thumb = entry.target;
            obs.unobserve(thumb);
            const request = posterQueue.get(thumb);
            posterQueue.delete(thumb);
            queuePosterWork(request, thumb);
          }
        },
        { rootMargin: "400px 0px" },
      )
    : null;

// Request a clip's poster when its thumbnail nears the viewport — or right away
// when IntersectionObserver is unavailable.
function observePoster(path, thumb) {
  if (!captureForegroundWork()) return;
  const request = { type: "local-poster", path };
  if (!posterObserver) {
    Promise.resolve().then(() => queuePosterWork(request, thumb));
    return;
  }
  posterQueue.set(thumb, request);
  posterObserver.observe(thumb);
}

function clipCard(c) {
  const el = document.createElement("article");
  const selected = selectedClipPaths.has(c.path);
  el.className = "card"
    + (currentClip && currentClip.path === c.path ? " active" : "")
    + (selected ? " selected" : "");
  el.dataset.clipPath = c.path;
  el.title = clipDisplayTitle(c) || c.name;
  const cloudRecord = clipCloudRecord(c);
  const uploadBusy = cloudRecord
    && ["queued", "uploading", "processing", "retrying"].includes(cloudRecord.upload_status);

  const kind = clipKind(c);
  const when = new Date(c.modified_unix * 1000);
  const markers = clipMarkers(c);
  const presentation = pluginPresentationForClip(c);
  const duration = Number.isFinite(c.duration_s)
    ? c.duration_s
    : (c.markers ? c.markers.duration_s : NaN);
  const fallbackTitle = formatClipTitle(
    when.getMonth(), when.getDate(), when.getHours(), when.getMinutes());
  const cardPreview = clipGalleryCardPreview(c, kind, fallbackTitle);
  const cardTitleUsesSummary = cardPreview.titleSource === "summary";
  const cardTitle = cardPreview.title || fallbackTitle;

  // Thumbnail: gradient placeholder + lazily-loaded poster, with the kind chip,
  // a hover delete, a play glyph, the duration, and marker ticks layered on.
  const thumb = document.createElement("div");
  thumb.className = "card-thumb";
  thumb.style.cssText = thumbGradient(c);
  // Cached and uncached posters share the same viewport gate so revisiting a
  // page does not immediately decode every image in that page.
  observePoster(c.path, thumb);

  const play = document.createElement("div");
  play.className = "card-play";
  play.innerHTML = '<svg viewBox="0 0 24 24"><path d="M8 5v14l11-7z"/></svg>'; // static markup, safe

  const kindChip = document.createElement("span");
  kindChip.className = "card-kind " + kind;
  kindChip.title = CLIP_KIND_LABELS[kind];
  kindChip.innerHTML = CLIP_KIND_ICONS[kind]; // static markup, safe
  const kindLabel = document.createElement("span");
  kindLabel.className = "card-kind-label";
  kindLabel.textContent = CARD_KIND_LABELS[kind];
  kindChip.appendChild(kindLabel);

  const del = document.createElement("button");
  del.className = "card-del";
  del.title = "Delete clip";
  // Static markup, no clip data — innerHTML is safe here.
  del.innerHTML =
    '<svg viewBox="0 0 24 24"><path d="M9 3v1H4v2h16V4h-5V3H9zM6 8v11a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2V8H6zm3 2h2v9H9v-9zm4 0h2v9h-2v-9z"/></svg>';

  // Favorite star sits left of the trash so it can be toggled without opening
  // the clip; filled when favorited, outline otherwise.
  const fav = document.createElement("button");
  fav.type = "button";
  fav.className = "card-fav-toggle" + (c.favorite ? " on" : "");
  fav.title = c.favorite ? "Remove from favorites" : "Add to favorites";
  fav.setAttribute("aria-pressed", String(!!c.favorite));
  fav.innerHTML = c.favorite
    ? '<svg viewBox="0 0 24 24"><path d="M12 2.6l3 5.9 6.5 1-4.8 4.5 1.1 6.5L12 17.3 6.2 20.6l1.1-6.5L2.5 9.6l6.5-1z"/></svg>'
    : '<svg class="off" viewBox="0 0 24 24"><path d="M12 2.6l3 5.9 6.5 1-4.8 4.5 1.1 6.5L12 17.3 6.2 20.6l1.1-6.5L2.5 9.6l6.5-1z"/></svg>';
  fav.addEventListener("click", (ev) => {
    ev.stopPropagation();
    toggleClipFavorite(c);
  });

  thumb.append(play, kindChip, fav, del);
  if (Number.isFinite(duration)) {
    const dur = document.createElement("span");
    dur.className = "card-dur";
    dur.textContent = fmtDur(duration);
    thumb.appendChild(dur);
  }

  if (Number.isFinite(duration) && duration > 0 && markers.length) {
    const strip = document.createElement("div");
    strip.className = "card-markers";
    for (const m of markers) {
      const tick = document.createElement("i");
      tick.style.left = Math.max(0, Math.min(100, (m.t_s / duration) * 100)) + "%";
      const style = markerStyle(m.kind, presentation);
      const tint = MARKER_CATEGORY_TICK_VARS[style.cls] || MARKER_TICK_VARS[m.kind];
      if (tint) tick.style.setProperty("--mc", `var(${tint})`);
      strip.appendChild(tick);
    }
    thumb.appendChild(strip);
  }

  const meta = document.createElement("div");
  meta.className = "card-meta";
  const nameRow = document.createElement("div");
  nameRow.className = "card-name";
  const previewIcon = cardPreview.icon && cardPreview.icon.url ? cardPreview.icon : null;
  const game = clipGameIcon(c);
  const cardIcon = previewIcon || (game ? { type: "game", url: game.url, label: game.label } : null);
  if (cardIcon) {
    const gi = document.createElement("img");
    gi.className = "card-game-ico" + (cardIcon.type === "portrait" ? " portrait" : "");
    gi.src = cardIcon.url;
    gi.alt = "";
    gi.title = cardIcon.label || (game ? game.label : "");
    // Fall back to a neutral glyph if the icon can't load.
    gi.addEventListener("error", () => {
      if (previewIcon && game && gi.src !== game.url) {
        gi.className = "card-game-ico";
        gi.src = game.url;
        gi.title = game.label;
        return;
      }
      const ph = document.createElement("div");
      ph.className = "card-game-ico placeholder";
      ph.title = cardIcon.label || (game ? game.label : "");
      ph.innerHTML = GENERIC_GAME_ICON; // static markup, safe
      gi.replaceWith(ph);
    });
    nameRow.appendChild(gi);
  }
  const name = document.createElement("span");
  name.className = "t";
  name.textContent = cardTitle;
  nameRow.appendChild(name);
  if (uploadBusy) {
    const spinner = document.createElement("span");
    spinner.className = "clip-upload-spinner";
    spinner.title = "Uploading clip";
    nameRow.appendChild(spinner);
  }
  const cloudVisibility = cloudVisibilityEl(cloudRecord);
  if (cloudVisibility) nameRow.appendChild(cloudVisibility);

  const info = document.createElement("div");
  info.className = "card-sub";
  const digest = markerDigest(markers, presentation);
  const infoParts = libraryItemMeta(duration, c.size_mb, c.modified_unix);
  if (c.game && c.game.queue && c.game.queue.label) infoParts.push(c.game.queue.label);
  if (!cardPreview.summary && digest) infoParts.push(digest);
  info.textContent = infoParts.join(" · ");

  meta.append(nameRow, info);
  if (cardPreview.summary && !cardTitleUsesSummary) {
    const detail = document.createElement("div");
    detail.className = "game-meta";
    detail.textContent = cardPreview.summary;
    meta.appendChild(detail);
  }

  el.append(thumb, meta);

  // Clicking the open clip's card again closes it (back to the gallery).
  el.addEventListener("click", () => {
    if (selectMode && gallerySource === "local") {
      toggleClipSelection(c.path);
      return;
    }
    if (currentClip && currentClip.path === c.path) closeReview();
    else openClip(c);
  });
  el.addEventListener("contextmenu", (ev) => showClipContextMenu(ev, c));
  del.addEventListener("click", (ev) => {
    ev.stopPropagation();
    deleteClip(c.path);
  });

  return el;
}
