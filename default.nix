# Background zellij plugin: pushes netspeed/load to zjstatus pipe widgets.
# Replaces per-tab `command_*` bash polling (see docs/design.md for the
# incident that motivated this design).
{
  lib,
  makeRustPlatform,
  rust-bin,
  pkg-config,
  openssl,
}:
let
  toolchain = rust-bin.stable.latest.minimal.override {
    targets = [ "wasm32-wasip1" ];
  };
  rustPlatform = makeRustPlatform {
    cargo = toolchain;
    rustc = toolchain;
  };
  excluded = [
    "default.nix"
    "flake.nix"
    "flake.lock"
    "target"
    ".github"
    "docs"
    "README.md"
    "LICENSE"
  ];
in
rustPlatform.buildRustPackage {
  pname = "zj-sysinfo";
  version = "0.1.0";

  src = lib.cleanSourceWith {
    src = ./.;
    filter = name: _type: !(builtins.elem (baseNameOf name) excluded);
  };
  cargoLock.lockFile = ./Cargo.lock;

  # Native checkPhase builds zellij-tile's transitive openssl-sys
  # (zellij-tile -> isahc -> curl -> curl-sys); wasm builds don't.
  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];

  buildPhase = ''
    runHook preBuild
    cargo build --release --target wasm32-wasip1
    runHook postBuild
  '';

  checkPhase = ''
    runHook preCheck
    cargo test --release
    runHook postCheck
  '';

  installPhase = ''
    runHook preInstall
    install -Dm644 target/wasm32-wasip1/release/zj-sysinfo.wasm \
      $out/bin/zj-sysinfo.wasm
    runHook postInstall
  '';
}
