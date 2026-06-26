{
  description = "emakiWM dev shell — Rust cross-compile to x86_64-pc-windows-gnu";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay, naersk }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };

      rustToolchain = pkgs.rust-bin.stable.latest.default.override {
        targets = [ "x86_64-pc-windows-gnu" ];
        extensions = [ "rust-src" "rust-analyzer" "clippy" "rustfmt" ];
      };

      mingwCC = pkgs.pkgsCross.mingwW64.buildPackages.gcc;
      winpthreads = pkgs.pkgsCross.mingwW64.windows.pthreads;

      naerskWin = (pkgs.callPackage naersk { }).override {
        cargo = rustToolchain;
        rustc = rustToolchain;
      };

      crossArgs = {
        strictDeps = true;
        depsBuildBuild = [ mingwCC ];
        CARGO_BUILD_TARGET = "x86_64-pc-windows-gnu";
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER =
          "${mingwCC}/bin/x86_64-w64-mingw32-gcc";
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS =
          "-L native=${winpthreads}/lib";
        doCheck = false;
      };

    in
    {
      # nix build          → result/bin/ に全バイナリ (.exe)
      # nix build .#emakiwm 等で個別指定も可
      packages.${system} = {
        default = naerskWin.buildPackage (crossArgs // { src = ./.; });
        emakiwm = naerskWin.buildPackage (crossArgs // {
          src = ./.;
          cargoBuildOptions = opts: opts ++ [ "-p" "emakiwm" ];
        });
        emakiwmc = naerskWin.buildPackage (crossArgs // {
          src = ./.;
          cargoBuildOptions = opts: opts ++ [ "-p" "emakiwmc" ];
        });
        emakiwm-setup = naerskWin.buildPackage (crossArgs // {
          src = ./.;
          cargoBuildOptions = opts: opts ++ [ "-p" "emakiwm-setup" ];
        });
      };

      devShells.${system}.default = pkgs.mkShell {
        packages = [ rustToolchain mingwCC ];

        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_LINKER =
          "${mingwCC}/bin/x86_64-w64-mingw32-gcc";
        CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS =
          "-L native=${winpthreads}/lib";

        shellHook = ''
          echo "emakiwm dev shell"
          echo "  build:  cargo build --target x86_64-pc-windows-gnu"
          echo "  test:   cargo test  (core クレートは Linux ネイティブで実行)"
        '';
      };
    };
}
