// Screenshots Gallery: a dedicated view for PNG screenshots, separate from
// the clip Library. Screenshots live in <media root>\Screenshots; the main
// Library keeps excluding them, and this view lists only them.
(function () {
  var active = false;

  function shotCard(clip, index, allShots) {
    var el = document.createElement("article");
    el.className = "card screenshot-card";
    el.dataset.clipPath = clip.path;
    el.title = clip.name;

    var thumb = document.createElement("div");
    thumb.className = "card-thumb";
    thumb.style.cssText = thumbGradient(clip);
    observePoster(clip.path, thumb);

    var kindChip = document.createElement("span");
    kindChip.className = "card-kind screenshot";
    kindChip.innerHTML = CLIP_KIND_ICONS.screenshot; // static markup, safe
    kindChip.appendChild(kindLabelFor("Screenshot"));
    thumb.appendChild(kindChip);

    var del = document.createElement("button");
    del.className = "card-del";
    del.title = "Delete screenshot";
    del.innerHTML =
      '<svg viewBox="0 0 24 24"><path d="M9 3v1H4v2h16V4h-5V3H9zM6 8v11a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2V8H6zm3 2h2v9H9v-9zm4 0h2v9h-2v-9z"/></svg>';
    del.addEventListener("click", function (ev) {
      ev.stopPropagation();
      deleteClip(clip.path);
    });
    thumb.appendChild(del);

    var meta = document.createElement("div");
    meta.className = "card-meta";
    var name = document.createElement("span");
    name.className = "t";
    name.textContent = clip.name;
    meta.appendChild(name);
    var sub = document.createElement("div");
    sub.className = "card-sub";
    sub.textContent = fmtMegabytes(clip.size_mb)
      + " · " + fmtAgo(Date.now() / 1000, clip.modified_unix);
    meta.appendChild(sub);

    el.append(thumb, meta);
    el.addEventListener("click", function () {
      // Arrows navigate the Gallery's current sorted order.
      openScreenshotLightbox(clip, { items: allShots, index: index });
    });
    el.addEventListener("contextmenu", function (ev) {
      // Stills get their own item set: no Upload / Rename / shareable export.
      showScreenshotContextMenu(ev, clip);
    });
    return el;
  }

  function kindLabelFor(label) {
    var span = document.createElement("span");
    span.className = "card-kind-label";
    span.textContent = label;
    return span;
  }

  function sortedScreenshots() {
    var sort = $("screenshots-sort").value;
    return clipsCache
      .filter(function (c) { return PlayerCore.clipKind(c) === "screenshot"; })
      .sort(function (a, b) {
        return sort === "old"
          ? a.modified_unix - b.modified_unix
          : b.modified_unix - a.modified_unix;
      });
  }

  function renderScreenshots() {
    if (!active) return;
    var root = $("screenshots-grid");
    if (!root) return;
    releaseGalleryRoot(root);
    var shots = sortedScreenshots();
    $("screenshots-count").textContent = shots.length
      ? shots.length + (shots.length === 1 ? " screenshot" : " screenshots")
      : "";
    if (!shots.length) {
      var empty = document.createElement("div");
      empty.className = "gallery-empty";
      empty.textContent = "No screenshots yet — press the screenshot hotkey to capture one.";
      root.appendChild(empty);
      return;
    }
    for (var i = 0; i < shots.length; i++) root.appendChild(shotCard(shots[i], i, shots));
  }

  function setActive(next) {
    if (active === next) return;
    active = next;
    $("rail-gallery").classList.toggle("active", next);
    // Visibility itself is owned by updateViews (review-player.js); calling
    // it here is what actually swaps the Library out for the Gallery.
    updateViews();
    if (next) renderScreenshots();
  }

  // Re-render on demand when clips refresh while Gallery is open; called
  // from refreshClips in library.js.
  window.__renderScreenshots = renderScreenshots;

  // Review opening must close Gallery; view arbitration is centralized in
  // updateViews (review-player.js), which hides this view via currentClip.
  window.__screenshotsGalleryActive = function () {
    return active;
  };

  $("rail-gallery").addEventListener("click", function () {
    if (!$("review-viewer").hidden) closeReview();
    setActive(!active);
  });

  $("screenshots-sort").addEventListener("change", renderScreenshots);
})();
