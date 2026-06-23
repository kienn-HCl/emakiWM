//! 名前付きパイプ IPC サーバ (FR-7.3, FR-7.4)。
//!
//! プロトコル: クライアントがテキスト 1 行のコマンド (CLI 引数と同じ構文) を送り、
//! サーバが JSON 1 行で応答して切断する。`state` のみ Core スレッドへの
//! 問い合わせ (WmEvent::Query) になり、それ以外はコマンドの投函のみ。
//! `subscribe` は接続を維持し、状態が変わるたびに JSON を 1 行ずつ配信する
//! (FR-7.4、Zebar 等のステータスバー向け)。
//! 購読中も他クライアントを受け付けるため、接続ごとにスレッドを立てる。

use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Storage::FileSystem::{
    FlushFileBuffers, ReadFile, WriteFile, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

use crate::events::{parse_command, WmEvent};

pub fn spawn_ipc_thread(tx: Sender<WmEvent>) {
    std::thread::spawn(move || serve(tx));
}

fn serve(tx: Sender<WmEvent>) {
    loop {
        unsafe {
            let pipe = CreateNamedPipeW(
                w!(r"\\.\pipe\emakiwm"),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                None,
            );
            if pipe.is_invalid() {
                tracing::error!("CreateNamedPipeW failed, IPC 停止");
                return;
            }

            // クライアント接続待ち (ブロック)
            if ConnectNamedPipe(pipe, None).is_err() {
                let _ = CloseHandle(pipe);
                continue;
            }

            // HANDLE は Send でないため生ポインタ値で渡す
            let raw = pipe.0 as isize;
            let tx = tx.clone();
            std::thread::spawn(move || client(raw, tx));
        }
    }
}

/// 1 接続ぶんの処理。subscribe 以外は 1 コマンド 1 応答で切断する。
fn client(raw: isize, tx: Sender<WmEvent>) {
    let pipe = HANDLE(raw as _);
    unsafe {
        let mut buf = [0u8; 4096];
        let mut read = 0u32;
        if ReadFile(pipe, Some(&mut buf), Some(&mut read), None).is_ok() && read > 0 {
            let cmd = String::from_utf8_lossy(&buf[..read as usize])
                .trim()
                .to_string();
            if cmd == "subscribe" {
                subscribe_loop(pipe, &tx);
            } else {
                let resp = handle(&cmd, &tx);
                let _ = WriteFile(pipe, Some(resp.as_bytes()), None, None);
                let _ = FlushFileBuffers(pipe);
            }
        }
        let _ = DisconnectNamedPipe(pipe);
        let _ = CloseHandle(pipe);
    }
}

/// FR-7.4: 購読モード。登録時の現在状態と、以後の変化を 1 行 1 JSON で流す。
/// クライアント切断 (WriteFile 失敗) または WM 終了 (チャネル切断) で終わる。
fn subscribe_loop(pipe: HANDLE, tx: &Sender<WmEvent>) {
    let (stx, srx) = mpsc::channel::<String>();
    if tx.send(WmEvent::Subscribe(stx)).is_err() {
        unsafe {
            let _ = WriteFile(
                pipe,
                Some(br#"{"error":"wm is shutting down"}"#.as_slice()),
                None,
                None,
            );
        }
        return;
    }
    while let Ok(mut line) = srx.recv() {
        line.push('\n');
        unsafe {
            if WriteFile(pipe, Some(line.as_bytes()), None, None).is_err() {
                return;
            }
            let _ = FlushFileBuffers(pipe);
        }
    }
}

fn handle(cmd: &str, tx: &Sender<WmEvent>) -> String {
    if cmd == "state" {
        let (rtx, rrx) = mpsc::channel();
        if tx.send(WmEvent::Query(rtx)).is_err() {
            return r#"{"error":"wm is shutting down"}"#.into();
        }
        return rrx
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| r#"{"error":"timeout"}"#.into());
    }
    match parse_command(cmd) {
        Some(ev) => {
            let _ = tx.send(ev);
            r#"{"ok":true}"#.into()
        }
        None => format!(r#"{{"error":"unknown command: {cmd}"}}"#),
    }
}
