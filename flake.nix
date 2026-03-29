{
  description = "zeldex Zellij plugin";

  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = {
    self,
    flake-utils,
    nixpkgs,
    rust-overlay,
  }:
    flake-utils.lib.eachDefaultSystem (system: let
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      rust = pkgs.rust-bin.stable.latest.default.override {
        extensions = ["clippy" "rust-src" "rustfmt"];
        targets = ["wasm32-wasip1"];
      };
    in {
      devShells.default = pkgs.mkShell {
        packages = [
          rust
          pkgs.binaryen
          pkgs.curl
          pkgs.openssl
          pkgs.pkg-config
          pkgs.wasm-tools
        ];

        RUST_SRC_PATH = "${rust}/lib/rustlib/src/rust/library";
      };
    });
}
