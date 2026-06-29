{
  description = "QMD - on-device hybrid document search (single static binary)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};

        # Native build inputs needed to compile qmd (llama-cpp-2 uses CMake + C/C++).
        # NOTE: Metal/Foundation/Accelerate frameworks are found automatically by the
        # compiler toolchain on macOS outside the Nix sandbox. A full sandboxed
        # rustPlatform.buildRustPackage derivation is a follow-up: llama-cpp-2 bundles
        # CMake + C++ code that has complex sandbox requirements. Track in CHANGELOG.
        nativeBuildDeps = [
          pkgs.rustc
          pkgs.cargo
          pkgs.rustfmt
          pkgs.clippy
          # cmake 3.x — llama.cpp CMakeLists.txt requires VERSION 3.14..3.28
          pkgs.cmake
          pkgs.pkg-config
        ] ++ pkgs.lib.optionals pkgs.stdenv.isLinux [
          pkgs.gcc
        ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
          pkgs.darwin.cctools
        ];

      in
      {
        # Development shell: full Rust toolchain + native deps for building qmd.
        # On macOS, Metal/Foundation/Accelerate are automatically available via xcrun
        # without listing them explicitly (needed for sandboxed builds only).
        # Usage: nix develop
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = nativeBuildDeps;

          shellHook = ''
            echo "qmd development shell"
            echo "  cargo build --workspace          — debug build"
            echo "  cargo build --profile dist -p qmd-cli  — release binary → target/dist/qmd"
            echo "  cargo run --bin qmd -- <command> — run from source"
          '';
        };
      }
    );
}
