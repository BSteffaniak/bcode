{
  description = "bcode development environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
        # Keep the native formatter crates and development tools on a compatible toolchain.
        rustc =
          assert pkgs.lib.versionAtLeast pkgs.rustc.version "1.97";
          pkgs.rustc;
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustc
          ]
          ++ (with pkgs; [
            cargo
            rustfmt
            clippy
            llvmPackages.lld
            cmake
            markdownlint-cli
            pkg-config
            alsa-lib
            libxkbcommon
            wayland
            openssl
            fish
          ]);

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
