# dsh 接入侦察笔记

> 侦察对象:`deepseek-ai/deepseek-harness` @ `c291e79`(v0.1.5-rc.2),浅克隆于 `/Volumes/W/code/refs/deepseek-harness`
> 侦察日期:2026-09-14 · 方法:读源码 + 实机运行 `dsh web` 并用 curl 验证

本文件是 DHDesktop 所有集成代码的依据。**标注「实测」的结论是实际跑出来的,可直接依赖;标注「读源码」的来自代码阅读,可能随版本变化。**

---

## 1. 结论摘要(TL;DR)

| 问题 | 结论 |
| --- | --- |
| 怎么连 dsh | `dsh web` 起本地 HTTP 服务,**唯一对第三方可用且开箱即用的入口** |
| 怎么知道起来了 | 启动后 stdout 打**一行** `dsh web: http://<host>:<port>/?token=<token>`,这一行同时给出端口与令牌(实测) |
| 怎么鉴权 | URL 里的 token 换 **cookie**,`dsh-auth-<authority>`,`HttpOnly; SameSite=Strict`,有效期 30 天(实测) |
| 自研 UI 能直连 `/api` 吗 | **不能**。非本服务 authority 的 `Origin` 一律 **403**(实测),而 cookie 是 `SameSite=Strict` 且 `HttpOnly` |
| 绕过办法 | Rust 侧代理(自己设 `Origin` 头,非浏览器客户端不受围栏约束,实测无 Origin 时返回 404 而非 403),或 `dsh web --trusted-host` 白名单 |
| 插件怎么装 | `dsh plugin --profile <名字> add <包名>` —— 官方 CLI 把参数**原样转发给 profile 目录里的 pnpm** |
| 插件状态存在哪 | `$DSH_HOME/profiles/<名字>/package.json` 的 `dependencies` + `dsh.profile.bundles` |

**对架构的直接影响:第一阶段应"把官方 Web UI 装进窗口",而不是自研界面直连 API。** 详见第 8 节。

---

## 2. dsh 的形态

- npm 包 `@deepseek-ai/dsh` 只有 **49KB**,是薄壳;真实功能拆成约 **75 个 `@deepseek-ai/*` 包**(每个插件/工具一个包)。
- 内核是 Cordis,「一切皆插件」;profile 是**有序的 bundle 补丁层栈**。
- `apps/` 下有:`cli`(就是 `@deepseek-ai/dsh`)、`web`(官方 Web UI)、`desktop` + `desktop-host`(**官方 Electron 桌面端**)。

### ⚠️ 官方已有 Electron 桌面端

`apps/desktop` 已实现:内置 Node 24.17.0 + 内置 pnpm + 完整 dsh 依赖树,签名/公证/自动更新,独立的插件管理窗口。**DHDesktop 的定位必须与它区分**(体积、定制自由),不能假装它不存在。

**但它不可复用**:`@deepseek-ai/dsh-desktop-host` 在 npm 上是 **404**(`"private": true`,实测),官方那套「分帧字节管道」传输层拿不到。第三方只能用公开 CLI。

---

## 3. 可用的对外入口(profile / bundle)

`packages/bundle/` 下六个 bundle,对应 npm 包:

| bundle | npm 包 | 用途 |
| --- | --- | --- |
| base | `@deepseek-ai/dsh-base` | 基础层,所有 profile 的底座 |
| web-app | `@deepseek-ai/dsh-web-app` | 浏览器 UI + HTTP/WS 服务 |
| headless | `@deepseek-ai/dsh-headless` | 跑一个任务、打印结果、退出 |
| acp-app | `@deepseek-ai/dsh-acp-app` | 标准 Agent Client Protocol(JSON-RPC stdio) |
| sdk-app / sdk-minimal | `@deepseek-ai/dsh-sdk-*` | 编程式 SDK |

## 4. CLI 命令面

来源:`apps/cli/src/args.ts:175-205`

```
dsh                                        # 启动器
  --profile <name>                         # 启动 $DSH_HOME/profiles 下的 profile
  --from-default-profile <name>            # 从内置模板新建自定义 profile
  --patch <path>                           # 追加补丁层(可重复)
  --dump-config / --dump-default-config    # 打印合成后的配置树后退出

dsh web [web 应用自己的参数...]             # = --profile web
dsh plugin --profile <name> <pnpm 参数...>  # 转发给 profile 目录里的 pnpm
```

**`dsh web` 的参数(实测 `dsh web --help`):**

| 参数 | 说明 |
| --- | --- |
| `--host <host>` | 绑定地址 |
| `--port <port>` | 端口;**传 `0` 让系统挑空闲端口** |
| `--no-open` | 不打开默认浏览器(**客户端必须加**) |
| `--trusted-host <authority...>` | 给 `/api` 信任围栏追加可接受的 authority(可重复) |

**注意**:`apps/cli` 里有 `rejectElectronProfile` —— 官方的 `desktop` profile **禁止 CLI 触碰**。DHDesktop 必须用**自己的 profile**(建议 `dhdesktop`),避免与用户 CLI 的 `web` profile 互相污染。

---

## 5. 启动、就绪、鉴权(全部实测)

### 启动

```sh
DSH_HOME=<可写目录> dsh web --no-open --port 3899
# stdout 恰好一行:
dsh web: http://127.0.0.1:3899/?token=BNHjr4KcVzcYGtmEyrM1OG0_ydKK6y83nDT826LTBKI
```

- **这一行就是就绪信号**,同时提供端口与 token —— 不需要端口轮询,也不需要用 `--port 0` 后再猜端口。
- token 每次运行随机生成。

### 鉴权与信任围栏

