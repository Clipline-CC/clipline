// Settings: game plugins, review options, osu tabs.
function updateGamePluginSummary(plugin) {
  const summary = document.querySelector(`[data-game-plugin-summary="${plugin.id}"]`);
  if (summary) summary.textContent = gamePluginSummary(plugin);
}

function renderGamePluginModeControl(plugin, settings) {
  const control = document.createElement("div");
  control.className = "segmented-control game-profile-mode";
  control.setAttribute("role", "radiogroup");
  control.setAttribute("aria-label", `${plugin.name} recording mode`);
  [
    ["replays_only", "Replays only"],
    ["full_session", "Full session"],
  ].forEach(([value, label]) => {
    const option = document.createElement("label");
    const input = document.createElement("input");
    input.type = "radio";
    input.name = `game-plugin-mode-${plugin.id}`;
    input.value = value;
    input.checked = settings.recording_mode === value;
    input.addEventListener("change", () => {
      if (input.checked) {
        gamePluginSettings[plugin.id] = normalizeGamePluginSettings({
          ...gamePluginSetting(plugin),
          recording_mode: value,
        }, plugin);
        updateGamePluginSummary(plugin);
        updateGameDetectionStatus();
        syncGamePluginSettingsDraft();
      }
    });
    const text = document.createElement("span");
    text.textContent = label;
    option.append(input, text);
    control.appendChild(option);
  });
  return control;
}

const GAME_REVIEW_GROUPS = [
  {
    id: "match_events",
    label: "Match events",
    options: [
      ["user_kills", "User kills"],
      ["user_deaths", "User deaths"],
      ["user_assists", "User assists"],
      ["team_kills", "Ally kills"],
      ["team_deaths", "Ally deaths"],
      ["enemy_kills", "Enemy kills"],
      ["enemy_deaths", "Enemy deaths"],
      ["objectives", "Objectives"],
      ["turrets", "Structures"],
    ],
  },
  {
    id: "timeline_markers",
    label: "Timeline markers",
    options: [
      ["user_kills", "User kills"],
      ["user_deaths", "User deaths"],
      ["user_assists", "User assists"],
      ["objectives", "Objectives"],
      ["turrets", "Structures"],
    ],
  },
];

const GAME_REVIEW_OPTION_GROUPS = {
  match_events: [
    {
      label: "Your events",
      keys: ["user_kills", "user_deaths", "user_assists"],
    },
    {
      label: "Team fights",
      keys: ["team_kills", "team_deaths", "enemy_kills", "enemy_deaths"],
    },
    {
      label: "Map events",
      keys: ["objectives", "turrets"],
    },
  ],
  timeline_markers: [
    {
      label: "Your markers",
      keys: ["user_kills", "user_deaths", "user_assists"],
    },
    {
      label: "Map markers",
      keys: ["objectives", "turrets"],
    },
  ],
};

const GAME_PLUGIN_SETTINGS_TAB_DEFINITIONS = {
  general: { label: "General" },
  match_events: { label: "Match events", requiresEventMarkers: true },
  timeline_markers: { label: "Timeline markers", requiresEventMarkers: true },
  osu_account: { label: "Account", pluginIds: ["osu"] },
  osu_plays: { label: "Plays", pluginIds: ["osu"] },
};

const GAME_PLUGIN_SETTINGS_TABS = Object.keys(GAME_PLUGIN_SETTINGS_TAB_DEFINITIONS);

function gamePluginSettingsTabs(plugin) {
  if (!plugin) return ["general"];
  return GAME_PLUGIN_SETTINGS_TABS.filter((tab) => {
    const definition = GAME_PLUGIN_SETTINGS_TAB_DEFINITIONS[tab];
    if (definition.requiresEventMarkers && !plugin.event_markers) return false;
    return !definition.pluginIds || definition.pluginIds.includes(plugin.id);
  });
}

function gamePluginReviewGroupDefinition(groupId) {
  return GAME_REVIEW_GROUPS.find((group) => group.id === groupId) || GAME_REVIEW_GROUPS[0];
}

