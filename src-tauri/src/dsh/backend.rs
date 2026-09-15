//! dsh 后端进程的生命周期管理。
//!
//! 关键机制(全部实机验证,见 `docs/recon-dsh.md` 第 5 节):
//!
//! * 启动 `dsh --profile dhdesktop --no-open --port 0 --host 127.0.0.1`;
//! * `--port 0` 让系统挑空闲端口,**实际端口会写进就绪行**,所以不需要自己抢端口;
//! * 就绪信号是 stdout 上恰好一行:
//!   `dsh web: http://127.0.0.1:<port>/?token=<token>`
//!   —— 这一行同时给出端口与鉴权令牌,不需要轮询探测;
//! * 停止用 SIGTERM,宽限期内不退再 SIGKILL。

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, WebviewUrl, WebviewWindowBuilder};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Mutex;

use super::locate;
use super::paths;

pub const EVENT_STATUS: &str = "backend://status";
pub const EVENT_LOG: &str = "backend://log";

/// dsh 自带界面的窗口 label。
/// 它跑的是远程页面(http://127.0.0.1:<port>),因此**不在** capabilities 的 windows 列表里
/// —— 它不需要也不应该拿 Tauri IPC 权限。
pub const WINDOW_BACKEND: &str = "dsh";

/// 启动页窗口 label(就绪后会被收起)。
const WINDOW_MAIN: &str = "main";

/// 日志环形缓冲上限。
const MAX_LOG_LINES: usize = 800;
/// 首次启动可能要装 profile 依赖,给足时间;超时视为失败并杀掉。
const READY_TIMEOUT: Duration = Duration::from_secs(300);
/// SIGTERM 后的宽限期。
const TERM_GRACE: Duration = Duration::from_secs(8);
/// SIGKILL 后的等待上限。
const KILL_GRACE: Duration = Duration::from_secs(4);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 后端状态。序列化成前端可直接判别的 tagged union(`phase` 字段)。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "phase", rename_all = "camelCase")]
pub enum BackendStatus {
    /// 未运行。
    Idle,
    /// 正在启动(profile 初始化、依赖安装都算在内)。
    Starting {
        program: String,
        source: String,
    },
    /// 已就绪,`url` 可直接加载进窗口。
    Ready {
        url: String,
        port: u16,
        pid: u32,
        started_at_ms: u64,
    },
    /// 正在停止。
    Stopping,
    /// 启动失败(进程没起来 / 超时)。
    Failed {
        message: String,
    },
    /// 起来过,后来退出了。
    Exited {
        code: Option<i32>,
        message: String,
    },
}

impl BackendStatus {
    pub fn is_ready(&self) -> bool {
        matches!(self, BackendStatus::Ready { .. })
    }

