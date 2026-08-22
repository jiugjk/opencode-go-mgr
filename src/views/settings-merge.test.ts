import assert from "node:assert/strict";
import test from "node:test";
import type { AppConfig } from "../api/tauri.ts";
import { EDITABLE_SETTING_KEYS, mergeUnsavedSettings } from "./settings-merge.ts";

test("the primary key value is not an editable settings field", () => {
  assert.ok(!(EDITABLE_SETTING_KEYS as readonly string[]).includes("gateway_key"));
});

function config(overrides: Partial<AppConfig> = {}): AppConfig {
  return {
    revision: 1,
    gateway_port: 9042,
    gateway_key: "server-key-1",
    upstream_base_url: "https://opencode.ai/zen/go",
    proxy_mode: "auto",
    proxy_url: "",
    proxy_list_direction: "whitelist",
    proxy_list_models: [],
    proxy_supported_models: [],
    opencode_invite_url: "https://opencode.ai/go?ref=68XPB6NP8V",
    client_root_url: "",
    client_root_url_from_env: false,
    auto_start: false,
    auto_start_supported: true,
    show_dock_icon: true,
    dock_visibility_supported: false,
    connect_timeout_secs: 30,
    non_stream_timeout_secs: 900,
    stream_idle_timeout_secs: 300,
    routing_mode: "strict-priority",
    conversation_sticky: true,
    free_model_routing: "explicit",
    ...overrides,
  };
}

test("settings conflict merge preserves local edits and accepts unrelated remote edits", () => {
  const saved = config();
  const current = config({
    opencode_invite_url: "https://opencode.ai/invite/local",
    proxy_mode: "manual",
    proxy_url: "http://127.0.0.1:7890",
    connect_timeout_secs: 45,
  });
  const latest = config({
    revision: 2,
    gateway_port: 9142,
    non_stream_timeout_secs: 1_200,
  });

  const merged = mergeUnsavedSettings(latest, current, saved);

  assert.equal(merged.revision, 2);
  assert.equal(merged.opencode_invite_url, "https://opencode.ai/invite/local");
  assert.equal(merged.proxy_mode, "manual");
  assert.equal(merged.proxy_url, "http://127.0.0.1:7890");
  assert.equal(merged.connect_timeout_secs, 45);
  assert.equal(merged.gateway_port, 9142);
  assert.equal(merged.non_stream_timeout_secs, 1_200);
});

test("settings conflict merge adopts the server primary key and keeps other local edits", () => {
  const saved = config();
  const current = config({ gateway_key: "unpersisted-key", auto_start: true });
  const latest = config({
    revision: 3,
    auto_start_supported: false,
    client_root_url_from_env: true,
  });

  const merged = mergeUnsavedSettings(latest, current, saved);

  // gateway_key is no longer a settings-form field: an unsaved local
  // edit is discarded so a rotation on the Keys page wins. Capability
  // flags still come from the server; other editable fields stay local.
  assert.equal(merged.gateway_key, "server-key-1");
  assert.equal(merged.auto_start, true);
  assert.equal(merged.auto_start_supported, false);
  assert.equal(merged.client_root_url_from_env, true);
});

test("an untouched primary key value follows the server snapshot", () => {
  const saved = config();
  const current = config();
  const latest = config({ revision: 2, gateway_key: "server-key-2" });

  const merged = mergeUnsavedSettings(latest, current, saved);

  assert.equal(merged.gateway_key, "server-key-2");
});
