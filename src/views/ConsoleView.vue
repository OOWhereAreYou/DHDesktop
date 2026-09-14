<script setup lang="ts">
/**
 * 控制台窗口:后端状态、插件管理、环境诊断。
 *
 * 主窗口会被导航到 dsh 自带界面,所以「能随时回来操作」的入口在这个独立窗口,
 * 通过原生菜单(DHDesktop → 控制台 / 插件管理,⌘⇧P)打开。
 */
import { computed, onMounted, onUnmounted, ref } from "vue";
import {
  api,
  onLog,
  onStatus,
  type BackendStatus,
  type EnvReport,
  type LogLine,
  type PluginSnapshot,
} from "../api";
import LogPanel from "../components/LogPanel.vue";

type Tab = "backend" | "plugins" | "diag";

const tab = ref<Tab>("backend");
const status = ref<BackendStatus>({ phase: "idle" });
const logs = ref<LogLine[]>([]);
const env = ref<EnvReport | null>(null);
const plugins = ref<PluginSnapshot | null>(null);
const busy = ref(false);
const pluginError = ref("");
const unlisteners: Array<() => void> = [];

const phase = computed(() => status.value.phase);

const statusText = computed(() => {
  const s = status.value;
  switch (s.phase) {
    case "idle":
      return "未运行";
    case "starting":
      return "正在启动…";
    case "ready":
      return `运行中 · 端口 ${s.port}`;
    case "stopping":
      return "正在停止…";
    case "failed":
      return "启动失败";
    case "exited":
      return "已退出";
  }
});

const statusClass = computed(() => {
  switch (status.value.phase) {
    case "ready":
      return "ok";
    case "starting":
    case "stopping":
      return "busy";
    case "failed":
    case "exited":
      return "err";
    default:
      return "";
  }
});

const activeProbes = computed(() => (env.value?.probes ?? []).filter((p) => p.hasDsh || p.hasNode));

async function refresh() {
  const [s, l, e, p] = await Promise.all([
    api.status(),
    api.logs(),
    api.envReport(),
    api.pluginSnapshot().catch((err) => {
      pluginError.value = String(err);
      return null;
    }),
  ]);
  status.value = s;
  logs.value = l;
  env.value = e;
  if (p) plugins.value = p;
}

async function act(fn: () => Promise<unknown>) {
  busy.value = true;
  try {
    await fn();
  } finally {
    busy.value = false;
    await refresh();
  }
}

onMounted(async () => {
  await refresh();
  unlisteners.push(await onStatus((s) => (status.value = s)));
  unlisteners.push(
    await onLog((line) => {
      logs.value.push(line);
      if (logs.value.length > 800) logs.value.splice(0, logs.value.length - 800);
    }),
  );
});

onUnmounted(() => {
  for (const off of unlisteners) off();
});
</script>

