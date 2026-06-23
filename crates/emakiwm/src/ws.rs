//! WebSocket 状態配信 (FR-7.5)。Zebar 等の webview ウィジェット向け。
//!
//! webview の JS は名前付きパイプを開けないため、localhost の WebSocket で
//! subscribe (FR-7.4) と同じ state JSON を接続時 + 変化時に push する。
//! 一方向配信なのでクライアントからのメッセージは ping/close のみ処理する。
//! 依存を増やさないため SHA-1 / Base64 / フレーム処理は最小実装。

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};

use crate::events::WmEvent;

pub fn spawn_ws_thread(port: u16, tx: Sender<WmEvent>) {
    std::thread::spawn(move || serve(port, tx));
}

fn serve(port: u16, tx: Sender<WmEvent>) {
    let listener = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            tracing::error!("WebSocket ポート {port} を確保できません: {e}");
            return;
        }
    };
    tracing::info!("WebSocket: ws://127.0.0.1:{port}/");
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = client(stream, tx);
        });
    }
}

fn client(mut stream: TcpStream, tx: Sender<WmEvent>) -> std::io::Result<()> {
    // HTTP リクエストをヘッダ終端まで読む
    let mut buf = Vec::new();
    let mut tmp = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 16 * 1024 {
            return Ok(());
        }
    }
    let req = String::from_utf8_lossy(&buf);
    let Some(key) = req.lines().find_map(|l| {
        let (name, v) = l.split_once(':')?;
        name.eq_ignore_ascii_case("sec-websocket-key")
            .then(|| v.trim().to_string())
    }) else {
        let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n");
        return Ok(());
    };
    let accept = handshake_accept(&key);
    stream.write_all(
        format!(
            "HTTP/1.1 101 Switching Protocols\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .as_bytes(),
    )?;

    // 購読登録 — IPC subscribe (FR-7.4) と同じ配信に乗る
    let (stx, srx) = mpsc::channel::<String>();
    if tx.send(WmEvent::Subscribe(stx)).is_err() {
        return Ok(());
    }

    // フレームの interleave を防ぐため書き込みは Mutex 越しに行う
    let writer = Arc::new(Mutex::new(stream.try_clone()?));
    {
        let writer = Arc::clone(&writer);
        std::thread::spawn(move || read_loop(stream, writer));
    }

    while let Ok(msg) = srx.recv() {
        let mut w = writer.lock().unwrap_or_else(|p| p.into_inner());
        if write_frame(&mut w, 0x1, msg.as_bytes()).is_err() {
            break; // クライアント切断 → 購読も終了 (sender drop で retain から外れる)
        }
    }
    Ok(())
}

/// クライアントからのフレームを処理する (ping 応答・close 検出のみ)。
fn read_loop(mut s: TcpStream, writer: Arc<Mutex<TcpStream>>) {
    loop {
        let mut hdr = [0u8; 2];
        if s.read_exact(&mut hdr).is_err() {
            break;
        }
        let opcode = hdr[0] & 0x0F;
        let masked = hdr[1] & 0x80 != 0;
        let mut len = (hdr[1] & 0x7F) as u64;
        if len == 126 {
            let mut b = [0u8; 2];
            if s.read_exact(&mut b).is_err() {
                break;
            }
            len = u16::from_be_bytes(b) as u64;
        } else if len == 127 {
            let mut b = [0u8; 8];
            if s.read_exact(&mut b).is_err() {
                break;
            }
            len = u64::from_be_bytes(b);
        }
        if len > 64 * 1024 {
            break; // 一方向配信なので大きな受信は想定外
        }
        let mut mask = [0u8; 4];
        if masked && s.read_exact(&mut mask).is_err() {
            break;
        }
        let mut payload = vec![0u8; len as usize];
        if s.read_exact(&mut payload).is_err() {
            break;
        }
        if masked {
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
        }
        match opcode {
            0x8 => {
                // close: エコーして切断
                let mut w = writer.lock().unwrap_or_else(|p| p.into_inner());
                let _ = write_frame(&mut w, 0x8, &payload);
                break;
            }
            0x9 => {
                // ping → pong (payload をエコー)
                let mut w = writer.lock().unwrap_or_else(|p| p.into_inner());
                if write_frame(&mut w, 0xA, &payload).is_err() {
                    break;
                }
            }
            _ => {} // テキスト等は無視
        }
    }
    // 書き込み側 (srx.recv ループ) も write エラーで終わるよう両方向を閉じる
    let _ = s.shutdown(Shutdown::Both);
}

/// サーバ → クライアントのフレーム (非マスク・FIN) を書く。
fn write_frame(s: &mut TcpStream, opcode: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 10);
    frame.push(0x80 | opcode);
    let len = payload.len();
    if len < 126 {
        frame.push(len as u8);
    } else if len < 65536 {
        frame.push(126);
        frame.extend((len as u16).to_be_bytes());
    } else {
        frame.push(127);
        frame.extend((len as u64).to_be_bytes());
    }
    frame.extend_from_slice(payload);
    s.write_all(&frame)
}

/// Sec-WebSocket-Accept の計算 (RFC 6455)。
fn handshake_accept(key: &str) -> String {
    let guid = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    base64(&sha1(format!("{key}{guid}").as_bytes()))
}

/// SHA-1 (RFC 3174)。ハンドシェイク専用なので速度は不要。
fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let ml = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&ml.to_be_bytes());
    for chunk in msg.chunks(64) {
        let mut w = [0u32; 80];
        for (i, word) in chunk.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let (mut a, mut b, mut c, mut d, mut e) = (h[0], h[1], h[2], h[3], h[4]);
        for (i, &wi) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999u32),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let tmp = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(wi);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = tmp;
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// Base64 (標準アルファベット、パディングあり)。
fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let b = [c[0], *c.get(1).unwrap_or(&0), *c.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_known_vector() {
        let hex: String = sha1(b"abc").iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn handshake_accept_rfc6455_example() {
        // RFC 6455 §1.3 の例
        assert_eq!(
            handshake_accept("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn base64_padding() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }
}
