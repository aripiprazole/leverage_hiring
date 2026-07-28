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
            pkgs.lima
            pkgs.cargo-mutants
          ];
          RUST_BACKTRACE = "1";
        };
      }
    );
}
