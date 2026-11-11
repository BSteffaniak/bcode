{
  description = "bcode development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };
        # One toolchain supplies rustc, cargo, rustfmt, and clippy so the native
        # formatter crates and development tools stay compatible. The wasm32
        # standard library is required by BMUX's embedded Mermaid guest build.
        rustToolchain =
          let
            toolchain = pkgs.rust-bin.stable.latest.default.override {
              extensions = [
                "rust-src"
                "rustfmt"
                "clippy"
              ];
              targets = [ "wasm32-unknown-unknown" ];
            };
          in
          assert pkgs.lib.versionAtLeast toolchain.version "1.99";
          toolchain;
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
          ]
          ++ (with pkgs; [
            llvmPackages.lld
            cmake
            markdownlint-cli
            pkg-config
            openssl
            fish
          ])
          ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux (
            with pkgs;
            [
              alsa-lib
              libxkbcommon
              wayland
            ]
          );

          shellHook = ''
            echo "bcode development environment loaded"
            echo "Available tools:"
            echo "  - cargo ($(cargo --version))"
            echo "  - rustc ($(rustc --version))"
            echo "  - clippy ($(cargo clippy --version))"
            echo "  - markdownlint ($(markdownlint --version))"
            echo ""
            echo "Run 'markdownlint *.md **/*.md' to lint all markdown files"
            echo "Run 'markdownlint --fix *.md **/*.md' to auto-fix markdown issues"
            echo "Run 'cargo clippy --all-targets -- -D warnings' for strict linting"

            # Only exec fish if we're in an interactive shell (not running a command)
            if [ -z "$IN_NIX_SHELL_FISH" ] && [ -z "$BASH_EXECUTION_STRING" ]; then
              case "$-" in
                *i*) export IN_NIX_SHELL_FISH=1; exec fish ;;
              esac
            fi
          '';
        };
      }
    );
}
