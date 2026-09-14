# DHDesktop

**DeepSeek Harness 桌面客户端** —— 基于 [Tauri v2](https://v2.tauri.app/) + Vue 3 + TypeScript。

## dsh 是什么

[DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)(`dsh`,npm 包 `@deepseek-ai/dsh`)是 DeepSeek 官方开源的
agent runtime:基于 Cordis 的「一切皆插件」架构,负责模型适配、工具调用、上下文管理、沙箱、append-only 会话日志与 agent 主循环。
它自带一个 Web UI(`dsh web`,默认 `http://127.0.0.1:3080`)。

DHDesktop 不复刻 dsh 的能力,只做**桌面外壳 + 原生交互层**:管好 dsh 进程的生命周期,把它的流式会话协议渲染成桌面原生的使用体验。

> ⚠️ dsh 处于 developer preview,接口与配置格式可能发生不兼容变更,适配层需要按版本隔离。

## 技术选型(已确认)

| 维度 | 选择 | 备注 |
| --- | --- | --- |
| 桌面框架 | Tauri v2 | Rust 后端 + 系统 WebView,安装包远小于 Electron |
| 前端 | Vue 3 + TypeScript + Vite | `<script setup>` SFC |
| 包管理 | pnpm | 仓库根即应用根,无 monorepo |
| 集成方式 | Rust 托管 dsh 进程 + 前端直连 dsh API | 见下 |

## 集成架构(已确认)

```
┌─────────────────────────── DHDesktop ───────────────────────────┐
│  Vue 3 前端 (WebView)                                            │
│      │  HTTP + WebSocket 直连 ──────────────┐                    │
│      │  Tauri IPC(仅进程控制/配置)          │                    │
│      ▼                                      ▼                    │
│  src-tauri (Rust)  ──spawn/守护/日志──▶  dsh 进程 (127.0.0.1:3080)│
└──────────────────────────────────────────────────────────────────┘
```

- **Rust 侧**:定位、启动、健康检查、日志转发、崩溃重启、退出时清理子进程。
- **前端**:直接与 dsh 的 HTTP + WebSocket 通信,**不经过 Rust 中转**,避免流式 token 多一跳。
- **运行时来源策略**(目标是「用户零操作」):
  1. 复用已在运行的实例(端口探测 + 健康检查);
  2. 否则使用 `PATH` 中已安装的 `dsh`;
  3. 否则用 `npx @deepseek-ai/dsh` 自动拉起兜底(首次需下载);
  4. 支持手动配置远程 dsh 地址(局域网 / 服务器);
  5. 长期方案:随安装包捆绑 Node 运行时 + dsh 作为 sidecar,彻底免依赖。

## 目录结构

```
DHDesktop/
├── src/                    # Vue 3 前端
│   ├── App.vue
│   ├── main.ts
│   └── assets/
├── src-tauri/              # Rust 后端
│   ├── src/
│   │   ├── main.rs         # 入口(仅调用 lib 的 run)
│   │   └── lib.rs          # Tauri builder + commands(待重构为多模块)
│   ├── capabilities/       # 权限声明
│   ├── icons/
│   ├── Cargo.toml
│   └── tauri.conf.json
├── public/
├── index.html
├── vite.config.ts
└── package.json
```

## 开发

```sh
pnpm install
pnpm tauri dev        # 启动桌面应用(自动拉起 vite dev server)
pnpm tauri build      # 打包安装包
pnpm build            # 仅前端:vue-tsc 类型检查 + vite build
```

要求:Node.js 20+、Rust stable、Xcode Command Line Tools(macOS)。

## 当前状态

- [x] 项目初始化(Tauri v2 + Vue 3 + TS + Vite 模板)
- [x] 应用命名 / 标识符:`DHDesktop` / `com.dhdesktop.app`
- [x] dsh 接入侦察(见 [`docs/recon-dsh.md`](docs/recon-dsh.md),含实机验证)
- [x] **P1 进程托管**:定位 → 启动 → 就绪检测 → 主窗口切到 dsh 界面 → 停止 / 退出清理
- [x] 控制台窗口:后端状态、日志、环境诊断、插件清单(只读)
- [ ] **P2 插件管理**:安装 / 启用 / 禁用 / 卸载(目标命令 `dsh plugin --profile dhdesktop add <包>`)
- [ ] 内置运行时(消灭 npx 兜底,见下)

## 已实现的运行时托管

细节见 [`docs/recon-dsh.md`](docs/recon-dsh.md),几个关键点:

* 启动命令:`dsh --profile dhdesktop [--from-default-profile web] --no-open --port 0 --host 127.0.0.1`
  —— `--port 0` 让系统挑端口,实际端口从就绪行里读,不用抢端口。
* 就绪信号:stdout 上恰好一行 `dsh web: http://127.0.0.1:<port>/?token=<token>`,
  同时给出端口与鉴权令牌;拿到后把**主窗口导航**到该 URL(dsh 自带界面)。
* 停止:SIGTERM → 8s 宽限 → SIGKILL。
* **按进程组收尾**:`dsh`(尤其 npx 路径)是「npx → npm exec → node dsh」三层进程树,
  只杀直接子进程会把真正的服务留下变孤儿(实测确认),所以子进程用 `setpgid` 自成进程组,
  停止时向整组发信号。
* **残留清理**:进程组 id 落盘到 `$DSH_HOME/dhdesktop-backend.pid`。应用被强杀/崩溃时退出钩子
  不会执行,下次启动会先扫一遍并收掉残留(启动前会用 `ps` 核对确实是我们的 profile,避免 pgid 复用误杀)。
* **GUI 启动的 PATH 问题**:Finder 启动的应用只拿到 `/usr/bin:/bin:/usr/sbin:/sbin`,
  连 `node` 都找不到(`dsh`/`npx` 都是 `#!/usr/bin/env node`),所以会主动探测 Homebrew / nvm /
  pnpm / fnm 等目录并补进子进程的 `PATH`。
* **并发保护**:启动/停止/重启串行化(`lifecycle` 锁)。早期版本存在 TOCTOU 竞态,
  两个并发调用会起出两个 dsh(实测两个进程相差 33ms)。

## ⚠️ 已知阻塞与坑

1. **`npx` 兜底代价大**:若系统里没有全局 `dsh`,会退到 `npx -y @deepseek-ai/dsh@latest`,
   首次要下载约 **292MB**。终态方案是随安装包内置 Node + dsh(官方 Electron 版就是这么做的)。
2. **`~/.dsh` 属主问题**:若该目录曾被 `sudo` 使用过(属主变成 root),普通用户完全无法启动 dsh。
   控制台「诊断」页会检测并提示:`sudo chown -R $(whoami) ~/.dsh`。
3. **协议围栏**:dsh 的 `/api` 有 Origin 信任围栏(非本服务 origin 返回 403),
   且 cookie 是 `SameSite=Strict; HttpOnly`。因此自研界面**不能**直接跨 origin 调 API,
   必须走 Rust 代理 —— 这也是当前阶段直接复用 dsh 自带界面的原因。
