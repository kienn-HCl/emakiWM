{
  description = "emakiWM dev shell — Rust cross-compile to x86_64-pc-windows-gnu";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };

      # Windows 用 std を含む stable toolchain。
      # ホスト (Linux) 向け std も含むため、core クレートの
      # 純粋ロジックの cargo test は Linux 上でそのまま実行できる。
      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        targets = [ "x86_64-pc-windows-gnu" ];
        extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
      };

      mingwCC = pkgs.pkgsCross.mingwW64.buildPackages.gcc;
      # rust std (windows-gnu) が -l:libpthread.a を要求するため winpthreads が必要
      winpthreads = pkgs.pkgsCross.mingwW64.windows.pthreads;
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = [
          rustToolchain
          mingwCC
        ];

        # .exe のリンクに mingw-w64 gcc を使う
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER =
          "${mingwCC}/bin/x86_64-w64-mingw32-gcc";
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS =
          "-L native=${winpthreads}/lib";

        # WSL2 では binfmt interop により .exe を直接実行できるため、
        # `cargo run --target x86_64-pc-windows-gnu` がそのまま動く
        shellHook = ''
          echo "emakiwm dev shell"
          echo "  build:  cargo build --target x86_64-pc-windows-gnu"
          echo "  test:   cargo test  (core クレートは Linux ネイティブで実行)"
        '';
      };
    };
}
