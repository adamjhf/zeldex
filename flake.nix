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
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
      packageMeta = cargoToml.package;
      binMeta =
        if cargoToml ? bin && cargoToml.bin != [] then
          builtins.head cargoToml.bin
        else
          {name = packageMeta.name;};
      pkgs = import nixpkgs {
        inherit system;
        overlays = [(import rust-overlay)];
      };
      rust = pkgs.rust-bin.stable.latest.default.override {
        extensions = ["clippy" "rust-src" "rustfmt"];
        targets = ["wasm32-wasip1"];
      };
      rustPlatform = pkgs.makeRustPlatform {
        cargo = rust;
        rustc = rust;
      };
      zeldexPackage = pkgs.stdenv.mkDerivation {
        pname = packageMeta.name;
        version = packageMeta.version;
        src = ./.;
        cargoDeps = rustPlatform.importCargoLock {
          lockFile = ./Cargo.lock;
        };
        nativeBuildInputs = [
          rustPlatform.cargoSetupHook
          rust
          pkgs.pkg-config
        ];
        buildInputs = [pkgs.openssl];

        buildPhase = ''
          runHook preBuild
          cargo build --frozen --offline --release --target wasm32-wasip1 --bin ${binMeta.name}
          runHook postBuild
        '';

        installPhase = ''
          runHook preInstall
          mkdir -p $out/lib
          cp target/wasm32-wasip1/release/${binMeta.name}.wasm $out/lib/${binMeta.name}.wasm
          runHook postInstall
        '';
      };
    in {
      packages.default = zeldexPackage;

      devShells.default = pkgs.mkShell {
        inputsFrom = [zeldexPackage];
        packages = [
          rust
          pkgs.binaryen
          pkgs.curl
          pkgs.wasm-tools
        ];

        RUST_SRC_PATH = "${rust}/lib/rustlib/src/rust/library";
      };
    });
}
