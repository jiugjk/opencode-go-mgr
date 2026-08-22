import assert from "node:assert/strict";
import test from "node:test";
import { normalizeProxyUrl, validateProxyList } from "./settings-proxy.ts";

test("manual proxy URL is normalized to an HTTP origin", () => {
  assert.equal(normalizeProxyUrl("manual", " http://127.0.0.1:7890/ "), "http://127.0.0.1:7890");
  assert.equal(normalizeProxyUrl("auto", ""), "");
  assert.equal(
    normalizeProxyUrl("auto", " http://127.0.0.1:7890/ "),
    "http://127.0.0.1:7890",
  );
});

test("manual proxy rejects missing, credentialed, and non-origin URLs", () => {
  for (const value of [
    "",
    "socks5://127.0.0.1:1080",
    "http://user:secret@127.0.0.1:7890",
    "http://127.0.0.1:7890/proxy",
  ]) {
    assert.throws(() => normalizeProxyUrl("manual", value));
  }
});

test("non-manual modes keep leftover invalid URLs instead of blocking save", () => {
  assert.equal(normalizeProxyUrl("auto", "socks5://127.0.0.1:1080"), "socks5://127.0.0.1:1080");
  assert.equal(normalizeProxyUrl("direct", "not a proxy"), "not a proxy");
});

test("list mode requires a proxy URL like manual mode", () => {
  assert.throws(() => normalizeProxyUrl("list", ""), /名单模式需要填写代理地址/);
  assert.throws(() => normalizeProxyUrl("list", "socks5://127.0.0.1:1080"));
  assert.equal(normalizeProxyUrl("list", " http://127.0.0.1:7890/ "), "http://127.0.0.1:7890");
});

test("list validation rejects empty selections and unknown ids", () => {
  const supported = ["gpt-5.6-luna", "grok-4.5"];
  assert.throws(() => validateProxyList("list", [], supported), /至少勾选一个模型/);
  assert.throws(
    () => validateProxyList("list", ["gpt-5.6-luna", "wildcard-*"], supported),
    /未知模型/,
  );
});

test("list validation trims and dedupes known ids", () => {
  const supported = ["gpt-5.6-luna", "grok-4.5"];
  assert.deepEqual(
    validateProxyList("list", [" gpt-5.6-luna ", "grok-4.5", "gpt-5.6-luna"], supported),
    ["gpt-5.6-luna", "grok-4.5"],
  );
});

test("non-list modes keep stored lists untouched", () => {
  assert.deepEqual(
    validateProxyList("auto", ["removed-model", " "], []),
    ["removed-model"],
  );
});
