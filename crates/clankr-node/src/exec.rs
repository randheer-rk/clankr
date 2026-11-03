//! The node's side of `clank exec` (ADR 0030). clankrd lists sessions for this node's
//! instances in heartbeats; for each, the node streams input down and output up over its mTLS
//! client and runs `kennel exec <instance>` on a pty (or pipes). The bytes are kennel's exec
//! frames, `[kind, len u32 BE, bytes]`: up, `1` output and `2` the exit status; down, `1`
//! stdin and `3` end of stdin.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use serde::Deserialize;

use crate::client::NodeTls;

#[derive(Debug, Clone, Deserialize)]
pub struct ExecRequest {
    pub session: String,
    pub namespace: String,
    pub instance: String,
    pub request: Req,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Req {
    pub command: Vec<String>,
    #[serde(default)]
    pub tty: bool,
    #[serde(default)]
    pub rows: Option<u16>,
    #[serde(default)]
    pub cols: Option<u16>,
}

fn frame(kind: u8, data: &[u8]) -> Bytes {
    let mut v = Vec::with_capacity(5 + data.len());
    v.push(kind);
    v.extend_from_slice(&(data.len() as u32).to_be_bytes());
    v.extend_from_slice(data);
    Bytes::from(v)
}

/// Runs one session to completion (errors end it; the user sees no exit frame).
pub async fn serve(base: String, tls: NodeTls, node: String, kennel: PathBuf, kennel_root: Option<PathBuf>, x: ExecRequest) -> anyhow::Result<()> {
    // a session can last as long as someone's shell does
    let http = tls.client(Duration::from_secs(12 * 3600));
    let url = |leg: &str| format!("{base}/v1/nodes/{node}/exec/{}/{leg}", x.session);
    let input = http.get(url("input")).send().await?.error_for_status()?;

    let mut cmd = std::process::Command::new(&kennel);
    if let Some(r) = &kennel_root {
        cmd.arg("--root").arg(r);
    }
    cmd.args(["exec", &x.instance]);
    if x.request.tty {
        cmd.arg("-t");
    }
    cmd.arg("--").args(&x.request.command);
    let (mut child, mut stdin, mut output): (std::process::Child, Option<Box<dyn Write + Send>>, Box<dyn Read + Send>) = if x.request.tty {
        let ws = nix::pty::Winsize { ws_row: x.request.rows.unwrap_or(24), ws_col: x.request.cols.unwrap_or(80), ws_xpixel: 0, ws_ypixel: 0 };
        let p = nix::pty::openpty(Some(&ws), None)?;
        use std::os::fd::AsRawFd;
        let slave = p.slave.as_raw_fd();
        cmd.stdin(p.slave.try_clone()?).stdout(p.slave.try_clone()?).stderr(p.slave.try_clone()?);
        // kennel exec sees a real terminal: its own session with this pty as controlling tty
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut cmd, move || {
                nix::libc::setsid();
                nix::libc::ioctl(slave, nix::libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        // the Command holds its stdio copies: drop them, or the output never reaches EOF/EIO
        drop(cmd);
        drop(p.slave);
        let master = std::fs::File::from(p.master);
        (child, Some(Box::new(master.try_clone()?)), Box::new(master))
    } else {
        let (r, w) = std::io::pipe()?;
        cmd.stdin(std::process::Stdio::piped()).stdout(w.try_clone()?).stderr(w);
        let mut child = cmd.spawn()?;
        drop(cmd);
        let stdin = child.stdin.take().map(|s| Box::new(s) as Box<dyn Write + Send>);
        (child, stdin, Box::new(r))
    };
    tracing::info!(session = %x.session, instance = %x.instance, tty = x.request.tty, "exec session started");

    // output (and finally the status) up to clankrd
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(64);
    let reader = {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            // a pty reports EIO once the last process using it has gone
            while let Ok(n) = output.read(&mut buf) {
                if n == 0 || tx.blocking_send(frame(1, &buf[..n])).is_err() {
                    break;
                }
            }
        })
    };
    let body = reqwest::Body::wrap_stream(futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (Ok::<_, std::io::Error>(b), rx)) }));
    let upload = tokio::spawn(http.post(url("output")).body(body).send());

    // the user's input down to the process
    let (in_tx, in_rx) = std::sync::mpsc::channel::<Option<Vec<u8>>>();
    let writer = std::thread::spawn(move || {
        while let Ok(m) = in_rx.recv() {
            match (m, stdin.as_mut()) {
                (Some(d), Some(w)) => {
                    let _ = w.write_all(&d).and_then(|_| w.flush());
                }
                (None, _) => stdin = None, // end of stdin (a pty stays open: ^D is data)
                _ => {}
            }
        }
    });
    let pump = tokio::spawn(async move {
        let mut s = input.bytes_stream();
        let mut buf: Vec<u8> = vec![];
        while let Some(Ok(chunk)) = s.next().await {
            buf.extend_from_slice(&chunk);
            while buf.len() >= 5 {
                let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
                if buf.len() < 5 + len {
                    break;
                }
                let (kind, data) = (buf[0], buf[5..5 + len].to_vec());
                buf.drain(..5 + len);
                let _ = in_tx.send(match kind {
                    1 => Some(data),
                    _ => None,
                });
            }
        }
    });

    let status = tokio::task::spawn_blocking(move || child.wait()).await??;
    let _ = tokio::task::spawn_blocking(move || reader.join()).await;
    let code = status.code().unwrap_or(1);
    let _ = tx.send(frame(2, &code.to_be_bytes())).await;
    drop(tx);
    let _ = upload.await;
    pump.abort();
    drop(writer);
    tracing::info!(session = %x.session, code, "exec session ended");
    Ok(())
}
