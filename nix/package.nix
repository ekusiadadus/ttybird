{
  lib,
  stdenv,
  rustPlatform,
  fetchFromGitHub,
  callPackage,
  zig_0_15,
  makeWrapper,
  unixtools,
  lsof,
  tmux,
  openssh,
  gitMinimal,
  libnotify,
  runtimeShell,
  python3,
  writeShellScriptBin,
  pkgs,
  darwin,
}: let
  manifest = builtins.fromTOML (builtins.readFile ../Cargo.toml);
  ghosttyCommit = "a887df42c56f6de86c0fe6da9c4eeca37931e083";
  ghosttySource = fetchFromGitHub {
    owner = "ghostty-org";
    repo = "ghostty";
    rev = ghosttyCommit;
    hash = "sha256-1Zz65SCk3rkJ9+Q0MmyNOTNiDSLBRIHRd3IvFM4iNXw=";
  };
  ghosttyZigDeps = callPackage ./ghostty-zig-deps.nix {};
  runtimePrograms =
    [
      unixtools.ps
      tmux
      openssh
      gitMinimal
    ]
    ++ lib.optionals stdenv.hostPlatform.isLinux [libnotify]
    ++ lib.optionals (!stdenv.hostPlatform.isLinux) [lsof];
  appleSdk = pkgs.apple-sdk;
  darwinSdkDiscovery = writeShellScriptBin "xcode-select" ''
    if [ "$#" -eq 1 ] && [ "$1" = "--print-path" ]; then
      printf '%s\n' '${appleSdk}'
      exit 0
    fi
    exit 64
  '';
  source = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../LICENSE
      ../src
      ../scripts/ghostty_export.js
      ../tests
      ../examples
    ];
  };
in
  rustPlatform.buildRustPackage {
    pname = manifest.package.name;
    version = manifest.package.version;
    src = source;

    cargoLock.lockFile = ../Cargo.lock;
    strictDeps = true;

    nativeBuildInputs =
      [
        makeWrapper
        zig_0_15
      ]
      ++ lib.optionals stdenv.hostPlatform.isDarwin [
        darwinSdkDiscovery
        darwin.cctools.libtool
      ];
    nativeCheckInputs = runtimePrograms ++ [python3];
    dontUseZigBuild = true;
    dontUseZigCheck = true;
    dontUseZigInstall = true;
    GHOSTTY_SOURCE_DIR = ghosttySource;
    GHOSTTY_ZIG_SYSTEM_DIR = ghosttyZigDeps;
    LIBGHOSTTY_VT_SYS_OPTIMIZE = "ReleaseFast";
    RUST_TEST_THREADS = "2";
    DEVELOPER_DIR = lib.optionalString stdenv.hostPlatform.isDarwin "${appleSdk}";
    SDKROOT = lib.optionalString stdenv.hostPlatform.isDarwin "${appleSdk}/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk";

    doCheck = true;

    # This fixture is created at test runtime, after the generic shebang
    # patching hook has run. Pure Linux sandboxes do not provide /bin/sh.
    postPatch = lib.optionalString stdenv.hostPlatform.isLinux ''
      substituteInPlace src/remote.rs \
        --replace-fail '#!/bin/sh' '#!${runtimeShell}'
    '';

    postInstall = ''
      wrapProgram $out/bin/ttybird \
        --prefix PATH : ${lib.makeBinPath runtimePrograms}
    '';

    passthru = {
      inherit ghosttyCommit ghosttySource ghosttyZigDeps;
    };

    meta = {
      description = manifest.package.description;
      homepage = "https://github.com/ekusiadadus/ttybird";
      license = lib.licenses.mit;
      mainProgram = "ttybird";
      platforms = [
        "aarch64-darwin"
        "x86_64-darwin"
        "aarch64-linux"
        "x86_64-linux"
      ];
    };
  }