function gamePluginReviewOptionLabel(group, key) {
  const option = group.options.find(([optionKey]) => optionKey === key);
  return option ? option[1] : key;
}

function syncGamePluginReviewControls(plugin) {
  const settings = gamePluginSetting(plugin);
  const reviewEnabled = settings.review.enabled;
  const master = document.querySelector(`[data-game-plugin-review-enabled="${plugin.id}"]`);
  if (master) master.checked = reviewEnabled;
  const groups = document.querySelectorAll(`[data-game-plugin-review-group="${plugin.id}"]`);
  groups.forEach((group) => {
    const groupName = group.dataset.reviewGroup;
    const groupEnabled = Boolean(settings.review[groupName] && settings.review[groupName].enabled);
    group.classList.toggle("disabled", !reviewEnabled || !groupEnabled);
    group.querySelectorAll("input").forEach((input) => {
      if (input.dataset.reviewKey === "enabled") {
        input.disabled = !reviewEnabled;
      } else {
        input.disabled = !reviewEnabled || !groupEnabled;
      }
    });
  });
}

function updateGamePluginReviewSetting(plugin) {
  const existing = gamePluginSetting(plugin);
  gamePluginSettings[plugin.id] = normalizeGamePluginSettings({
    ...existing,
    review: readGamePluginReviewSettings(plugin, existing.review),
  }, plugin);
  syncGamePluginReviewControls(plugin);
  syncGamePluginSettingsDraft();
  refreshReviewForSettingsChange();
}

function renderReviewCheckbox(plugin, groupId, key, labelText, checked) {
  const label = document.createElement("label");
  label.className = "check-line";
  const input = document.createElement("input");
  input.type = "checkbox";
  input.checked = checked;
  input.dataset.gamePluginReviewSetting = plugin.id;
  input.dataset.reviewGroup = groupId;
  input.dataset.reviewKey = key;
  input.addEventListener("change", () => updateGamePluginReviewSetting(plugin));
  const text = document.createElement("span");
  text.textContent = labelText;
  label.append(input, text);
  return label;
}

function renderGamePluginOptionGroup(plugin, group, groupSettings, optionGroup) {
  const section = document.createElement("section");
  section.className = "game-review-option-group";
  const title = document.createElement("strong");
  title.textContent = optionGroup.label;

  const list = document.createElement("div");
  list.className = "game-review-option-list";
  for (const key of optionGroup.keys) {
    list.appendChild(renderReviewCheckbox(
      plugin,
      group.id,
      key,
      gamePluginReviewOptionLabel(group, key),
      groupSettings[key],
    ));
  }

  section.append(title, list);
  return section;
}

function renderGamePluginSettingsButton(plugin) {
  const button = document.createElement("button");
  button.type = "button";
  button.className = "game-profile-settings";
  button.dataset.gamePluginSettings = plugin.id;
  button.textContent = "Settings";
  button.setAttribute("aria-label", `${plugin.name} settings`);
  button.addEventListener("click", () => showGamePluginSettingsDialog(plugin));
  return button;
}

function renderGamePluginReviewGroup(plugin, groupId, review) {
  const group = gamePluginReviewGroupDefinition(groupId);
  const groupSettings = review[group.id];
  const section = document.createElement("section");
  section.className = "game-review-group";
  section.dataset.gamePluginReviewGroup = plugin.id;
  section.dataset.reviewGroup = group.id;

  const head = document.createElement("label");
  head.className = "check-line game-review-master-card game-review-group-head";
  const enabled = document.createElement("input");
  enabled.type = "checkbox";
  enabled.checked = groupSettings.enabled;
  enabled.dataset.gamePluginReviewSetting = plugin.id;
  enabled.dataset.reviewGroup = group.id;
  enabled.dataset.reviewKey = "enabled";
  enabled.addEventListener("change", () => updateGamePluginReviewSetting(plugin));
  const title = document.createElement("strong");
  title.textContent = group.label;
  head.append(enabled, title);

  const groups = document.createElement("div");
  groups.className = "game-review-option-groups";
  for (const optionGroup of GAME_REVIEW_OPTION_GROUPS[group.id] || []) {
    groups.appendChild(renderGamePluginOptionGroup(
      plugin,
      group,
      groupSettings,
      optionGroup,
    ));
  }

  section.append(head, groups);
  return section;
}

