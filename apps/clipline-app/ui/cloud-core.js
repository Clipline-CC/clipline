// Pure Clipline Cloud request arbitration. Keep DOM- and Tauri-free for Boa tests.
var CloudCore = (() => {
  const accountKey = (cloud = {}) => [
    cloud.host_url || "",
    cloud.connected_user_id || "",
    cloud.credential_target || "",
  ].map(String).join("|");

  const createRequestGate = () => {
    let generation = 0;
    return {
      begin(key) {
        generation += 1;
        return { generation, accountKey: String(key || "") };
      },
      invalidate() {
        generation += 1;
        return generation;
      },
      isCurrent(request, key) {
        return !!request
          && request.generation === generation
          && request.accountKey === String(key || "");
      },
    };
  };

  const backendOwnedCloudFields = [
    "host_url",
    "public_url",
    "connected_user_id",
    "connected_username",
    "connected_display_name",
    "credential_target",
    "uploads",
  ];

  const mergeBackendCloudSettings = (localSettings = {}, backendSettings = {}) => {
    const backendCloud = backendSettings.cloud || {};
    const cloud = { ...(localSettings.cloud || {}) };
    for (const field of backendOwnedCloudFields) {
      cloud[field] = field === "uploads"
        ? { ...(backendCloud.uploads || {}) }
        : (backendCloud[field] ?? null);
    }
    return { ...localSettings, cloud };
  };

  const plainHttpConfirmed = (activeOrigin, confirmedOrigin, checked) => (
    Boolean(activeOrigin)
    && Boolean(checked)
    && String(activeOrigin) === String(confirmedOrigin || "")
  );

  const recordUploaded = (record) => !!(
    record
    && record.remote_clip_id
    && String(record.upload_status || "").startsWith("uploaded_")
  );

  const shareUrl = (record) => {
    if (!recordUploaded(record) || String(record.visibility || "") === "private") return "";
    return String(record.remote_url || "");
  };

  const progressRenderFields = [
    "local_clip_id",
    "path",
    "remote_clip_id",
    "remote_url",
    "visibility",
    "upload_status",
    "error",
  ];

  const progressValue = (current, progress, field, fallback) => (
    Object.prototype.hasOwnProperty.call(progress, field)
      ? progress[field]
      : (current[field] ?? fallback)
  );

  const reconcileUploadProgress = (
    current = {},
    progress = {},
    defaultVisibility = "private",
    nowUnix = 0,
  ) => {
    const record = {
      local_clip_id: String(progressValue(current, progress, "local_clip_id", "") || ""),
      path: String(progressValue(current, progress, "path", "") || ""),
      remote_clip_id: progressValue(current, progress, "remote_clip_id", null),
      remote_url: progressValue(current, progress, "remote_url", null),
      visibility: String(current.visibility || defaultVisibility || "private"),
      upload_status: String(progressValue(
        current,
        progress,
        "upload_status",
        "not_uploaded",
      ) || "not_uploaded"),
      error: progressValue(current, progress, "error", null),
    };
    const renderRequired = progressRenderFields.some((field) => current[field] !== record[field]);
    record.updated_at_unix = renderRequired
      ? Math.max(0, Math.floor(Number(nowUnix) || 0))
      : Math.max(0, Math.floor(Number(current.updated_at_unix) || Number(nowUnix) || 0));
    return { record, renderRequired };
  };

  return {
    accountKey,
    createRequestGate,
    mergeBackendCloudSettings,
    plainHttpConfirmed,
    recordUploaded,
    reconcileUploadProgress,
    shareUrl,
  };
})();

globalThis.CloudCore = CloudCore;
