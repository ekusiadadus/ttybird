# Nix

The flake provides a reproducible `ttybird` package and development shell for
Apple Silicon and Intel macOS and Linux.

```sh
nix build .#ttybird
./result/bin/ttybird doctor

nix develop
cargo test --locked

nix flake check
```

If you use direnv, run `direnv allow` once; `.envrc` enters the same default
development shell.

These commands use Git's tracked flake source. `path:.` is useful when testing
a deliberately untracked snapshot, but Nix must first materialize every visible
path in that snapshot. Keep large `target` directories outside such a snapshot.
For an untracked checkout, the repository helper copies only package inputs to a
temporary snapshot, so it does not copy `target`:

```sh
./scripts/nix-local.sh build
./scripts/nix-local.sh check --all-systems --no-build
./scripts/nix-local.sh develop
```

## Native terminal parser dependency

`libghostty-vt-sys` 0.2.1 compiles Ghostty's terminal parser during the Cargo
build. Its build script requires Zig 0.15.2 and pins Ghostty commit
`a887df42c56f6de86c0fe6da9c4eeca37931e083`.

The Nix package mirrors that contract. It fetches the pinned Ghostty source and
its generated Zig dependency set as fixed-output derivations. The commit's
current Uucode 0.2 entry is pinned alongside that generated set, then the merged
dependency store is passed to Zig with `--system`. Cargo crates, Ghostty source,
and Zig packages may be downloaded into the Nix store when they are missing;
the actual package build does not fetch from the network.

The derivation uses only fixed Nix inputs once those inputs are present. Local
validation cannot by itself prove sandbox enforcement when the host Nix daemon
has `sandbox = false`; use a trusted daemon or CI with sandboxing enabled for
that additional boundary.

The flake lock pins nixpkgs, including Rust and Zig. Intel macOS uses the final
nixpkgs 26.05 Darwin branch because newer unstable revisions no longer evaluate
that platform. Avoid replacing Zig with a global installation or overriding
Apple SDK variables: the package uses the ordinary nixpkgs Darwin toolchain and
only builds Ghostty's VT library. A build-only `xcode-select` shim points Zig's
native SDK discovery at that pinned nixpkgs SDK; it never selects a host Xcode
installation and is not included in the installed package.

## Platform scope

The flake declares `aarch64-darwin`, `x86_64-darwin`, `aarch64-linux`, and
`x86_64-linux`. Each output is a native package for that system. Building the
Linux output from macOS is evaluation or cross-build coverage unless a Linux
builder executes it; it is not Linux runtime proof.

The packaged binary wraps its runtime path with `ps`, `lsof` where needed,
`tmux`, and `ssh`. Ghostty AppleScript additionally uses the macOS system
`osascript`; that integration is unavailable on Linux. Builds outside the Nix
package must provide the corresponding process-inspection and navigation tools
on `PATH`.

## References

- [Ghostty's flake](https://github.com/ghostty-org/ghostty/blob/main/flake.nix) and [package structure](https://github.com/ghostty-org/ghostty/blob/main/nix/package.nix) informed the split between package and development shell.
- [Pinned Ghostty dependency expression](https://github.com/ghostty-org/ghostty/blob/a887df42c56f6de86c0fe6da9c4eeca37931e083/build.zig.zon.nix) is vendored for evaluation without building another platform's source fetcher.
- [GitHub runner specifications](https://docs.github.com/en/actions/reference/runners/github-hosted-runners) identify the native CI targets. Local [validation](VALIDATION.md) is distinct from an actual CI run.
