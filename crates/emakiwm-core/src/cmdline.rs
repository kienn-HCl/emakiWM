//! `spawn` コマンドラインの分解。

/// コマンドラインを (プログラム, 引数全体) に分ける。
///
/// 空白を含むパスは二重引用符で囲む: `"C:\Program Files\App\app.exe" --flag`。
/// 引数部分は加工せずそのまま返す (引数内の引用符は起動先が解釈する)。
/// プログラムが空・引用符が閉じていない場合は None。
pub fn split_program(cmd: &str) -> Option<(&str, &str)> {
    let cmd = cmd.trim();
    let (prog, args) = match cmd.strip_prefix('"') {
        Some(rest) => rest.split_once('"')?,
        None => cmd.split_once(char::is_whitespace).unwrap_or((cmd, "")),
    };
    (!prog.is_empty()).then_some((prog, args.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_only() {
        assert_eq!(split_program("wt"), Some(("wt", "")));
        assert_eq!(split_program("  zebar  "), Some(("zebar", "")));
    }

    #[test]
    fn program_with_args() {
        assert_eq!(split_program("wt -p NixOS"), Some(("wt", "-p NixOS")));
    }

    #[test]
    fn quoted_program_with_spaces() {
        assert_eq!(
            split_program(r#""C:\Program Files\App\app.exe" --flag "a b""#),
            Some((r"C:\Program Files\App\app.exe", r#"--flag "a b""#))
        );
        assert_eq!(
            split_program(r#""C:\Program Files\App\app.exe""#),
            Some((r"C:\Program Files\App\app.exe", ""))
        );
    }

    #[test]
    fn invalid_input() {
        assert_eq!(split_program(""), None);
        assert_eq!(split_program("   "), None);
        assert_eq!(split_program(r#""unterminated path"#), None);
        assert_eq!(split_program(r#""" args"#), None);
    }
}
