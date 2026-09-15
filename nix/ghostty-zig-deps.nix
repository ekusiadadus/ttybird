{
  fetchurl,
  callPackage,
  linkFarm,
  runCommandLocal,
  symlinkJoin,
  zig_0_15,
}: let
  packageHash = "uucode-0.2.0-ZZjBPqZVVABQepOqZHR7vV_NcaN-wats0IB6o-Exj6m9";
  archive = fetchurl {
    url = "https://deps.files.ghostty.org/${packageHash}.tar.gz";
    hash = "sha256-0KvuD0+L1urjwFF3fhbnxC2JZKqqAVWRxOVlcD9GX5U=";
  };
  package =
    runCommandLocal "ghostty-${packageHash}" {
      nativeBuildInputs = [zig_0_15];
    } ''
      export ZIG_GLOBAL_CACHE_DIR="$TMPDIR/zig-cache"
      actual="$(zig fetch ${archive})"
      test "$actual" = '${packageHash}'
      cp -R "$ZIG_GLOBAL_CACHE_DIR/p/$actual" "$out"
    '';
  generated = callPackage ./ghostty-build-zig-zon.nix {
    name = "ghostty-generated-zig-deps";
  };
  current = linkFarm "ghostty-current-zig-deps" [
    {
      name = packageHash;
      path = package;
    }
  ];
in
  symlinkJoin {
    name = "ghostty-vt-zig-deps";
    paths = [
      generated
      current
    ];
  }
