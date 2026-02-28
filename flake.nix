{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rust-toolchain = pkgs.rust-bin.stable.latest.default.override { extensions = [ "rust-src" ]; };

        nix-deploy = pkgs.rustPlatform.buildRustPackage {
          pname = "nix-deploy";
          version = "0.1.0";

          src = ./.;

          cargoLock = {
            lockFile = ./Cargo.lock;
          };

          nativeBuildInputs = with pkgs; [
            pkg-config
            rust-toolchain
          ];

          buildInputs = with pkgs; [
            openssl
            zlib
          ];

          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";

          meta = with pkgs.lib; {
            description = "TUI tool to update servers running NixOS";
            license = licenses.mit;
            maintainers = [ painerp ];
          };
        };
      in
      {

        packages = {
          default = nix-deploy;
          nix-deploy = nix-deploy;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rust-toolchain
            pkg-config
            autoconf
            openssl
            libtool
            automake
            clippy
          ];

          RUST_SRC_PATH = "${pkgs.rust-bin.stable.latest.default}/lib/rustlib/src/rust";
        };
      }
    );
}
