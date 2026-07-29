{
  description = "SSH VM service development";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs?ref=nixos-26.05";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
    git-hooks.url = "github:cachix/git-hooks.nix";
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
      flake-utils,
      git-hooks,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
        nightly-rustfmt = pkgs.rust-bin.nightly.latest.rustfmt;
        aarch64-linux-headers = pkgs.pkgsCross.aarch64-multiplatform.linuxHeaders;
      in
      {
        checks.pre-commit-check = git-hooks.lib.${system}.run {
          tools = pkgs;
          enabledPackages = [ rust ];
          src = ./.;
          hooks.rustfmt = {
            enable = true;
            language = "system";
            packageOverrides.rustfmt = nightly-rustfmt;
          };
        };

        devShells.default = pkgs.mkShell {
          inherit (self.checks.${system}.pre-commit-check) shellHook;
          buildInputs = self.checks.${system}.pre-commit-check.enabledPackages;
          nativeBuildInputs = [
            nightly-rustfmt
            rust
            pkgs.pre-commit
            pkgs.just
            pkgs.jq
            pkgs.lima
            pkgs.cargo-mutants
          ];
          BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_gnu =
            "-I${aarch64-linux-headers}/include";
          RUST_BACKTRACE = "1";
        };
      }
    );
}
