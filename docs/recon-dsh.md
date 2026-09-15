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

## 5. 启动、就绪与鉴权(全部实测)

### 鉴权的实现细节(读源码: `packages/client/connection/src/browser-auth.ts`)

`isAuthenticated()` 只看两样东西:**`Host` 头** 与 **`Cookie` 头**。

```
authority = host 头                     // 如 127.0.0.1:50000
cookie 名  = dsh-auth-<hash(authority)>
cookie 值  = v1.<base64 payload>.<hmac>   // 用 credentials 里的 secret 签名
payload   = { version, authority, issuedAt, expiresAt }
```

- `authorizeIndex()`:带 `?token=<launchToken>` 的 `GET /` → 303 + `Set-Cookie`;其它情况必须带有效 cookie,否则 401 纯文本「dsh web authentication required; reopen the URL printed by dsh web.」
- token 是无状态的(与进程启动 token 比对),**可以反复使用**,不是一次性的。
- 要点:**既然只认 Host/Cookie 两个头,那能自由设头的非浏览器客户端(比如 Rust 代理)也能通过鉴权。**

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

1. **官方 UI 装进 WebView 的正确做法(已实现)**:
   - 不要从我们自己的页面导航过去(跨站导航会让那枚 `SameSite=Strict` cookie 不被带上,页面停在 401——实际踩过)。
   - 也不要“先建窗口加载、再注入 cookie、再重载”(实测重载不带 cookie)。
   - **正确顺序**:Rust 先用自己的 HTTP 请求把 token 换成 cookie → **先写入窗口的 cookie 存储** → 再创建窗口加载干净的根地址。
2. 自研 UI 若从 `tauri://localhost` 或 Vite dev server 发请求 → **403**。必须二选一:
   - Rust 侧正向代理(自己写 `Host` / `Origin` 并携带 cookie);
   - 或 `--trusted-host` 把开发 origin 加进白名单。
3. iframe 嵌套**没有** `X-Frame-Options` / CSP `frame-ancestors` 限制(实测响应头只有 `content-type`)。

### 验证手段的教训(重要)

判断「界面到底加载成功没有」不能靠间接信号:

| 手段 | 结果 |
| --- | --- |
| 回读窗口 URL(token 被消费 → `/`) | ❌ **误导**:只要跟随了 303 就会变,401 页面也一样 |
| `webview.cookies_for_url(url)` | ❌ **误导**:macOS 上实测恒返回 0(全部 cookie 里明明有那枚) |
| **让页面自己报告**(Rust 临时监听一个环回端口,让页面 `fetch` 回自己的 `document.title` 与正文) | ✅ 确定:401 是 `text/plain` 无标题,SPA 标题是 `DeepSeek Harness` |

最后这个探针实现在 `src-tauri/src/dsh/backend.rs` 的 `page_report_probe()`,用 `DHDESKTOP_PAGE_PROBE=1` 开启。

---

## 6. profile 与插件机制

> **目录归属决策(2026-09-15)**:DHDesktop 使用**完全独立**的数据目录,
> 不读也不写用户机器上共享的 `~/.dsh`。传给子进程的 `DSH_HOME` 是
> `~/Library/Application Support/com.dhdesktop.app/dsh-home`(可用 `DHDESKTOP_DSH_HOME` 覆盖)。
> 下文涉及的 `$DSH_HOME` 均指这个独立目录。

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

### 6.1 官方桌面端的安装流程(源码侦察)

来源:`apps/desktop/src/project-manager.ts:378-391`、`apps/desktop/renderer/plugin-manager.js:77-85`

```
点「安装」→ pnpm add <spec> --save-exact --ignore-scripts
          → 读新包 package.json,校验它声明了 dsh.bundle.patch
            (不声明就报错 —— 所以不能随便装一个 npm 包当插件)
          → 把包名追加到 dsh.profile.bundles
```

| 操作 | 机制 | 需重启 |
| --- | --- | --- |
| 启用 | 包名加入 `dsh.profile.bundles` | 是 |
| 禁用 | 从 `dsh.profile.bundles` 移除 | 是 |
| 卸载 | `pnpm remove` + 从 `dependencies` 与 `bundles` 都删 | 是 |

**关键结论**:不要自己重写 pnpm + 清单同步逻辑。`dsh plugin --profile <名> add|remove|update <包>`
内部就是「pnpm 薄包装 + `reconcilePlugins()` 自动同步 bundles 数组」(`apps/cli/src/plugin.ts:127-165`),
直接调它比自己拼更稳。

插件包必须在自己的 `package.json` 里声明 `dsh.bundle.patch`,指向一个 Cordis 配置片段 YAML;
启动时按 `bundles` 顺序依次加载这些片段。

---

## 7. 硬约束与环境坑

