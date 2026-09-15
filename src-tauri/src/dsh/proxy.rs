//! dsh 的本地反向代理。
//!
//! ## 为什么需要它
//!
//! dsh 的 `/api` 有一道"浏览器信任围栏":它按 `Host` / `Origin` 判断请求来源,
//! 非本服务 authority 的 Origin 一律 **403**;同时所有请求都要带那枚
//! `SameSite=Strict; HttpOnly` 的签名 cookie(见 `docs/recon-dsh.md` 第 5 节)。
//!
//! 我们的界面跑在 `tauri://localhost`,天然不满足这两条。而**非浏览器客户端不受
//! 围栏限制**(实测:不带 Origin 时围栏放行)—— 所以让 Rust 站在中间:
//!
//! ```text
//! 我们的 Vue 界面 ──HTTP/WS──▶ Rust 代理 ──补 Host+Cookie──▶ dsh (127.0.0.1:<port>)
//! ```
//!
//! 代理只做三件事:改写请求头、转发、双向按字节搬运。
//! WebSocket 不需要解析帧 —— HTTP/1.1 upgrade 成功后就是一个字节流。

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// 一次代理会话所需的上游信息。
pub struct Upstream {
    /// dsh 实际监听的端口。
    pub port: u16,
    /// 已换到的授权 cookie(`name=value`,不含属性)。
    pub cookie: String,
}

/// 启动代理,返回它监听的本地端口。
///
/// 端口用 0 让系统挑,避免与用户其它服务冲突。
pub async fn serve(upstream: Arc<Upstream>) -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    println!("[dsh] 代理监听 127.0.0.1:{port}");
    tokio::spawn(async move {
        loop {
            let Ok((client, _)) = listener.accept().await else {
                continue;
            };
            let upstream = Arc::clone(&upstream);
            tokio::spawn(async move {
                if let Err(e) = handle(client, upstream).await {
                    println!("[dsh] 代理连接结束: {e}");
                }
            });
        }
    });
    Ok(port)
}

async fn handle(mut client: TcpStream, upstream: Arc<Upstream>) -> std::io::Result<()> {
    // 1. 读到请求头结束(\r\n\r\n)。之后的字节可能是 body,或 WS 帧。
    let mut head: Vec<u8> = Vec::with_capacity(2048);
    let mut byte = [0u8; 1];
    loop {
        let n = client.read(&mut byte).await?;
        if n == 0 {
            return Ok(());
        }
        head.push(byte[0]);
        if head.len() >= 4 && &head[head.len() - 4..] == b"\r\n\r\n" {
            break;
        }
        if head.len() > 64 * 1024 {
            return Ok(()); // 头部异常大,放弃
        }
    }
    let head_text = String::from_utf8_lossy(&head).to_string();
    let mut lines = head_text.split("\r\n");
    let request_line = lines.next().unwrap_or_default().to_string();

    // 2. 改写请求头。
    //
    //    Host / Origin 指向上游 authority —— 围栏按这两个头判断来源;
    //    Cookie 补上授权 —— 浏览器不会替我们带(cookie 是 SameSite=Strict 且属于别的 origin)。
    let authority = format!("127.0.0.1:{}", upstream.port);
    let mut out = String::new();
    out.push_str(&request_line);
    out.push_str("\r\n");
    let mut saw_origin = false;
    let mut saw_cookie = false;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (name, _value) = match line.split_once(':') {
            Some((n, v)) => (n.trim().to_ascii_lowercase(), v.trim()),
            None => continue,
        };
        match name.as_str() {
            "host" => out.push_str(&format!("Host: {authority}\r\n")),
            "origin" => {
                saw_origin = true;
                out.push_str(&format!("Origin: http://{authority}\r\n"));
            }
            "cookie" => {
                saw_cookie = true;
                out.push_str(&format!(
                    "Cookie: {}; {}\r\n",
                    upstream.cookie,
                    line.split_once(':').map(|(_, v)| v.trim()).unwrap_or("")
                ));
            }
            // 浏览器给代理发的 CORS 预检相关头对上游没有意义,丢弃
            "referer" => {}
            _ => out.push_str(&format!("{line}\r\n")),
        }
    }
    if !saw_origin {
        out.push_str(&format!("Origin: http://{authority}\r\n"));
    }
    if !saw_cookie {
        out.push_str(&format!("Cookie: {}\r\n", upstream.cookie));
    }
    out.push_str("\r\n");

    // 3. 连上游,把改写后的头送过去。
    let mut server = TcpStream::connect(("127.0.0.1", upstream.port)).await?;
    server.write_all(out.as_bytes()).await?;

    // 4. 双向按字节搬运。WS upgrade 之后这里就是纯字节流,不需要解析帧。
    let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
    Ok(())
}
