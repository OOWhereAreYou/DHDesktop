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
- [ ] 移除模板 demo(`greet` command、模板 App.vue)
- [ ] Rust 侧 dsh 进程管理器(探测 / 启动 / 健康检查 / 日志 / 退出清理)
- [ ] 前端 dsh 客户端封装(HTTP + WebSocket 流式会话)
- [ ] 会话 UI(消息流、思维链折叠、工具调用卡片)
- [ ] 设置页(运行时来源、远程地址、模型与 provider 配置)
