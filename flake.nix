{
  description = "rchat-blazar — build environment for the Rocket.Chat-compatible client";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };

        # Runtime libraries for the GPUI desktop client: Wayland/X11 windowing,
        # GPU (Vulkan/GL), fonts, audio, TLS and the *-sys crates.
        libraries = with pkgs; [
          alsa-lib
          fontconfig
          freetype
          libGL
          libgit2
          libxkbcommon
          libX11
          libXcursor
          libXi
          libXinerama
          libXrandr
          libxcb
          openssl
          pipewire
          sqlite
          udev
          vulkan-loader
          wayland
          zlib
          zstd
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          name = "rchat-blazar";

          # The Rust toolchain is the operator's rustup install, never a nixpkgs
          # substitute: `cargo`/`rustc` must be the same build as `clippy`.
          nativeBuildInputs = with pkgs; [ pkg-config cmake just python3 jq ];

          buildInputs = libraries;

          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath libraries;
          PKG_CONFIG_PATH = pkgs.lib.makeSearchPathOutput "dev" "lib/pkgconfig" libraries;

          shellHook = ''
            echo "[rchat-blazar] devShell ready - rustup toolchain, GUI libraries, just"
          '';
        };

        formatter = pkgs.nixfmt-rfc-style;
      });
}