function renderGamePluginSettingsGeneralTab(plugin, settings) {
  const review = normalizeGamePluginReviewSettings(settings.review);
  const root = document.createElement("div");
  root.className = "game-plugin-settings-panel";

  const modeSection = document.createElement("section");
  modeSection.className = "game-plugin-settings-section";
  const modeTitle = document.createElement("strong");
  modeTitle.textContent = "Recording";
  modeSection.append(modeTitle, renderGamePluginModeControl(plugin, settings));

  root.append(modeSection);

  if (plugin.id === "osu") {
    const playsSection = document.createElement("section");
    playsSection.className = "game-plugin-settings-section";
    const playsTitle = document.createElement("strong");
    playsTitle.textContent = "Play blocks";
    const playsHint = document.createElement("span");
    playsHint.className = "hint";
    playsHint.textContent = "Recent submitted plays are fetched after a full-session recording is saved.";
    playsSection.append(playsTitle, playsHint);
    root.append(playsSection);
    return root;
  }

  const reviewSection = document.createElement("section");
  reviewSection.className = "game-plugin-settings-section";
  const master = document.createElement("label");
  master.className = "check-line game-review-master-card game-review-master";
  const masterInput = document.createElement("input");
  masterInput.type = "checkbox";
  masterInput.checked = review.enabled;
  masterInput.dataset.gamePluginReviewEnabled = plugin.id;
  masterInput.addEventListener("change", () => updateGamePluginReviewSetting(plugin));
  const masterText = document.createElement("span");
  masterText.textContent = "Show League match details";
  master.append(masterInput, masterText);
  reviewSection.append(master);

  if (plugin.id === "league_of_legends") {
    const gateSection = document.createElement("section");
    gateSection.className = "game-plugin-settings-section";
    const gateTitle = document.createElement("strong");
    gateTitle.textContent = "Record game types";
    const gateHint = document.createElement("span");
    gateHint.className = "hint";
    gateHint.textContent =
      "Automatic recording is skipped for unchecked game types. Manual recording always works.";
    gateSection.append(gateTitle, gateHint);
    for (const [key, label] of LEAGUE_MODE_RECORD_LABELS) {
      const check = document.createElement("label");
      check.className = "check-line";
      const input = document.createElement("input");
      input.type = "checkbox";
      input.checked = (leagueModeSettings || {})[key] !== false;
      input.dataset.leagueModeRecord = key;
      input.addEventListener("change", updateLeagueModeSetting);
      const text = document.createElement("span");
      text.textContent = label;
      check.append(input, text);
      gateSection.append(check);
    }
    root.append(gateSection);
  }

  root.append(reviewSection);
  return root;
}

function osuApiConnectionLabel(status = osuApiSettings()) {
  const name = status.username || status.last_connected_username;
  if (status.configured) return name ? `Connected as ${name}` : "Connected";
  if (status.secret_present) return "Saved; test the connection";
  if (status.client_id || status.user || status.credential_target) return "Client secret needed";
  return "Not configured";
}

function updateOsuApiSettingsFromStatus(status) {
  const next = {
    ...osuApiSettings(),
    client_id: status.client_id || null,
    user: status.user || null,
    credential_target: status.credential_target || null,
    last_connected_username: status.username || null,
  };
  if (currentSettings) currentSettings.osu = next;
  if (settingsDraft) settingsDraft.osu = { ...next };
}

function renderOsuApiField(labelText, input) {
  const label = document.createElement("label");
  label.className = "osu-api-field";
  const labelSpan = document.createElement("span");
  labelSpan.textContent = labelText;
  label.append(labelSpan, input);
  return label;
}