| 约束 | 影响 |
| --- | --- |
| `@deepseek-ai/dsh-desktop-host` 未发布(404) | 拿不到官方管道传输层,只能用公开 CLI |
| `npx @deepseek-ai/dsh` 首次下载 **约 292MB**(`~/.npm/_npx` 总计 808MB,实测) | **不能把 npx 作为默认路径**;必须内置运行时或明确的首次下载引导 |
| 官方标注 **developer preview,会有不兼容改动** | 协议与配置格式需要版本适配层 / 钉版本 |
| 本机 `~/.dsh` **整个目录属于 root**(`settings.yaml` / `.credentials.yaml` 均为 root 所有) | 普通用户无法写入 → dsh 报 `EACCES`。**DHDesktop 通过使用独立数据目录规避了这个问题**(见第 6 节),因此不影响客户端 |
| `dsh` 是**三层进程树**(`npx` → `npm exec` → `node .../.bin/dsh`) | 只杀直接子进程会把真正的服务留下变孤儿(实测)。必须 `setpgid` 自成进程组 + 按组发信号 |
| 应用被强杀时,退出钩子不执行 | 子进程组会残留并占端口 → 需要 pidfile + 下次启动前的残留清理 |
| Finder 启动的 GUI 应用只拿到 `/usr/bin:/bin:/usr/sbin:/sbin` | `dsh`/`npx` 都是 `#!/usr/bin/env node`,子进程会因找不到 `node` 而死 → 必须补全 `PATH` |

---

## 8. 对 DHDesktop 的架构结论

**第一阶段(P1)** — ✅ 已实现:Rust 托管 `dsh web --no-open --port 0`,解析 stdout 那行拿到端口 + token,
Tauri 主窗口导航到该 URL。用户「打开就能用」,且**完全不依赖 RPC 协议**。

**第二阶段(P2)** — 待做:插件管理 = 独立窗口 + Rust 命令:读 `package.json` 的
`dsh.profile.bundles` / `dependencies`,调 `dsh plugin --profile dhdesktop add|remove`,
写 `cordis.patch.yml` 做禁用,改完重启后端。同样**不依赖 RPC 协议**。

**第三阶段(P3)** — 待定:是否用自研 Vue UI 替换官方 Web UI。若做,必须让 **Rust 当代理**
(自己设 `Origin` + 持 cookie),或改走 ACP/SDK 入口(见第 9 节)。

**profile 归属**:DHDesktop 使用自己的 profile(`dhdesktop`,由 `--from-default-profile web` 初始化),
与用户 CLI 的 `web` profile 隔离,理由同官方:「共享可执行依赖图会让两者相互改变版本」。

---

## 9. 三条集成路径的最终结论(源码侦察)

| 路径 | 传输 | 官方定位 | 对第三方 GUI 的结论 |
| --- | --- | --- | --- |
| **A. API Gateway** | WebSocket `/api/remote.mux` + HTTP `POST /api/<ns>/<method>` | 官方 Web UI 的正式接口 | ✅ **唯一官方认可的 GUI 路径**(官方 Web UI 就是这么做的) |
| B. SDK | 换行分帧的 JSON-RPC over stdio | 「面向以子进程方式启动 Harness 的外部程序」 | ⚠️ 可行但非设计目标;适合程序式集成,不适合 GUI |
| C. ACP | JSON-RPC over stdio | 「**仅面向自动化**的服务器」 | ❌ 官方明确不把 GUI 列入支持范围 |
| Desktop 分帧管道 | 私有协议 | 官方 Electron 桌面端 | ❌ 无公开规范,第三方无法实现 |

**路径 A 的流式协议形状**(来源:`packages/api/gateway/src/stream-protocol.ts:260-290`):

```ts
// 客户端 → 服务端
{ type: 'open' | 'cancel', streamId, endpoint, payload }
// 服务端 → 客户端
{ type: 'item' | 'error' | 'end', streamId, value? }
```

### 自研 UI 的硬前提

路径 A 要做自研 UI,必须由 **Rust 做正向代理**,前端不能直连:

1. HTTP:注入 loopback 的 `Host` / `Origin`(围栏按这两者校验),带上签名 cookie;
2. WebSocket:先做一次 HTTP 握手拿到 cookie,再用 `Cookie` 头转发 upgrade;
3. 原因见第 5 节实测:cookie 是 `SameSite=Strict; HttpOnly`,跨源一律 403。

这也是官方 Electron 端宁可用私有管道也不用端口的原因。

## 10. 待补

- [x] ACP / SDK 两条路径的定位与稳定性评估 → 见第 9 节
- [x] `dsh plugin` 的精确语义 → 见第 6.1 节
- [ ] `--trusted-host` 是否接受 `tauri://localhost` 这类非 http scheme(若要做自研 UI 则需验证)
- [x] `dsh web` 的端口策略:`--port 0` 由系统挑端口,实际端口写在就绪行里(实测)
- [x] 崩溃/强杀后的残留行为:进程组会残留,已用 pidfile + 启动前清理兜住(实测)
- [x] Finder 双击启动(极窄 PATH)能否工作:已用 `env -i` 模拟验证通过