<template>
  <div class="page">
    <header class="bar">
      <h1>DHDesktop 控制台</h1>
      <span class="pill" :class="statusClass"><i class="dot" />{{ statusText }}</span>
      <button class="btn ghost small" @click="refresh">刷新</button>
    </header>

    <nav class="tabs">
      <button :class="{ on: tab === 'backend' }" @click="tab = 'backend'">后端</button>
      <button :class="{ on: tab === 'plugins' }" @click="tab = 'plugins'">插件</button>
      <button :class="{ on: tab === 'diag' }" @click="tab = 'diag'">诊断</button>
    </nav>

    <!-- ------------------------------------------------------------------ 后端 -->
    <section v-if="tab === 'backend'" class="panel">
      <div class="row">
        <button
          class="btn primary"
          :disabled="busy || ['starting', 'ready', 'stopping'].includes(phase)"
          @click="act(api.start)"
        >
          启动
        </button>
        <button class="btn" :disabled="busy || phase === 'idle'" @click="act(api.stop)">停止</button>
        <button class="btn" :disabled="busy || phase !== 'ready'" @click="act(api.restart)">
          重启
        </button>
        <button class="btn ghost" :disabled="phase !== 'ready'" @click="act(api.openDshUi)">
          打开 dsh 界面
        </button>
      </div>

      <dl class="kv" v-if="status.phase === 'ready'">
        <dt>地址</dt>
        <dd><code>{{ status.url }}</code></dd>
        <dt>PID</dt>
        <dd>{{ status.pid }}</dd>
      </dl>
      <div v-else-if="status.phase === 'failed' || status.phase === 'exited'" class="note err">
        {{ status.message }}
      </div>
      <div v-else-if="status.phase === 'starting'" class="note">
        启动中使用的命令来源:{{ status.source }}
        <br /><code>{{ status.program }}</code>
      </div>

      <h2>日志</h2>
      <LogPanel :lines="logs" class="fill" />
    </section>

    <!-- ------------------------------------------------------------------ 插件 -->
    <section v-else-if="tab === 'plugins'" class="panel">
      <div v-if="pluginError" class="note err">{{ pluginError }}</div>

      <template v-if="plugins">
        <div class="row">
          <div class="grow">
            profile <code>{{ plugins.profile }}</code>
            <span v-if="!plugins.exists" class="dim">(尚未初始化,首次启动后端时创建)</span>
          </div>
          <button class="btn" @click="api.reveal(plugins.profileDir)">在访达中显示</button>
        </div>

        <div class="note">
          插件清单来自 <code>{{ plugins.manifestPath }}</code>:<code>dsh.profile.bundles</code>
          决定启用顺序,<code>dependencies</code> 是已安装的包。禁用/覆盖走同目录的
          <code>cordis.patch.yml</code> 补丁层。
          <br /><strong>安装 / 启用 / 禁用</strong>下一步接入(命令为
          <code>dsh plugin --profile {{ plugins.profile }} add &lt;包名&gt;</code>)。
        </div>

        <h2>已启用({{ plugins.enabled.length }})</h2>
        <div v-if="plugins.enabled.length === 0" class="dim empty">无</div>
        <ul class="list">
          <li v-for="entry in plugins.enabled" :key="entry.name">
            <code>{{ entry.name }}</code>
            <span class="tag" :class="entry.builtin ? 'builtin' : 'third'">
              {{ entry.builtin ? "内置" : "第三方" }}
            </span>
            <span v-if="entry.installedVersion" class="dim">{{ entry.installedVersion }}</span>
          </li>
        </ul>

        <h2>已安装依赖({{ Object.keys(plugins.dependencies).length }})</h2>
        <div v-if="Object.keys(plugins.dependencies).length === 0" class="dim empty">
          还没有安装任何插件
        </div>
        <ul v-else class="list">
          <li v-for="(version, name) in plugins.dependencies" :key="name">
            <code>{{ name }}</code><span class="dim">{{ version }}</span>
          </li>
        </ul>

        <template v-if="plugins.installedNotEnabled.length">
          <h2>已安装但未启用({{ plugins.installedNotEnabled.length }})</h2>
          <ul class="list">
            <li v-for="name in plugins.installedNotEnabled" :key="name"><code>{{ name }}</code></li>
          </ul>
        </template>

        <h2>补丁层 cordis.patch.yml</h2>
        <pre class="code">{{ plugins.patch || "(空)" }}</pre>
      </template>
    </section>

    <!-- ------------------------------------------------------------------ 诊断 -->
    <section v-else class="panel">
      <template v-if="env">
        <dl class="kv">
          <dt>平台</dt>
          <dd>{{ env.platform }}</dd>
          <dt>DSH_HOME</dt>
          <dd>
            <code>{{ env.dshHome }}</code>
            <span v-if="!env.dshHomeExists" class="dim">(不存在)</span>
            <span v-else-if="env.dshHomeWritable" class="ok">可写</span>
            <span v-else class="bad">
              不可写 · 属主 uid {{ env.dshHomeOwner }}(当前 {{ env.currentUid }})
            </span>
          </dd>
          <dt>profile</dt>
          <dd>
            <code>{{ env.profile }}</code>{{ env.profileReady ? " · 已初始化" : " · 待创建" }}
          </dd>
          <dt>node</dt>
          <dd><code>{{ env.node ?? "未找到" }}</code></dd>
          <dt>dsh</dt>
          <dd v-if="env.resolved">
            <code>{{ env.resolved.program }}</code>
            <span class="dim">来源:{{ env.resolved.sourceLabel }}</span>
          </dd>
          <dd v-else class="bad">未找到任何可用的 dsh</dd>
        </dl>

        <div
          v-if="env.dshHomeExists && !env.dshHomeWritable && env.dshHomeOwner !== env.currentUid"
          class="note warn"
        >
          修复权限:<code>sudo chown -R $(whoami) ~/.dsh</code>
        </div>

        <h2>已探测的目录(仅列出含 dsh 或 node 的)</h2>
        <table class="tbl">
          <thead>
            <tr>
              <th>目录</th>
              <th>dsh</th>
              <th>node</th>
            </tr>
          </thead>
          <tbody>
            <tr v-for="probe in activeProbes" :key="probe.dir">
              <td><code>{{ probe.dir }}</code></td>
              <td>{{ probe.hasDsh ? "✓" : "" }}</td>
              <td>{{ probe.hasNode ? "✓" : "" }}</td>
            </tr>
          </tbody>
        </table>
        <p class="dim">
          共探测 {{ env.probes.length }} 个目录。GUI 启动的应用拿到的是极窄的 PATH,所以这里会主动
          检查 Homebrew / nvm / pnpm / fnm 等常见位置。
        </p>
      </template>
    </section>
  </div>
