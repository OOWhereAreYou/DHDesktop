<script setup lang="ts">
/**
 * 主窗口的启动页。
 *
 * 后端就绪后 Rust 会把主窗口**导航**到 dsh 自带界面(见 src-tauri/src/dsh/backend.rs),
 * 所以这个页面主要出现在三段时机:启动前、启动中、以及从 dsh 界面切回来时。
 */
import { computed, onMounted, onUnmounted, ref } from "vue";
import { api, onLog, onStatus, type BackendStatus, type EnvReport, type LogLine } from "../api";
import LogPanel from "../components/LogPanel.vue";

const status = ref<BackendStatus>({ phase: "idle" });
const logs = ref<LogLine[]>([]);
const env = ref<EnvReport | null>(null);
const busy = ref(false);
const showLogs = ref(true);
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

/** 环境里根本没有可用的 dsh。 */
const noDsh = computed(() => env.value !== null && env.value.resolved === null);

/** `~/.dsh` 属主不是当前用户 —— 几乎必然是曾被 sudo 用过,普通用户写不进去。 */
const ownerMismatch = computed(() => {
  const e = env.value;
  if (!e) return false;
  return (
    e.dshHomeExists &&
    !e.dshHomeWritable &&
    e.dshHomeOwner !== null &&
    e.dshHomeOwner !== e.currentUid
  );
});

const failureMessage = computed(() => {
  const s = status.value;
  return s.phase === "failed" || s.phase === "exited" ? s.message : "";
});

const startDisabled = computed(
  () => busy.value || ["starting", "ready", "stopping"].includes(phase.value),
);

async function refresh() {
  const [s, l] = await Promise.all([api.status(), api.logs()]);
  status.value = s;
  logs.value = l;
  if (env.value === null) env.value = await api.envReport();
}

async function act(fn: () => Promise<unknown>) {
  busy.value = true;
  try {
    await fn();
  } finally {
    busy.value = false;
    // 启动/停止是异步推进的,状态由事件推过来,这里只补一次快照。
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
  // 「打开客户端就能用」:主窗口进入且后端未运行时直接拉起,不让用户先点按钮。
  if (status.value.phase === "idle") {
    void act(api.start);
  }
});

onUnmounted(() => {
  for (const off of unlisteners) off();
});
</script>

<template>
  <div class="page">
    <header class="brand">
      <div class="logo">DH</div>
      <div class="titles">
        <h1>DHDesktop</h1>
        <p>DeepSeek Harness 桌面客户端</p>
      </div>
      <span class="pill" :class="statusClass"><i class="dot" />{{ statusText }}</span>
    </header>

    <main class="body">
      <section class="card">
        <div class="actions">
          <button class="btn primary" :disabled="startDisabled" @click="act(api.start)">
            启动 dsh 后端
          </button>
          <button class="btn" :disabled="busy || phase === 'idle'" @click="act(api.stop)">
            停止
          </button>
          <button class="btn" :disabled="busy || phase !== 'ready'" @click="act(api.restart)">
            重启
          </button>
          <button class="btn ghost" :disabled="phase !== 'ready'" @click="act(api.openDshUi)">
            打开 dsh 界面
          </button>
        </div>

        <p v-if="phase === 'starting'" class="hint">
          正在拉起 dsh 并准备 profile。首次启动要从内置模板初始化 profile
          (装依赖),通常会慢一些;进度见下方日志。
        </p>
        <p v-else-if="phase === 'ready'" class="hint">
          后端已就绪,界面会自动切换到 dsh。若没有跳转,点「打开 dsh 界面」。
        </p>

        <div v-if="failureMessage" class="note err">{{ failureMessage }}</div>

        <div v-if="ownerMismatch" class="note warn">
          <strong>{{ env?.dshHome }}</strong> 的属主是 uid {{ env?.dshHomeOwner }},不是当前用户(uid
          {{ env?.currentUid }}),所以 dsh 无法写入而启动失败。常见原因:本应用曾被
          <code>sudo</code> 启动过。修复:
          <code>sudo chown -R $(whoami) "{{ env?.dshHome }}"</code>
        </div>

        <div v-if="noDsh" class="note warn">
          没找到可用的 <code>dsh</code>。两种解决办法:
          <br />1. 全局安装:<code>npm i -g @deepseek-ai/dsh</code>
          <br />2. 在「控制台 → 诊断」里查看探测了哪些目录,或用环境变量
          <code>DHDESKTOP_DSH_BIN</code> 指定可执行文件路径。
        </div>

        <div class="envline" v-if="env">
          <span>DSH_HOME <code>{{ env.dshHome }}</code></span>
          <span>profile <code>{{ env.profile }}</code>{{ env.profileReady ? "(已初始化)" : "(待创建)" }}</span>
          <span v-if="env.resolved">dsh 来源 <code>{{ env.resolved.sourceLabel }}</code></span>
        </div>
      </section>

      <section class="logsection">
        <div class="loghead">
          <h2>日志</h2>
          <button class="btn ghost small" @click="showLogs = !showLogs">
            {{ showLogs ? "收起" : "展开" }}
          </button>
        </div>
        <LogPanel v-show="showLogs" :lines="logs" class="logbox" />
      </section>
    </main>
  </div>
</template>

<style scoped>
.page {
  display: flex;
  flex-direction: column;
  height: 100%;
  padding: 22px 24px 18px;
  gap: 16px;
}

.brand {
  display: flex;
  align-items: center;
  gap: 13px;
}
.logo {
  width: 40px;
  height: 40px;
  border-radius: 10px;
  display: grid;
  place-items: center;
  font-weight: 700;
  letter-spacing: 0.5px;
  color: #fff;
  background: linear-gradient(140deg, #4f8cff, #7a5cff);
}
.titles h1 {
  margin: 0;
  font-size: 17px;
  font-weight: 600;
}
.titles p {
  margin: 1px 0 0;
  font-size: 12px;
  color: var(--text-faint);
}
.brand .pill {
  margin-left: auto;
}

.body {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: 14px;
}

.card {
  border: 1px solid var(--border);
  border-radius: 11px;
  background: var(--bg-elev);
  padding: 16px;
  display: flex;
  flex-direction: column;
  gap: 11px;
}

.actions {
  display: flex;
  flex-wrap: wrap;
  gap: 8px;
}

.hint {
  margin: 0;
  color: var(--text-dim);
  font-size: 13px;
}

.envline {
  display: flex;
  flex-wrap: wrap;
  gap: 6px 18px;
  font-size: 12px;
  color: var(--text-faint);
  border-top: 1px solid var(--border-soft);
  padding-top: 11px;
}
.envline code {
  color: #cdd5e0;
  font-size: 11.5px;
}

.logsection {
  flex: 1;
  min-height: 0;
  display: flex;
  flex-direction: column;
  gap: 7px;
}
.loghead {
  display: flex;
  align-items: center;
  gap: 10px;
}
.loghead h2 {
  margin: 0;
  font-size: 13px;
  font-weight: 600;
  color: var(--text-dim);
}
.loghead .btn {
  margin-left: auto;
}
.btn.small {
  padding: 3px 9px;
  font-size: 12px;
}
.logbox {
  flex: 1;
  min-height: 0;
}
</style>
