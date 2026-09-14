/**
 * Rust 侧命令与事件的类型化封装。
 *
 * 字段名与 `src-tauri/src/dsh/*.rs` 的 serde 输出一一对应
 * (`#[serde(rename_all = "camelCase")]`),改 Rust 结构体时这里要同步。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type BackendStatus =
  | { phase: "idle" }
  | { phase: "starting"; program: string; source: string }
  | { phase: "ready"; url: string; port: number; pid: number; startedAtMs: number }
  | { phase: "stopping" }
  | { phase: "failed"; message: string }
  | { phase: "exited"; code: number | null; message: string };

export interface LogLine {
  seq: number;
  stream: string;
  text: string;
  atMs: number;
}

export interface ResolvedDsh {
  program: string;
  args: string[];
  source: string;
  sourceLabel: string;
  display: string;
}

export interface Probe {
  dir: string;
  hasDsh: boolean;
  hasNode: boolean;
}

export interface EnvReport {
  platform: string;
  dshHome: string;
  dshHomeExists: boolean;
  dshHomeWritable: boolean;
  dshHomeOwner: number | null;
  currentUid: number | null;
  profile: string;
  profileDir: string;
  profileManifest: string;
  profileReady: boolean;
  probes: Probe[];
  resolved: ResolvedDsh | null;
  node: string | null;
}

export interface EnabledEntry {
  name: string;
  builtin: boolean;
  installedVersion: string | null;
}

export interface PluginSnapshot {
  profile: string;
  profileDir: string;
  manifestPath: string;
  patchPath: string;
  exists: boolean;
  enabled: EnabledEntry[];
  dependencies: Record<string, string>;
  installedNotEnabled: string[];
  patch: string;
}

export const api = {
  status: () => invoke<BackendStatus>("backend_status"),
  logs: () => invoke<LogLine[]>("backend_logs"),
  start: () => invoke<BackendStatus>("backend_start"),
  stop: () => invoke<BackendStatus>("backend_stop"),
  restart: () => invoke<BackendStatus>("backend_restart"),
  openDshUi: () => invoke<void>("open_dsh_ui"),
  envReport: () => invoke<EnvReport>("env_report"),
  pluginSnapshot: () => invoke<PluginSnapshot>("plugin_snapshot"),
  reveal: (path: string) => invoke<void>("reveal_in_finder", { path }),
};

export const onStatus = (cb: (s: BackendStatus) => void): Promise<UnlistenFn> =>
  listen<BackendStatus>("backend://status", (e) => cb(e.payload));

export const onLog = (cb: (l: LogLine) => void): Promise<UnlistenFn> =>
  listen<LogLine>("backend://log", (e) => cb(e.payload));