</template>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  height: 100%;
  padding: 16px 20px 18px;
  gap: 12px;
}

.bar {
  display: flex;
  align-items: center;
  gap: 12px;
}
.bar h1 {
  margin: 0;
  font-size: 15px;
  font-weight: 600;
}
.bar .pill {
  margin-left: auto;
}
.btn.small {
  padding: 3px 9px;
  font-size: 12px;
}

.tabs {
  display: flex;
  gap: 4px;
  border-bottom: 1px solid var(--border-soft);
}
.tabs button {
  padding: 7px 14px;
  border: none;
  background: transparent;
  color: var(--text-dim);
  cursor: pointer;
  border-bottom: 2px solid transparent;
  margin-bottom: -1px;
}
.tabs button:hover {
  color: var(--text);
}
.tabs button.on {
  color: var(--text);
  border-bottom-color: var(--accent);
}

.panel {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: 11px;
  overflow: auto;
}
.panel > h2 {
  margin: 6px 0 0;
  font-size: 12px;
  font-weight: 600;
  color: var(--text-dim);
  text-transform: none;
}

.row {
  display: flex;
  gap: 8px;
  flex-wrap: wrap;
  align-items: center;
}
.grow {
  flex: 1;
  font-size: 13px;
}

.kv {
  display: grid;
  grid-template-columns: 96px 1fr;
  gap: 5px 12px;
  margin: 0;
  font-size: 13px;
}
.kv dt {
  color: var(--text-faint);
}
.kv dd {
  margin: 0;
  word-break: break-all;
}

.list {
  list-style: none;
  margin: 0;
  padding: 0;
  display: flex;
  flex-direction: column;
  gap: 4px;
}
.list li {
  display: flex;
  align-items: center;
  gap: 9px;
  padding: 6px 10px;
  border: 1px solid var(--border-soft);
  border-radius: 7px;
  background: var(--bg-elev);
  font-size: 13px;
}
.list li code {
  word-break: break-all;
}

.tag {
  font-size: 11px;
  padding: 1px 7px;
  border-radius: 999px;
  border: 1px solid var(--border);
  color: var(--text-faint);
  flex: none;
}
.tag.third {
  color: #79c0ff;
  border-color: #1f3a5c;
  background: #10202f;
}

.code {
  margin: 0;
  background: #0b0d11;
  border: 1px solid var(--border-soft);
  border-radius: 8px;
  padding: 10px 12px;
  font-size: 12px;
  color: #cdd5e0;
  overflow: auto;
  max-height: 260px;
}

.tbl {
  border-collapse: collapse;
  font-size: 12.5px;
  width: 100%;
}
.tbl th,
.tbl td {
  text-align: left;
  padding: 5px 10px;
  border-bottom: 1px solid var(--border-soft);
}
.tbl th {
  color: var(--text-faint);
  font-weight: 500;
}

.dim {
  color: var(--text-faint);
  font-size: 12.5px;
}
.ok {
  color: #7ee787;
}
.bad {
  color: #ff7b72;
}
.empty {
  padding: 4px 0;
}
.fill {
  flex: 1;
  min-height: 180px;
}
</style>
