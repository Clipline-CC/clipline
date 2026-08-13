// Pure Library search-token matching. Keep this DOM-free so prefix/value
// suggestions can be exercised in Boa without constructing the gallery.
var GallerySearchCore = (() => {
  const LOL_TYPE = Object.freeze({
    key: "lol-type",
    chipLabel: "LoL Type",
    prefixes: Object.freeze(["lol type", "loltype"]),
    hint: "Ranked, ARAM, Replay, …",
    values: Object.freeze([
      Object.freeze({ value: "ranked-solo-duo", label: "Ranked Solo/Duo" }),
      Object.freeze({ value: "ranked-flex", label: "Ranked Flex" }),
      Object.freeze({ value: "normal", label: "Normal" }),
      Object.freeze({ value: "aram", label: "ARAM" }),
      Object.freeze({ value: "arena", label: "Arena" }),
      Object.freeze({ value: "custom", label: "Custom" }),
      Object.freeze({ value: "replay", label: "Replay" }),
      Object.freeze({ value: "other", label: "Other" }),
      Object.freeze({ value: "unknown", label: "Unknown" }),
    ]),
  });

  const FILTERS = Object.freeze([LOL_TYPE]);

  const normalize = (text) => String(text || "").trim().toLowerCase().replace(/\s+/g, " ");

  const filterByKey = (key) => FILTERS.find((item) => item.key === key) || null;

  const inspect = (text) => {
    const trimmed = String(text || "").replace(/^\s+/, "");
    const lower = trimmed.toLowerCase();
    if (!trimmed) {
      return { kind: "empty", filterKey: null, valueDraft: "", remainder: "" };
    }
    for (const filter of FILTERS) {
      for (const prefix of filter.prefixes) {
        const colon = `${prefix}:`;
        if (lower.startsWith(colon)) {
          return {
            kind: "values",
            filterKey: filter.key,
            valueDraft: trimmed.slice(colon.length).replace(/^\s+/, ""),
            remainder: "",
          };
        }
        if (prefix.startsWith(lower) || (lower.startsWith(prefix) && lower.length <= prefix.length + 1)) {
          return {
            kind: "filters",
            filterKey: filter.key,
            valueDraft: "",
            remainder: trimmed,
          };
        }
      }
    }
    return { kind: "query", filterKey: null, valueDraft: "", remainder: trimmed };
  };

  const matchingFilters = (text) => {
    const state = inspect(text);
    if (state.kind === "empty" || state.kind === "filters") return FILTERS.slice();
    return [];
  };

  const matchingValues = (filterKey, draft, presentValues) => {
    const filter = filterByKey(filterKey);
    if (!filter) return [];
    const present = Array.isArray(presentValues) ? presentValues : null;
    const query = normalize(draft);
    return filter.values.filter((item) => {
      if (present && present.length && !present.includes(item.value)) return false;
      if (!query) return true;
      const label = item.label.toLowerCase();
      return label === query
        || item.value === query
        || label.startsWith(query)
        || item.value.startsWith(query)
        || label.split(/[\s/]+/).some((part) => part.startsWith(query));
    });
  };

  const valueById = (filterKey, value) => {
    const filter = filterByKey(filterKey);
    if (!filter) return null;
    return filter.values.find((item) => item.value === value) || null;
  };

  const chipText = (filterKey, value) => {
    const filter = filterByKey(filterKey);
    if (!filter) return "";
    if (!value) return `${filter.chipLabel}:`;
    const item = valueById(filterKey, value);
    return item ? `${filter.chipLabel}: ${item.label}` : `${filter.chipLabel}:`;
  };

  return Object.freeze({
    FILTERS,
    LOL_TYPE_KEY: LOL_TYPE.key,
    inspect,
    matchingFilters,
    matchingValues,
    valueById,
    chipText,
    filterByKey,
  });
})();
