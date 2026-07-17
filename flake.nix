{
  description = "Zero-fork netspeed/load-average feed for zellij's zjstatus pipe widgets";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, rust-overlay, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forEachSystem =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }
          )
        );
    in
    {
      packages = forEachSystem (pkgs: rec {
        zj-sysinfo = pkgs.callPackage ./default.nix { };
        default = zj-sysinfo;
      });

      devShells = forEachSystem (pkgs: {
        default = pkgs.mkShell {
          packages = [
            (pkgs.rust-bin.stable.latest.default.override {
              targets = [ "wasm32-wasip1" ];
            })
            pkgs.pkg-config
            pkgs.openssl
          ];
        };
      });
    };
}