    fn ready_url(&self) -> Option<String> {
        match self {
            BackendStatus::Ready { url, .. } => Some(url.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogLine {
    pub seq: u64,
    pub stream: String,
    pub text: String,
    pub at_ms: u64,
}

struct Running {
    pid: u32,
}

struct Inner {
    status: BackendStatus,
    running: Option<Running>,
    /// 已 spawn 但还没就绪的 pid,供超时/停止时清理。
    pending_pid: Option<u32>,
    /// 正在被我们主动停止 —— 用于区分「意外退出」和「按预期退出」。
    stopping: bool,
    logs: VecDeque<LogLine>,
    seq: u64,
}

pub struct BackendManager {
    inner: Mutex<Inner>,
    /// 串行化 start/stop/restart。
    ///
    /// 必要性:`inner` 的「检查状态 → 置为 Starting → spawn」之间要经过 await,
    /// 仅靠 `inner` 的锁会有 TOCTOU 窗口 —— 两个并发调用能同时穿过检查,
    /// 结果起出两个 dsh(实测发生过:两个进程相差 33ms)。
    lifecycle: Mutex<()>,
    /// 无锁的 pid 快照,给退出时的同步清理用。
    pid_snapshot: AtomicU32,
    app: AppHandle,
}

/// 从就绪行里解析出来的信息。
struct ReadyInfo {
    url: String,
    port: u16,
    token: String,
}

/// 解析 `dsh web: http://127.0.0.1:57710/?token=xxx`。
///
/// 前缀按实测固定为 `dsh web: `;解析失败就当作普通日志行,不影响启动。
fn parse_ready_line(line: &str) -> Option<ReadyInfo> {
    const PREFIX: &str = "dsh web: ";
    let idx = line.find(PREFIX)?;
    let raw = line[idx + PREFIX.len()..].trim();
    let parsed = url::Url::parse(raw).ok()?;
    let port = parsed.port()?;
    let token = parsed
        .query_pairs()
        .find(|(k, _)| k == "token")
        .map(|(_, v)| v.to_string())?;
    Some(ReadyInfo {
        url: raw.to_string(),
        port,
        token,
    })
}

// ---- 进程组管理 ----
//
// `dsh`(尤其是 npx 兑底路径)是「npx → npm exec → node dsh」三层进程树。
// 只杀直接子进程会把真正的 dsh 服务器(孙进程)留下变成孤儿 —— 实测确认过:
// 杀掉 npx 后,`node .../.bin/dsh` 仍活着并继续监听端口。
// 因此 spawn 时把子进程放进**独立进程组**,停止时向整个组发信号。

/// 让子进程自成进程组组长(pgid == pid)。仅 Unix。
fn spawn_in_own_group(cmd: &mut Command) {
    use std::os::unix::process::CommandExt;
    cmd.as_std_mut().process_group(0);
}

/// 向整个进程组发信号(负 pid 表示组)。
fn signal_group(pgid: u32, sig: i32) {
    unsafe { libc::kill(-(pgid as i32), sig) };
}

/// 进程组里是否还有活着的进程。
fn group_alive(pgid: u32) -> bool {
    unsafe { libc::kill(-(pgid as i32), 0) == 0 }
}

async fn wait_pid_gone(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if !group_alive(pid) {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// 向进程组发 SIGTERM 并等待;返回是否已全部退出。
async fn terminate(pid: u32, grace: Duration) -> bool {
    signal_group(pid, libc::SIGTERM);
    wait_pid_gone(pid, grace).await
}

// ---- 残留清理 ----
//
// 应用被强杀(崩溃 / SIGKILL / 开发期重启)时,Rust 的退出钩子不会执行,
// 子进程组会变成孤儿并继续占端口。所以把进程组 id 落盘,下次启动前先扫一遍。

fn pidfile_write(pgid: u32) {
    let path = paths::pidfile();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, pgid.to_string());
}

fn pidfile_clear() {
    let _ = std::fs::remove_file(paths::pidfile());
}

/// 取出指定进程组里的 pid 列表(`/usr/bin/pgrep -g`)。
fn group_pids(pgid: u32) -> Vec<u32> {
    let Ok(out) = std::process::Command::new("/usr/bin/pgrep")
        .args(["-g", &pgid.to_string()])
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse().ok())
        .collect()
}

/// 启动前清理上一次遗留的后端。
///
/// 会先用 `ps` 确认目标进程确实是跑我们这个 profile 的 dsh —— 防止 pgid 被系统
/// 复用后误杀无关进程。返回被清理的进程组描述(仅用于记日志)。
fn cleanup_stale_backend() -> Option<String> {
    let text = std::fs::read_to_string(paths::pidfile()).ok()?;
    let Ok(pgid) = text.trim().parse::<u32>() else {
        pidfile_clear();
        return None;
    };
    if !group_alive(pgid) {
        pidfile_clear();
        return None;
    }

    let pids = group_pids(pgid);
    if pids.is_empty() {
        pidfile_clear();
        return None;
    }
    let list = pids
        .iter()
        .map(|p| p.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &list])
        .output()
    else {
        return None;
    };
    let listing = String::from_utf8_lossy(&out.stdout).to_string();
    let ours = listing
        .lines()
        .any(|l| l.contains("--profile") && l.contains(paths::PROFILE_NAME));
    if !ours {
        // pgid 被复用了,不是我们的进程。
        pidfile_clear();
        return None;
    }

    signal_group(pgid, libc::SIGTERM);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if !group_alive(pgid) {
            break;
        }
        std::thread::sleep(Duration::from_millis(150));
    }
    if group_alive(pgid) {
        signal_group(pgid, libc::SIGKILL);
    }
    pidfile_clear();
    Some(format!(
        "已清理上次遗留的 dsh 后端(进程组 {pgid},{} 个进程)",
        pids.len()
    ))
}

// ---- token → cookie 交换 ----
//
// 为什么不让 WebView 自己走那个 303:实测在 WKWebView 里 cookie 没有落地
// (`cookies_for_url` 返回 0),页面停在 401「dsh web authentication required」。
// dsh 自己的提示也说明了这点 —— 「reopen the URL printed by dsh web」。
// 这件事依赖浏览器的 cookie 策略(第三方 cookie / SameSite 在跳转时的行为),
// 不可控;所以改成自己发一个最简 HTTP 请求把 token 换成 cookie,再显式注入窗口。
// 只取 Set-Cookie 的 name=value,不管其它属性 —— 属性由我们构造时决定。
fn exchange_with_cookie(url: &url::Url) -> Option<(String, String)> {
    use std::io::{Read, Write};

    let host = url.host_str()?;
    let port = url.port()?;
    let mut target = url.path().to_string();
    if let Some(query) = url.query() {
        target.push('?');
        target.push_str(query);
    }

    let mut stream = std::net::TcpStream::connect((host, port)).ok()?;
    let timeout = Some(Duration::from_secs(5));
    stream.set_read_timeout(timeout).ok()?;
    stream.set_write_timeout(timeout).ok()?;

    // 故意不发 Origin —— 围栏对无 Origin 的客户端放行(实测)。
    let request = format!(
        "GET {target} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).ok()?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;

    for line in String::from_utf8_lossy(&raw).lines() {
        let trimmed = line.trim_start();
        let bytes = trimmed.as_bytes();
        // 字节级比较,避免多字节字符上切片越界
        if bytes.len() > 11 && bytes[..11].eq_ignore_ascii_case(b"set-cookie:") {
            let pair = trimmed[11..].split(';').next()?.trim();
            let (name, value) = pair.split_once('=')?;
            return Some((name.trim().to_string(), value.trim().to_string()));
        }
    }
    None
}

fn build_cookie(host: &str, name: &str, value: &str) -> tauri::webview::cookie::Cookie<'static> {
    use tauri::webview::cookie::{Cookie, SameSite};
    let mut cookie = Cookie::new(name.to_string(), value.to_string());
    cookie.set_path("/");
    cookie.set_domain(host.to_string());
    cookie.set_http_only(true);
    // 与服务端一致。注入后我们马上导航到同源的根地址,同站请求会带上它。
    cookie.set_same_site(SameSite::Strict);
    cookie
}

/// 页面自报探针:让窗口里的页面把标题与正文头一段发回本地一个临时端口。
///
/// 存在的理由:判断“界面到底加载成功没有”很不好做 —— 回读 URL 只能说明跳转发生了
/// (之前就是被这个不充分的判据骗过),cookie 数在 macOS 上又不可靠(`cookies_for_url`
/// 实测会误报 0)。直接让页面自报内容,才是确定的信号。
///
/// 用 `DHDESKTOP_PAGE_PROBE=1` 开启,平时不监听端口。
async fn page_report_probe(window: tauri::WebviewWindow, tag: &'static str) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let Ok(listener) = tokio::net::TcpListener::bind("127.0.0.1:0").await else {
        return;
    };
    let Ok(addr) = listener.local_addr() else { return };
    let port = addr.port();

    // 页面与探针不同源,所以这是一次跨源请求:响应会被浏览器拦下,
    // 但请求本身已经发出去了 —— 我们要的就是服务端收到的那一发。
    let js = format!(
        "try{{fetch('http://127.0.0.1:{port}/report?tag={tag}&text='+encodeURIComponent((document.title||'')+' :: '+(document.body?document.body.innerText.slice(0,160):'(no body)'))).catch(()=>{{}})}}catch(e){{}}"
    );
    if let Err(e) = window.eval(&js) {
        println!("[probe] eval 失败: {e}");
        return;
    }

    if let Ok(Ok((mut sock, _))) = tokio::time::timeout(Duration::from_secs(6), listener.accept()).await {
        let mut buf = vec![0u8; 8192];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        if let Some(line) = request.lines().next() {
            println!("[probe] {tag} 页面自报: {}", percent_decode(line));
        }
        let _ = sock
            .write_all(
                b"HTTP/1.1 204 No Content\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
    } else {
        println!("[probe] {tag} 页面没有任何自报(可能没加载 JS 环境)");
    }
}

/// 只处理 %XX 与 '+' 的简易解码,够看报告文本用。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 自测只跑一次。
///
/// 必须是一次性的:每次「打开 dsh 界面」都会挂一个新的探针任务,
/// 而探针里又跑自测、自测又调 open_dsh_ui —— 不加守卫会无限循环。
static SELFTEST_REOPEN_DONE: AtomicBool = AtomicBool::new(false);

/// 调试开关:控制台诊断输出(包括 cookie 明细与页面自报探针)。
fn debug_enabled() -> bool {
    std::env::var("DHDESKTOP_DEBUG").is_ok() || std::env::var("DHDESKTOP_PAGE_PROBE").is_ok()
}

/// 清掉历史遗留的授权 cookie。
///
/// 每次运行端口都变,cookie 名也会变(服务端用 authority 的哈希命名),
/// 不清理就会在 WebView 存储里越积越多(实测一次调试就攒了 14 个)。
/// 只删我们自己的前缀,不碰其它 cookie。
fn purge_stale_auth_cookies(window: &tauri::WebviewWindow) {
    let Ok(cookies) = window.cookies() else { return };
    for cookie in cookies {
        if cookie.name().starts_with("dsh-auth-") {
            let _ = window.delete_cookie(cookie);
        }
    }
}

#[allow(dead_code)]
fn _assert_manager_send_sync() {
    fn f<T: Send + Sync>() {}
    f::<BackendManager>();
}

/// 「打开 dsh 界面」这一步的结果:待写日志的消息、是否成功、以及根地址(供探针用)。
struct OpenOutcome {
    messages: Vec<String>,
    opened: bool,
    root: Option<url::Url>,
}

impl BackendManager {
    pub fn new(app: AppHandle) -> Self {
        Self {
            inner: Mutex::new(Inner {
                status: BackendStatus::Idle,
                running: None,
                pending_pid: None,
                stopping: false,
                logs: VecDeque::new(),
                seq: 0,
            }),
            lifecycle: Mutex::new(()),
            pid_snapshot: AtomicU32::new(0),
            app,
        }
    }

    pub async fn status(&self) -> BackendStatus {
        self.inner.lock().await.status.clone()
    }

    pub async fn logs(&self) -> Vec<LogLine> {
        self.inner.lock().await.logs.iter().cloned().collect()
    }

    fn emit_status(&self, inner: &mut Inner, status: BackendStatus) {
        inner.status = status.clone();
        // 终端也打一份:从 Finder 启动时看不到,但 tauri dev / 命令行启动时是唯一的
        // 事后诊断线索(前端日志只在窗口里。)
        println!("[dsh] status -> {status:?}");
        let _ = self.app.emit(EVENT_STATUS, status);
    }

    fn emit_log(&self, inner: &mut Inner, stream: &str, text: String) {
        println!("[dsh:{stream}] {text}");
        inner.seq += 1;
        let line = LogLine {
            seq: inner.seq,
            stream: stream.to_string(),
            text,
            at_ms: now_ms(),
        };
        inner.logs.push_back(line.clone());
        while inner.logs.len() > MAX_LOG_LINES {
            inner.logs.pop_front();
        }
        let _ = self.app.emit(EVENT_LOG, line);
    }

    async fn log(&self, stream: &str, text: String) {
        let mut inner = self.inner.lock().await;
        self.emit_log(&mut inner, stream, text);
    }

    /// 打开(或聚焦)dsh 界面窗口。
    ///
    /// 为什么必须独立窗口、且第一次加载就带授权:见 `docs/recon-dsh.md` 第 5 节。
    /// 简述:从我们自己的页面导航过去属跨站导航,dsh 那枚 `SameSite=Strict` 的授权
    /// cookie 不会被带上,页面就停在 401。
    ///
    /// 结构上刻意拆成「同步核心 + 异步外壳」:所有窗口/网络操作都在同步函数里完成,
    /// async 外壳只负责写日志 —— 这样**没有任何窗口对象被跨越 `await` 持有**,
    /// future 必然是 Send(之前正是在这点上反复编译不过)。
    async fn open_backend_window(&self, url: &str) {
        let outcome = self.open_backend_window_sync(url);
        for message in outcome.messages {
            self.log("system", message).await;
        }
        if outcome.opened {
            self.spawn_window_probe(outcome.root);
        }
    }

    /// 同步核心:解析地址 → 换 cookie → 注入 → 建/复用窗口 → 显示。
    fn open_backend_window_sync(&self, url: &str) -> OpenOutcome {
        let mut messages: Vec<String> = Vec::new();
        let Ok(parsed) = url::Url::parse(url) else {
            messages.push(format!("后端地址无法解析:{url}"));
            return OpenOutcome { messages, opened: false, root: None };
        };
        let root = url::Url::parse(&format!("{}/", parsed.origin().ascii_serialization())).ok();
        let has_token = parsed.query_pairs().any(|(k, _)| k == "token");

        // 自己把 token 换成 cookie(原因见 exchange_with_cookie 的注释)
        let cookie = if has_token {
            match exchange_with_cookie(&parsed) {
                Some((name, value)) => {
                    messages.push(format!("已取得授权 cookie:{name}"));
                    Some(build_cookie(
                        parsed.host_str().unwrap_or("127.0.0.1"),
                        &name,
                        &value,
                    ))
                }
                None => {
                    messages.push("token 交换失败,退回直接打开带 token 的地址".to_string());
                    None
                }
            }
        } else {
            None
        };

        // 拿到 cookie 就用干净的根地址(不再需要 token)
        let target = match (&cookie, &root) {
            (Some(_), Some(r)) => r.clone(),
            _ => parsed.clone(),
        };

        // 关键时序:**先把 cookie 写进 WebView 的 cookie 存储,再创建 dsh 窗口**。
        // cookie 存储是应用级的(WKWebView 默认 data store),所以先写进已有窗口的存储,
        // 新窗口第一次加载就带授权。顺序反了实测会停在 401。
        if let Some(cookie) = cookie.clone() {
            match self
                .app
                .get_webview_window(WINDOW_MAIN)
                .or_else(|| self.app.get_webview_window(WINDOW_BACKEND))
            {
                Some(store) => {
                    // 先清旧(含本次之前写入的),再写新 —— 保证留下的是当前这枚
                    purge_stale_auth_cookies(&store);
                    if let Err(e) = store.set_cookie(cookie) {
                        messages.push(format!("注入授权 cookie 失败: {e}"));
                    }
                }
                None => messages.push("没有可用窗口写入 cookie".to_string()),
            }
        }

        let window = match self.app.get_webview_window(WINDOW_BACKEND) {
            Some(existing) => existing,
            None => {
                let built = WebviewWindowBuilder::new(
                    &self.app,
                    WINDOW_BACKEND,
                    WebviewUrl::External(target.clone()),
                )
                .title("DeepSeek Harness")
                .inner_size(1280.0, 832.0)
                .min_inner_size(960.0, 600.0)
                // 先隐藏:避免用户看到未授权的那一次加载
                .visible(false)
                .build();
                match built {
                    Ok(w) => w,
                    Err(e) => {
                        messages.push(format!("创建 dsh 窗口失败: {e}"));
                        return OpenOutcome { messages, opened: false, root };
                    }
                }
            }
        };

        match &cookie {
            // 有 cookie:再向 dsh 窗口自己写一次(防存储不共享),然后导航到干净的根地址
            Some(cookie) => {
                if let Err(e) = window.set_cookie(cookie.clone()) {
                    messages.push(format!("向 dsh 窗口注入 cookie 失败: {e}"));
                }
                if let Some(r) = root.clone() {
                    if let Err(e) = window.navigate(r) {
                        messages.push(format!("导航 dsh 窗口失败: {e}"));
                    }
                }
            }
            // 没有 cookie:只能用带 token 的地址
            None => {
                if let Err(e) = window.navigate(parsed) {
                    messages.push(format!("导航 dsh 窗口失败: {e}"));
                }
            }
        }

        let _ = window.show();
        let _ = window.set_focus();
        messages.push(format!("dsh 界面已打开:{target}"));

        // 收起启动页:正常使用时用户只应该看到 dsh 一个窗口。
        // 需要控制台时用菜单(⌘⇧P)重新拿出来。
        if let Some(main) = self.app.get_webview_window(WINDOW_MAIN) {
            let _ = main.hide();
        }

        OpenOutcome { messages, opened: true, root }
    }

    /// 6 秒后回读窗口状态、cookie 明细与页面自报。
    ///
    /// 存在的意义:判断「界面到底加载成功没有」不能靠间接信号 —— 回读 URL 只能说明
    /// 跳转发生了(401 页面也一样),`cookies_for_url` 在 macOS 上实测恒为 0。
    /// 详见 `docs/recon-dsh.md` 的「验证手段的教训」。
    fn spawn_window_probe(&self, probe_url: Option<url::Url>) {
        let app = self.app.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(6)).await;
            let Some(window) = app.get_webview_window(WINDOW_BACKEND) else {
                return;
            };
            let url = window.url().map(|u| u.to_string()).unwrap_or_default();
            let by_url = probe_url
                .clone()
                .and_then(|u| window.cookies_for_url(u).ok())
                .map(|c| c.len());
            let all = window.cookies().map(|c| c.len());
            println!("[dsh] 窗口 url={url} 根地址 cookie 数={by_url:?} 全部 cookie 数={all:?}");

            if debug_enabled() {
                if let Ok(cookies) = window.cookies() {
                    for c in cookies {
                        println!(
                            "[dsh]   cookie name={} domain={:?} path={:?} sameSite={:?} secure={}",
                            c.name(),
                            c.domain(),
                            c.path(),
                            c.same_site(),
                            c.secure().unwrap_or(false),
                        );
                    }
                }
            }

            // 让页面自己报告渲染了什么(见 page_report_probe 的说明)
            if std::env::var("DHDESKTOP_PAGE_PROBE").is_ok() {
                page_report_probe(window.clone(), "dsh").await;
            }

            // 验证 UX 约定:正常运行时用户应该只看到 dsh 窗口
            if debug_enabled() {
                let main_visible = app
                    .get_webview_window(WINDOW_MAIN)
                    .and_then(|w| w.is_visible().ok());
                let dsh_visible = app
                    .get_webview_window(WINDOW_BACKEND)
                    .and_then(|w| w.is_visible().ok());
                println!("[dsh] 窗口可见性 main={main_visible:?} dsh={dsh_visible:?}");
            }

            // 自测:模拟「关掉窗口 → 再打开」这条路径。
            // 用户报过的故障就是这里,而我又无法代用户点击,所以让它自己走一遍。
            if std::env::var("DHDESKTOP_SELFTEST_REOPEN").is_ok()
                && !SELFTEST_REOPEN_DONE.swap(true, Ordering::SeqCst)
            {
                if let Some(w) = app.get_webview_window(WINDOW_BACKEND) {
                    let _ = w.hide();
                }
                println!("[selftest] 已隐藏 dsh 窗口(= 用户关窗),1 秒后模拟点 Dock 图标重开");
                tokio::time::sleep(Duration::from_secs(1)).await;
                let manager = app
                    .try_state::<Arc<BackendManager>>()
                    .map(|state| Arc::clone(&state));
                match manager {
                    Some(manager) => {
                        manager.open_dsh_ui().await;
                        let visible = app
                            .get_webview_window(WINDOW_BACKEND)
                            .and_then(|w| w.is_visible().ok());
                        println!("[selftest] 重开后 dsh 窗口可见性={visible:?}");
                    }
                    None => println!("[selftest] 拿不到 BackendManager 状态"),
                }
            }
        });
    }

    /// 把启动页露出来 —— 后端不可用或出了问题时用户需要看到状态。
    /// 正常启动流程不会叫它:那时用户只应该看到 dsh 界面。
    pub fn show_main_window(&self) {
        if let Some(main) = self.app.get_webview_window(WINDOW_MAIN) {
            let _ = main.show();
            let _ = main.set_focus();
        }
    }

    /// 关闭 dsh 界面窗口(后端停止/退出时),并把启动页露出来
    /// —— 这时用户需要看到状态与错误原因。
    ///
    /// 注意是 **hide 而不是 close**:窗口对象保留下来,重开时只需 show(),
    /// 不用重建、不用重新鉴权(重建失败就会变成“打不开”)。
    fn close_backend_window(&self) {
        if let Some(window) = self.app.get_webview_window(WINDOW_BACKEND) {
            let _ = window.hide();
        }
        self.show_main_window();
    }

    /// 打开或聚焦 dsh 界面(菜单「打开 dsh 界面」/ 点 Dock 图标)。
    ///
    /// 兜底原则:**这个动作必须产生一个可见窗口**。
    /// 否则用户面对的是一片空桌面,而错误信息还打在隐藏着的状态页里 —— 那就是
    /// 「关掉之后再也打不开」的观感。
    pub async fn open_dsh_ui(&self) {
        // 先把地址取出来 —— 直接在 match 上锁会让锁守卫活过整个 match,
        // 使这个 future 变成 !Send(tokio::spawn 就不收了)。
        let ready_url = self.inner.lock().await.status.ready_url();
        match ready_url {
            Some(url) => {
                self.open_backend_window(&url).await;
                let visible = self
                    .app
                    .get_webview_window(WINDOW_BACKEND)
                    .and_then(|w| w.is_visible().ok())
                    .unwrap_or(false);
                if !visible {
                    self.show_main_window();
                }
            }
            None => {
                self.log("system", "后端未运行,无法打开 dsh 界面".to_string())
                    .await;
                self.show_main_window();
            }
        }
    }

    /// 启动后端。已在运行或正在启动时直接返回当前状态。
    pub async fn start(self: &Arc<Self>) -> BackendStatus {
        let _guard = self.lifecycle.lock().await;
        self.start_inner().await
    }

    async fn start_inner(self: &Arc<Self>) -> BackendStatus {
        {
            let inner = self.inner.lock().await;
            match &inner.status {
                s if s.is_ready() => return s.clone(),
                BackendStatus::Starting { .. } | BackendStatus::Stopping => {
                    return inner.status.clone()
                }
                _ => {}
            }
        }

        // 清理上次异常退出遗留的后端(强杀 / 崩溃时退出钩子不会执行)。
        if let Some(desc) = cleanup_stale_backend() {
            let mut inner = self.inner.lock().await;
            self.emit_log(&mut inner, "system", desc);
        }

        // 解析启动方式
        let resolved = match locate::resolve() {
            Some(r) => r,
            None => {
                let message = "未找到可用的 dsh:PATH 与常见安装位置都没有,也没有 npx。\
                               请先安装(例如 npm i -g @deepseek-ai/dsh),或用环境变量 DHDESKTOP_DSH_BIN 指定路径。"
                    .to_string();
                let mut inner = self.inner.lock().await;
                self.emit_log(&mut inner, "system", message.clone());
                self.emit_status(&mut inner, BackendStatus::Failed { message: message.clone() });
                drop(inner);
                self.show_main_window();
                return BackendStatus::Failed { message };
            }
        };

        let profile_ready = paths::profile_manifest().is_file();

        {
            let mut inner = self.inner.lock().await;
            self.emit_status(
                &mut inner,
                BackendStatus::Starting {
                    program: resolved.display.clone(),
                    source: resolved.source_label.clone(),
                },
            );
            self.emit_log(
                &mut inner,
                "system",
                format!("使用 {} ({}) 启动 dsh", resolved.display, resolved.source_label),
            );
            self.emit_log(
                &mut inner,
                "system",
                format!(
                    "DSH_HOME={}{}",
                    paths::dsh_home().display(),
                    if profile_ready {
                        format!(",复用已有 profile {}", paths::PROFILE_NAME)
                    } else {
                        format!(",首次启动:从内置模板 {} 初始化 profile", paths::PROFILE_TEMPLATE)
                    }
                ),
            );
        }

        let mut cmd = Command::new(&resolved.program);
        cmd.args(&resolved.args);
        cmd.arg("--profile").arg(paths::PROFILE_NAME);
        if !profile_ready {
            cmd.arg("--from-default-profile").arg(paths::PROFILE_TEMPLATE);
        }
        cmd.args(["--no-open", "--port", "0", "--host", "127.0.0.1"]);
        cmd.env("DSH_HOME", paths::dsh_home());
        // 补全 PATH,否则 Finder 启动时子进程找不到 node(见 locate::child_path_env)。
        if let Some(path) = locate::child_path_env() {
            cmd.env("PATH", path);
        }
        cmd.stdin(Stdio::null());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        // 自成进程组,使停止时能连孙进程一起收掉(见 spawn_in_own_group 的注释)。
        spawn_in_own_group(&mut cmd);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let message = format!("启动 dsh 失败: {e}");
                let mut inner = self.inner.lock().await;
                self.emit_log(&mut inner, "system", message.clone());
                self.emit_status(&mut inner, BackendStatus::Failed { message: message.clone() });
                drop(inner);
                self.show_main_window();
                return BackendStatus::Failed { message };
            }
        };

        let Some(pid) = child.id() else {
            let message = "启动 dsh 失败:拿不到子进程 pid".to_string();
            let mut inner = self.inner.lock().await;
            self.emit_status(&mut inner, BackendStatus::Failed { message: message.clone() });
            drop(inner);
            self.show_main_window();
            return BackendStatus::Failed { message };
        };

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        {
            let mut inner = self.inner.lock().await;
            inner.pending_pid = Some(pid);
            inner.stopping = false;
            self.emit_log(&mut inner, "system", format!("dsh 子进程已启动 (pid {pid})"));
        }
        self.pid_snapshot.store(pid, Ordering::SeqCst);
        pidfile_write(pid);

        // ---- stdout:日志 + 就绪信号 ----
        if let Some(stdout) = stdout {
            let me = Arc::clone(self);
            let program = resolved.display.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let ready = parse_ready_line(&line);
                    let mut inner = me.inner.lock().await;
                    me.emit_log(&mut inner, "stdout", line.clone());
                    if let Some(info) = ready {
                        let _ = info.token; // token 已经包含在 url 里,前端直接用 url
                        let started_at_ms = now_ms();
                        inner.running = Some(Running { pid });
                        inner.pending_pid = None;
                        me.emit_status(
                            &mut inner,
                            BackendStatus::Ready {
                                url: info.url.clone(),
                                port: info.port,
                                pid,
                                started_at_ms,
                            },
                        );
                        drop(inner);
                        me.log("system", format!("dsh 后端就绪:{program}")).await;
                        me.open_backend_window(&info.url).await;
                    }
                }
            });
        }

        // ---- stderr:只记日志 ----
        if let Some(stderr) = stderr {
            let me = Arc::clone(self);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut inner = me.inner.lock().await;
                    me.emit_log(&mut inner, "stderr", line);
                }
            });
        }

        // ---- 就绪超时 ----
        {
            let me = Arc::clone(self);
            tokio::spawn(async move {
                tokio::time::sleep(READY_TIMEOUT).await;
                let mut inner = me.inner.lock().await;
                if matches!(inner.status, BackendStatus::Starting { .. }) {
                    let message = format!(
                        "dsh 启动超时({}s 内没有就绪),详见日志",
                        READY_TIMEOUT.as_secs()
                    );
                    me.emit_log(&mut inner, "system", message.clone());
                    me.emit_status(&mut inner, BackendStatus::Failed { message });
                    inner.pending_pid = None;
                    drop(inner);
                    let _ = terminate(pid, TERM_GRACE).await;
                    signal_group(pid, libc::SIGKILL);
                    me.pid_snapshot.store(0, Ordering::SeqCst);
                    me.close_backend_window();
                }
            });
        }

        // ---- 退出观察 ----
        {
            let me = Arc::clone(self);
            tokio::spawn(async move {
                let status = child.wait().await;
                let code = status.ok().and_then(|s| s.code());
                me.pid_snapshot.compare_exchange(pid, 0, Ordering::SeqCst, Ordering::SeqCst).ok();

                let mut inner = me.inner.lock().await;
                inner.running = None;
                inner.pending_pid = None;
                if inner.stopping {
                    inner.stopping = false;
                    me.emit_log(&mut inner, "system", "dsh 后端已停止".to_string());
                    me.emit_status(&mut inner, BackendStatus::Idle);
                } else if inner.status.is_ready() {
                    let message = format!("dsh 后端已退出{}", code.map(|c| format!("(退出码 {c})")).unwrap_or_default());
                    me.emit_log(&mut inner, "system", message.clone());
                    me.emit_status(&mut inner, BackendStatus::Exited { code, message });
                } else {
                    let message = format!(
                        "dsh 启动失败{} —— 详见下方日志",
                        code.map(|c| format!("(退出码 {c})")).unwrap_or_default()
                    );
                    me.emit_log(&mut inner, "system", message.clone());
                    me.emit_status(&mut inner, BackendStatus::Failed { message });
                }
                drop(inner);
                me.close_backend_window();
            });
        }

        self.inner.lock().await.status.clone()
    }

    /// 停止后端。SIGTERM → 宽限 → SIGKILL。
    pub async fn stop(&self) -> BackendStatus {
        let _guard = self.lifecycle.lock().await;
        self.stop_inner().await
    }

    async fn stop_inner(&self) -> BackendStatus {
        let pid = {
            let mut inner = self.inner.lock().await;
            let pid = inner
                .running
                .as_ref()
                .map(|r| r.pid)
                .or(inner.pending_pid);
            let Some(pid) = pid else {
                inner.running = None;
                inner.pending_pid = None;
                inner.stopping = false;
                self.emit_status(&mut inner, BackendStatus::Idle);
                return BackendStatus::Idle;
            };
            inner.stopping = true;
            self.emit_status(&mut inner, BackendStatus::Stopping);
            pid
        };

        self.log("system", format!("正在停止 dsh 后端 (pid {pid})")).await;

        if !terminate(pid, TERM_GRACE).await {
            self.log(
                "system",
                format!("pid {pid} 在宽限期内未退出,强制 SIGKILL"),
            )
            .await;
            signal_group(pid, libc::SIGKILL);
            wait_pid_gone(pid, KILL_GRACE).await;
        }

        self.pid_snapshot.store(0, Ordering::SeqCst);
        pidfile_clear();
        {
            let mut inner = self.inner.lock().await;
            inner.running = None;
            inner.pending_pid = None;
            inner.stopping = false;
            self.emit_status(&mut inner, BackendStatus::Idle);
        }
        self.close_backend_window();
        BackendStatus::Idle
    }

    pub async fn restart(self: &Arc<Self>) -> BackendStatus {
        let _guard = self.lifecycle.lock().await;
        self.stop_inner().await;
        // 等端口/文件锁释放
        tokio::time::sleep(Duration::from_millis(400)).await;
        self.start_inner().await
    }

    /// 应用退出时的同步清理。用无锁 pid 快照,避免在退出路径上 await。
    pub fn kill_now(&self) {
        let pid = self.pid_snapshot.swap(0, Ordering::SeqCst);
        pidfile_clear();
        if pid == 0 {
            return;
        }
        // 向整个进程组发信号:npx 会再拉起孙进程 dsh,只杀直接子进程会留下孤儿。
        signal_group(pid, libc::SIGTERM);
        for _ in 0..8 {
            if !group_alive(pid) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        signal_group(pid, libc::SIGKILL);
    }
}
