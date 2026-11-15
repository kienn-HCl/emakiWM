//! WebSocket 接続元 (Origin ヘッダ) の検査 (FR-7.5)。
//!
//! 状態配信はウィンドウタイトルを含む。localhost で待ち受けていても、
//! ブラウザで開いた任意の Web ページは `ws://127.0.0.1:<port>` へ接続できる
//! (WebSocket は同一オリジンポリシーの対象外) ため、Origin で弾く。

/// この Origin からの WebSocket 接続を受け入れてよいか。
///
/// - Origin なし: ブラウザ以外のクライアント (スクリプト等) → 許可
/// - `http(s)` 以外のスキーム (`tauri://localhost` 等のアプリ内 webview) → 許可
/// - `http(s)` はループバック (`localhost` / `*.localhost` / `127.0.0.1` / `[::1]`) のみ許可
/// - `null` (sandbox iframe・data: URL 等) や解釈できない値 → 拒否
pub fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let Some((scheme, rest)) = origin.trim().split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return true;
    }
    let authority = rest.split('/').next().unwrap_or("");
    let host = if authority.starts_with('[') {
        // IPv6 リテラル: "[::1]:8080" → "[::1]"
        authority.split_inclusive(']').next().unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    }
    .to_ascii_lowercase();
    host == "localhost" || host == "127.0.0.1" || host == "[::1]" || host.ends_with(".localhost")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_origin_is_allowed() {
        assert!(origin_allowed(None));
    }

    #[test]
    fn loopback_http_origins_are_allowed() {
        for o in [
            "http://localhost",
            "http://localhost:6124",
            "http://127.0.0.1:6124",
            "https://LOCALHOST",
            "http://asset.localhost",
            "http://[::1]:3000",
        ] {
            assert!(origin_allowed(Some(o)), "{o}");
        }
    }

    #[test]
    fn app_webview_schemes_are_allowed() {
        assert!(origin_allowed(Some("tauri://localhost")));
    }

    #[test]
    fn remote_web_pages_are_rejected() {
        for o in [
            "https://example.com",
            "http://evil.example:6573",
            "http://localhost.example.com",
            "http://127.0.0.1.example.com",
            "https://notlocalhost",
            "null",
            "",
        ] {
            assert!(!origin_allowed(Some(o)), "{o}");
        }
    }
}
