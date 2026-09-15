{
  description = "TTYbird - find coding agents and return to their terminals";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Unstable dropped Intel macOS after 26.05. Keep that native output on the
    # final supported nixpkgs branch while the lock fixes the exact revision.
    nixpkgs-x86-darwin.url = "github:NixOS/nixpkgs/nixpkgs-26.05-darwin";
  };

  outputs = {
    nixpkgs,
    nixpkgs-x86-darwin,
    ...
  }: let
    systems = [
      "aarch64-darwin"
      "x86_64-darwin"
      "aarch64-linux"
      "x86_64-linux"
    ];
    eachSystem = nixpkgs.lib.genAttrs systems;
    packagesFor = system:
      import (
        if system == "x86_64-darwin"
        then nixpkgs-x86-darwin
        else nixpkgs
      ) {
        inherit system;
      };
  in {
    packages = eachSystem (system: let
      pkgs = packagesFor system;
      ttybird = pkgs.callPackage ./nix/package.nix {};
    in {
      inherit ttybird;
      default = ttybird;
    });

    checks = eachSystem (system: let
      pkgs = packagesFor system;
      ttybird = pkgs.callPackage ./nix/package.nix {};
      nixSource = pkgs.lib.fileset.toSource {
        root = ./.;
        fileset = pkgs.lib.fileset.unions [
          ./flake.nix
          ./nix
        ];
      };
    in {
      inherit ttybird;
      formatting =
        pkgs.runCommand "ttybird-nix-format" {
          nativeBuildInputs = [pkgs.alejandra];
        } ''
          cp -R ${nixSource} source
          chmod -R u+w source
          cd source
          alejandra --check flake.nix nix
          touch $out
        '';
    });

    devShells = eachSystem (system: let
      pkgs = packagesFor system;
      ttybird = pkgs.callPackage ./nix/package.nix {};
    in {
      default = pkgs.mkShell {
        inputsFrom = [ttybird];
        packages = with pkgs; [
          alejandra
          cargo
          clippy
          lsof
          nodejs
          openssh
          python3
          rustc
          rustfmt
          tmux
          unixtools.ps
          zig_0_15
        ];
        GHOSTTY_SOURCE_DIR = ttybird.passthru.ghosttySource;
        GHOSTTY_ZIG_SYSTEM_DIR = ttybird.passthru.ghosttyZigDeps;
      };
    });

    formatter = eachSystem (system: (packagesFor system).alejandra);
  };
}
