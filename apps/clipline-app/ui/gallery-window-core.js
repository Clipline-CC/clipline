// Pure bounded-gallery pagination. Keep this DOM-free so large-library
// behavior can be exercised in Boa without constructing thousands of cards.
var GalleryWindowCore = (() => {
  const DEFAULT_PAGE_SIZE = 60;

  const pageSize = (value) => {
    const normalized = Math.floor(Number(value));
    return Number.isSafeInteger(normalized) && normalized > 0
      ? normalized
      : DEFAULT_PAGE_SIZE;
  };

  const itemCount = (value) => {
    const normalized = Math.floor(Number(value));
    return Number.isSafeInteger(normalized) && normalized > 0 ? normalized : 0;
  };

  const pageCount = (total, size) => {
    const count = itemCount(total);
    return count ? Math.ceil(count / pageSize(size)) : 0;
  };

  const clampPage = (page, total, size) => {
    const pages = pageCount(total, size);
    if (!pages) return 0;
    const normalized = Math.floor(Number(page));
    if (!Number.isFinite(normalized)) return 0;
    return Math.max(0, Math.min(pages - 1, normalized));
  };

  const makeState = (page, identity, size) => Object.freeze({
    page,
    identity: String(identity ?? ""),
    pageSize: size,
  });

  const initialState = (size = DEFAULT_PAGE_SIZE) =>
    makeState(0, "", pageSize(size));

  // A changed source/filter/group/data identity always returns to page one.
  // Otherwise preserve the current page, clamped to the supplied bounds.
  const updateState = (
    state,
    { identity = "", total = 0, pageSize: requestedSize } = {},
  ) => {
    const previous = state || initialState(requestedSize);
    const size = pageSize(requestedSize ?? previous.pageSize);
    const nextIdentity = String(identity ?? "");
    const changed = previous.identity !== nextIdentity || previous.pageSize !== size;
    return makeState(
      changed ? 0 : clampPage(previous.page, total, size),
      nextIdentity,
      size,
    );
  };

  const setPage = (state, requestedPage, total) => {
    const previous = state || initialState();
    return makeState(
      clampPage(requestedPage, total, previous.pageSize),
      previous.identity,
      previous.pageSize,
    );
  };

  const pageInfo = (state, total) => {
    const previous = state || initialState();
    const count = itemCount(total);
    const size = pageSize(previous.pageSize);
    const page = clampPage(previous.page, count, size);
    const pages = pageCount(count, size);
    const start = pages ? page * size : 0;
    const end = Math.min(count, start + size);
    return {
      page,
      pageCount: pages,
      pageSize: size,
      total: count,
      start,
      end,
      hasPrevious: page > 0,
      hasNext: page + 1 < pages,
    };
  };

  const windowItems = (items, state) => {
    const source = Array.isArray(items) ? items : [];
    const info = pageInfo(state, source.length);
    return {
      ...info,
      items: source.slice(info.start, info.end),
    };
  };

  // Preserve group boundaries inside the bounded item window. A group that
  // crosses a page boundary appears on both pages with only its visible slice;
  // totalCount lets the UI keep the full bucket count in its heading.
  const windowGroups = (groups, state) => {
    const source = Array.isArray(groups) ? groups : [];
    let total = 0;
    for (const group of source) {
      total += Array.isArray(group && group.items)
        ? group.items.length
        : Array.isArray(group && group.clips)
          ? group.clips.length
          : 0;
    }
    const info = pageInfo(state, total);
    const visibleGroups = [];
    let offset = 0;
    for (const group of source) {
      const items = Array.isArray(group && group.items)
        ? group.items
        : Array.isArray(group && group.clips)
          ? group.clips
          : [];
      const groupStart = offset;
      const groupEnd = groupStart + items.length;
      offset = groupEnd;
      const visibleStart = Math.max(info.start, groupStart);
      const visibleEnd = Math.min(info.end, groupEnd);
      if (visibleStart >= visibleEnd) continue;
      visibleGroups.push({
        label: group && Object.prototype.hasOwnProperty.call(group, "label")
          ? group.label
          : null,
        totalCount: items.length,
        startInGroup: visibleStart - groupStart,
        items: items.slice(visibleStart - groupStart, visibleEnd - groupStart),
      });
    }
    return {
      ...info,
      groups: visibleGroups,
    };
  };

  const cacheGet = (cache, key) => {
    if (!cache || !cache.has(key)) return undefined;
    const value = cache.get(key);
    cache.delete(key);
    cache.set(key, value);
    return value;
  };

  const cacheSet = (cache, key, value, requestedLimit) => {
    if (!cache) return [];
    const normalizedLimit = Math.floor(Number(requestedLimit));
    const limit = Number.isSafeInteger(normalizedLimit) && normalizedLimit > 0
      ? normalizedLimit
      : 1;
    const evicted = [];
    cache.delete(key);
    cache.set(key, value);
    while (cache.size > limit) {
      const oldest = cache.keys().next().value;
      cache.delete(oldest);
      evicted.push(oldest);
    }
    return evicted;
  };

  // Match PlayerCore.sameClipPath semantics while producing a Set-compatible
  // key: Windows paths compare case-insensitively with slash/device-prefix
  // normalization; any other path retains exact, case-sensitive identity.
  const clipPathKey = (path) => {
    const text = String(path || "").trim();
    if (!text) return "";
    let normalized = text.replace(/\//g, "\\");
    const lower = normalized.toLowerCase();
    if (lower.startsWith("\\\\?\\unc\\")) {
      normalized = "\\\\" + normalized.slice(8);
    } else if (lower.startsWith("\\\\?\\")) {
      normalized = normalized.slice(4);
    }
    if (/^[a-z]:\\/i.test(normalized) || normalized.startsWith("\\\\")) {
      return `windows:${normalized.toLowerCase()}`;
    }
    return `exact:${text}`;
  };

  return Object.freeze({
    DEFAULT_PAGE_SIZE,
    initialState,
    updateState,
    setPage,
    pageInfo,
    windowItems,
    windowGroups,
    cacheGet,
    cacheSet,
    clipPathKey,
  });
})();

globalThis.GalleryWindowCore = GalleryWindowCore;