function osuApiRequestFromInputs(clientIdInput, clientSecretInput, userInput) {
  return {
    client_id: clientIdInput.value.trim(),
    client_secret: clientSecretInput.value.trim() || null,
    user: userInput.value.trim(),
  };
}

function syncOsuApiInputsFromStatus(clientIdInput, clientSecretInput, userInput, status) {
  if (status.client_id) clientIdInput.value = status.client_id;
  if (status.user) userInput.value = status.user;
  clientSecretInput.value = "";
  clientSecretInput.placeholder = status.secret_present
    ? "Leave blank to keep saved secret"
    : "Paste client secret";
}

async function saveOsuApiSettingsFromInputs(clientIdInput, clientSecretInput, userInput, status) {
  status.textContent = "Saving...";
  const result = await invoke("save_osu_api_settings", {
    request: osuApiRequestFromInputs(clientIdInput, clientSecretInput, userInput),
  });
  updateOsuApiSettingsFromStatus(result);
  syncOsuApiInputsFromStatus(clientIdInput, clientSecretInput, userInput, result);
  status.textContent = osuApiConnectionLabel(result);
  syncSettingsDraftFromForm();
  return result;
}

async function refreshOsuApiStatus(status) {
  try {
    const result = await invoke("osu_api_status");
    updateOsuApiSettingsFromStatus(result);
    status.textContent = osuApiConnectionLabel(result);
  } catch (e) {
    status.textContent = String(e);
  }
}

function renderOsuAccountSettingsTab() {
  const root = document.createElement("div");
  root.className = "game-plugin-settings-panel osu-account-panel";

  const accountSection = document.createElement("section");
  accountSection.className = "game-plugin-settings-section";
  const heading = document.createElement("div");
  heading.className = "osu-api-heading";
  const title = document.createElement("strong");
  title.textContent = "Account";
  const guide = document.createElement("button");
  guide.type = "button";
  guide.className = "osu-guide-button";
  guide.title = "Open osu! API setup guide";
  guide.setAttribute("aria-label", "Open osu! API setup guide");
  guide.textContent = "?";
  guide.addEventListener("click", async () => {
    try {
      await invoke("open_osu_api_setup_guide");
    } catch (e) {
      $("error").textContent = String(e);
    }
  });
  heading.append(title, guide);

  const osu = osuApiSettings();
  const hint = document.createElement("span");
  hint.className = "hint";
  hint.textContent = "Use your own osu! OAuth app. The client secret stays in Windows Credential Manager.";

  const fields = document.createElement("div");
  fields.className = "osu-api-fields";
  const clientId = document.createElement("input");
  clientId.type = "text";
  clientId.inputMode = "numeric";
  clientId.autocomplete = "off";
  clientId.placeholder = "Client ID";
  clientId.value = osu.client_id || "";
  const secret = document.createElement("input");
  secret.type = "password";
  secret.autocomplete = "off";
  secret.placeholder = osu.credential_target ? "Leave blank to keep saved secret" : "Paste client secret";
  const user = document.createElement("input");
  user.type = "text";
  user.autocomplete = "username";
  user.placeholder = "osu! User ID or Username";
  user.value = osu.user || "";
  fields.append(
    renderOsuApiField("Client ID", clientId),
    renderOsuApiField("Client Secret", secret),
    renderOsuApiField("osu! User ID or Username", user)
  );

  const actions = document.createElement("div");
  actions.className = "osu-account-actions";
  const save = document.createElement("button");
  save.type = "button";
  save.textContent = "Save";
  const test = document.createElement("button");
  test.type = "button";
  test.className = "primary";
  test.textContent = "Test osu! API connection";
  const status = document.createElement("span");
  status.className = "hint";
  status.textContent = osuApiConnectionLabel(osu);
  save.addEventListener("click", async () => {
    $("error").textContent = "";
    save.disabled = true;
    test.disabled = true;
    try {
      await saveOsuApiSettingsFromInputs(clientId, secret, user, status);
    } catch (e) {
      status.textContent = String(e);
    } finally {
      save.disabled = false;
      test.disabled = false;
    }
  });
  test.addEventListener("click", async () => {
    $("error").textContent = "";
    save.disabled = true;
    test.disabled = true;
    try {
      await saveOsuApiSettingsFromInputs(clientId, secret, user, status);
      status.textContent = "Testing...";
      const result = await invoke("test_osu_api_connection");
      updateOsuApiSettingsFromStatus(result.status);
      syncOsuApiInputsFromStatus(clientId, secret, user, result.status);
      const missing = result.pagination_ceiling_reached ? "; some plays may be missing" : "";
      status.textContent = `Connected. Recent scores: ${result.score_count}, failed: ${result.failed_count}${missing}`;
      await refresh();
    } catch (e) {
      status.textContent = String(e);
    } finally {
      save.disabled = false;
      test.disabled = false;
    }
  });
  actions.append(save, test, status);
  refreshOsuApiStatus(status);

  accountSection.append(heading, hint, fields, actions);
  root.append(accountSection);
  return root;
}

