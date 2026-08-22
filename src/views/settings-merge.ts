import type { AppConfig } from "../api/tauri";

export const EDITABLE_SETTING_KEYS = [
  "gateway_port",
  "upstream_base_url",
  "proxy_mode",
  "proxy_url",
  "proxy_list_direction",
  "proxy_list_models",
  "opencode_invite_url",
  "client_root_url",
  "auto_start",
  "show_dock_icon",
  "connect_timeout_secs",
  "non_stream_timeout_secs",
  "stream_idle_timeout_secs",
  "routing_mode",
  "conversation_sticky",
  "free_model_routing",
] as const satisfies readonly (keyof AppConfig)[];

/**
 * Keep locally edited fields while adopting a newer server snapshot.
 * Revision, sub keys, environment flags, and capability flags always come
 * from the server and are intentionally excluded from the editable key list.
 */
export function mergeUnsavedSettings(
  latest: AppConfig,
  current: AppConfig,
  saved: AppConfig,
): AppConfig {
  const merged = { ...latest };
  for (const key of EDITABLE_SETTING_KEYS) {
    if (current[key] !== saved[key]) {
      Object.assign(merged, { [key]: current[key] });
    }
  }
  return merged;
}
