// Generic authed gateway access for GUI data surfaces.
//
// Thin wrapper over the `gateway_fetch` Tauri command (src-tauri
// sol/commands.rs) — resolves the desktop JWT Rust-side and forwards
// to the gateway's `/api/*` routes. Use this for endpoints that have
// no bespoke IPC command (PnL windows, copy-trade summary, preset
// detail, …); established surfaces keep their typed commands in
// `ipc.ts`.
//
// NOTE on numbers: the gateway serialises Decimals as JSON *strings*
// (rust_decimal serde default). Parse with `Number(...)` per field.

import { invoke } from "@tauri-apps/api/core";

export type GatewayMethod = "GET" | "POST" | "PATCH" | "PUT" | "DELETE";

export async function gatewayFetch<T = unknown>(
  method: GatewayMethod,
  path: `/api/${string}`,
  body?: unknown,
): Promise<T> {
  return (await invoke("gateway_fetch", { method, path, body })) as T;
}

export const gwGet = <T = unknown>(path: `/api/${string}`) =>
  gatewayFetch<T>("GET", path);
export const gwPost = <T = unknown>(path: `/api/${string}`, body?: unknown) =>
  gatewayFetch<T>("POST", path, body);
export const gwPatch = <T = unknown>(path: `/api/${string}`, body?: unknown) =>
  gatewayFetch<T>("PATCH", path, body);
export const gwDelete = <T = unknown>(path: `/api/${string}`) =>
  gatewayFetch<T>("DELETE", path);