function renderOsuPlaysSettingsTab() {
  const root = document.createElement("div");
  root.className = "game-plugin-settings-panel";

  const playsSection = document.createElement("section");
  playsSection.className = "game-plugin-settings-section";
  const title = document.createElement("strong");
  title.textContent = "Plays";
  const list = document.createElement("div");
  list.className = "osu-play-settings-list";
  [
    "Recent submitted plays are fetched after a full-session recording is saved.",
    "Failed plays stay visible when osu! returns them; retries only appear if they were submitted.",
    "Some plays may be missing if osu!'s recent-score list reaches the 500 score ceiling.",
    "v1 tracks osu!standard only.",
  ].forEach((text) => {
    const item = document.createElement("div");
    item.textContent = text;
    list.appendChild(item);
  });
  playsSection.append(title, list);
  root.append(playsSection);
  return root;
}

function renderGamePluginSettingsMatchEventsTab(plugin, settings) {
  const root = document.createElement("div");
  root.className = "game-plugin-settings-panel";
  root.appendChild(renderGamePluginReviewGroup(
    plugin,
    "match_events",
    normalizeGamePluginReviewSettings(settings.review),
  ));
  return root;
}

function renderGamePluginSettingsTimelineMarkersTab(plugin, settings) {
  const root = document.createElement("div");
  root.className = "game-plugin-settings-panel";
  root.appendChild(renderGamePluginReviewGroup(
    plugin,
    "timeline_markers",
    normalizeGamePluginReviewSettings(settings.review),
  ));
  return root;
}

function renderGamePluginSettingsDialog(plugin = gamePluginSettingsDialogPlugin()) {
  if (!plugin) return;
  const settings = gamePluginSetting(plugin);
  const availableTabs = gamePluginSettingsTabs(plugin);
  const tab = availableTabs.includes(gamePluginSettingsDialogTab)
    ? gamePluginSettingsDialogTab
    : "general";
  gamePluginSettingsDialogTab = tab;
  gamePluginSettings[plugin.id] = settings;

  $("game-plugin-settings-title").textContent = `${plugin.name} settings`;
  $("game-plugin-settings-subtitle").textContent = "";
  document.querySelectorAll("[data-game-plugin-settings-tab]").forEach((button) => {
    button.hidden = !availableTabs.includes(button.dataset.gamePluginSettingsTab);
    const active = button.dataset.gamePluginSettingsTab === tab;
    button.classList.toggle("active", active);
    button.setAttribute("aria-selected", String(active));
    button.tabIndex = active ? 0 : -1;
  });

  const body = $("game-plugin-settings-body");
  if (tab === "match_events") {
    body.replaceChildren(renderGamePluginSettingsMatchEventsTab(plugin, settings));
  } else if (tab === "timeline_markers") {
    body.replaceChildren(renderGamePluginSettingsTimelineMarkersTab(plugin, settings));
  } else if (tab === "osu_account") {
    body.replaceChildren(renderOsuAccountSettingsTab(plugin, settings));
  } else if (tab === "osu_plays") {
    body.replaceChildren(renderOsuPlaysSettingsTab(plugin, settings));
  } else {
    body.replaceChildren(renderGamePluginSettingsGeneralTab(plugin, settings));
  }
  syncGamePluginReviewControls(plugin);
}