`GET /?token=<token>` → **303** 到 `/`,并 `Set-Cookie: dsh-auth-<authority>=v1.<base64>{version,authority,issuedAt,expiresAt}.<hmac>; HttpOnly; SameSite=Strict; Max-Age=2592000`

实际行为矩阵(实测):

| 场景 | 结果 |
| --- | --- |
| 无 token 访问 `/` 或 `/api` | **401** |
| 带 token 访问 `/?token=...` | **303** + Set-Cookie,之后 SPA 正常(约 27KB HTML,内含 `window.__ModuleLoader__`) |
| 有效 cookie + `Origin` = 自身 authority | **404**(过围栏,只是该路由不存在) |
| 有效 cookie + **无** `Origin` | **404**(过围栏 → 非浏览器客户端不受限) |
| 有效 cookie + `Origin` = 其他域(如 `http://tauri.localhost`) | **403 forbidden**(被围栏拒绝) |

**推论:**

1. **官方 Web UI 装进 Tauri WebView 是最省事的方案** —— 同 origin,303→cookie→SPA 全自动。
2. 自研 UI 若从 `tauri://localhost` 或 Vite dev server 发起请求 → **403**。必须二选一:
   - Rust 侧代理(自己写 `Origin: http://127.0.0.1:<port>`,并携带 cookie);
   - 或 `--trusted-host <我们的 authority>` 把开发 origin 加进白名单(需验证是否接受端口/非 http scheme)。
3. iframe 嵌套**没有** `X-Frame-Options` / CSP `frame-ancestors` 限制(实测响应头只有 `content-type`),所以「自研 Vue 外壳 + iframe 嵌官方 UI」在技术上也成立。

---

## 6. profile 与插件机制

`$DSH_HOME` 默认 `~/.dsh`,可用环境变量覆盖(`@deepseek-ai/dsh-home-paths`)。目录结构:

```
$DSH_HOME/
├── settings.yaml
├── storages/
└── profiles/
    ├── node_modules/            # hoisted 依赖(252 个条目)
    └── web/
        ├── package.json         # ← 插件装在这里
        ├── cordis.yml           # 合成入口,内容恒为 []
        ├── cordis.patch.yml     # ← 用户的覆盖层
        └── pnpm-workspace.yaml  # nodeLinker: hoisted / autoInstallPeers: false
```

**`profiles/web/package.json` 实物:**

```jsonc
{
  "name": "dsh-profile-web",
  "private": true,
  "dependencies": {},                 // ← 安装的外部插件
  "dsh": {
    "profile": {
      "bundles": [                    // ← 启用顺序:内置 bundle + 已启用插件
        "@deepseek-ai/dsh-base",
        "@deepseek-ai/dsh-web-app"
      ]
    }
  }
}
```

**`cordis.patch.yml`** 是用户自己的补丁层(注释原文):「a top-level YAML array of loader patch entries(id-targeted config overrides,**disables**,and insert lists;`!!js` expressions allowed)」→ **禁用插件就是在这一层做**,不必改 bundle 列表。

**安装插件:** `dsh plugin --profile <name> add <包名>`(转发给 profile 目录里的 pnpm)。插件生态用 GitHub topic `dsh-plugin` 标记。

**DHDesktop 的插件管理因此不需要任何 RPC 协议** —— 就是「读配置文件 + 跑 pnpm + 重启后端」,这是第 8 节把它排在自研 UI 之前的原因。

---

## 7. 硬约束与环境坑

| 约束 | 影响 |
| --- | --- |
| `@deepseek-ai/dsh-desktop-host` 未发布(404) | 拿不到官方管道传输层,只能用公开 CLI |
| `npx @deepseek-ai/dsh` 首次下载 **约 292MB**(`~/.npm/_npx` 总计 808MB,实测) | **不能把 npx 作为默认路径**;必须内置运行时或明确的首次下载引导 |
| 官方标注 **developer preview,会有不兼容改动** | 协议与配置格式需要版本适配层 / 钉版本 |
| 本机 `~/.dsh` **整个目录属于 root**(`settings.yaml` 为 `600 root`) | 普通用户 `chen` 无法写入 → `dsh web` 报 `EACCES ... profiles/web/package.json`。**客户端必须检测并给出修复引导**,否则任何 dsh 使用都会失败 |

---

## 8. 对 DHDesktop 的架构结论

**第一阶段(P1)**:Rust 托管 `dsh web --no-open`,解析 stdout 那行拿到端口+token,Tauri 主窗口加载该 URL。
→ 用户「打开就能用」,且**完全不依赖 RPC 协议**。

**第二阶段(P2)**:插件管理 = 独立窗口 + Rust 命令:读 `package.json` 的 `dsh.profile.bundles` / `dependencies`,调 `dsh plugin --profile dhdesktop add|remove`,写 `cordis.patch.yml` 做禁用,改完重启后端。
→ 同样**不依赖 RPC 协议**,是你明确要的功能。

**第三阶段(P3)**:是否用自研 Vue UI 替换官方 Web UI。若做,必须让 **Rust 当代理**(Origin + cookie),或使用 ACP/SDK 入口 —— 待补(见第 9 节)。

**profile 归属**:DHDesktop 使用自己的 profile(`dhdesktop`,由 `--from-default-profile web` 初始化),与用户 CLI 的 `web` profile 隔离,理由同官方:「共享可执行依赖图会让两者相互改变版本」。

---

## 9. 待补

- [ ] `dsh-acp-app` / `dsh-sdk-*` 两条路径的定位与稳定性评估(能否作为第三方自研 UI 的正式入口)
- [ ] `dsh plugin` 对各包操作(enable/disable/remove)的精确语义
- [ ] `--trusted-host` 是否接受 `tauri://localhost` 这类非 http scheme
- [ ] `dsh web` 的端口占用/崩溃/重启行为