function showGamePluginSettingsDialog(plugin, tab = "general") {
  gamePluginSettingsDialogPluginId = plugin.id;
  const availableTabs = gamePluginSettingsTabs(plugin);
  gamePluginSettingsDialogTab = availableTabs.includes(tab) ? tab : "general";
  renderGamePluginSettingsDialog(plugin);
  const dialog = $("game-plugin-settings-dialog");
  if (!dialog.open) dialog.showModal();
}

function hideGamePluginSettingsDialog() {
  const dialog = $("game-plugin-settings-dialog");
  if (dialog.open) dialog.close();
  else gamePluginSettingsDialogPluginId = null;
}

function setGamePluginSettingsTab(tab) {
  const plugin = gamePluginSettingsDialogPlugin();
  if (!gamePluginSettingsTabs(plugin).includes(tab)) return;
  syncGamePluginSettingsDraft();
  gamePluginSettingsDialogTab = tab;
  renderGamePluginSettingsDialog();
}

function syncGamePluginCatalog(nextPlugins) {
  gamePlugins = Array.isArray(nextPlugins) ? nextPlugins : [];
  renderGamePlugins();
  if (gamePluginSettingsDialogPluginId && !gamePluginSettingsDialogPlugin()) {
    hideGamePluginSettingsDialog();
  } else if (gamePluginSettingsDialogPluginId) {
    renderGamePluginSettingsDialog();
  }
  updateGameDetectionStatus();
  if (clipsCache.length) renderClips();
  if (currentClip) {
    renderGameEventRail(currentClip);
    renderGameMetadataPanel(currentClip);
  }
}

function renderGamePlugins() {
  const root = $("supported-games");
  root.replaceChildren();
  if (!gamePlugins.length) {
    const empty = document.createElement("div");
    empty.className = "hint";
    empty.textContent = "no supported games available";
    root.appendChild(empty);
    syncSettingsChangeIndicators();
    return;
  }

  for (const plugin of gamePlugins) {
    const settings = gamePluginSetting(plugin);
    gamePluginSettings[plugin.id] = settings;

    const row = document.createElement("div");
    row.className = "game-profile supported";
    row.dataset.gamePluginId = plugin.id;
    row.dataset.settingsKey = `games.plugins.${plugin.id}`;

    const enabled = document.createElement("label");
    enabled.className = "check-line";
    const checkbox = document.createElement("input");
    checkbox.type = "checkbox";
    checkbox.checked = settings.enabled;
    checkbox.dataset.gamePluginEnabled = plugin.id;
    checkbox.addEventListener("change", () => {
      gamePluginSettings[plugin.id] = {
        ...gamePluginSetting(plugin),
        enabled: checkbox.checked,
      };
      updateGamePluginSummary(plugin);
      updateGameDetectionStatus();
      syncGamePluginSettingsDraft();
    });
    enabled.appendChild(checkbox);

    const icon = gameIconEl(plugin.icon, plugin.name);

    const meta = document.createElement("div");
    meta.className = "game-profile-meta";
    const name = document.createElement("strong");
    name.textContent = plugin.name;
    const summary = document.createElement("span");
    summary.dataset.gamePluginSummary = plugin.id;
    summary.textContent = gamePluginSummary(plugin, settings);
    meta.append(name, summary);

    row.append(
      enabled,
      icon,
      meta,
      renderGamePluginSettingsButton(plugin)
    );
    root.appendChild(row);
  }
  syncSettingsChangeIndicators();
}
