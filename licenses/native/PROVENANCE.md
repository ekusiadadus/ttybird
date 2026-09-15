# Native dependency licenses

`libghostty-vt-sys` builds Ghostty at commit
`a887df42c56f6de86c0fe6da9c4eeca37931e083`. That native build incorporates
dependencies which Cargo metadata cannot inventory:

- **uucode 0.2.0**: the exact archive and Zig package hash are pinned in
  Ghostty's `build.zig.zon` as
  `uucode-0.2.0-ZZjBPqZVVABQepOqZHR7vV_NcaN-wats0IB6o-Exj6m9`. The license
  files here are copied byte-for-byte from that archive.
- **Highway** at commit `66486a10623fa0d72fe91260f96c892e41aceb06`:
  Ghostty's `pkg/highway/build.zig.zon` pins the archive and incorporates its
  code when SIMD support is available. The Apache-2.0 and BSD-3-Clause license
  files here are copied byte-for-byte from that archive.
- **simdutf 5.2.8**: Ghostty vendors this version under `pkg/simdutf`. Its
  license files are copied byte-for-byte from the matching upstream `v5.2.8`
  tag.

Source URLs:

- <https://deps.files.ghostty.org/uucode-0.2.0-ZZjBPqZVVABQepOqZHR7vV_NcaN-wats0IB6o-Exj6m9.tar.gz>
- <https://deps.files.ghostty.org/highway-66486a10623fa0d72fe91260f96c892e41aceb06.tar.gz>
- <https://github.com/simdutf/simdutf/tree/v5.2.8>

SHA-256 checksums of the packaged texts:

```text
UUCODE-LICENSE.md                  312e901e142be2477b4ca859e9311f9e3f80d33372991759b7921c1893605f33
HIGHWAY-LICENSE-APACHE-2.0        43070e2d4e532684de521b885f385d0841030efa2b1a20bafb76133a5e1379c1
HIGHWAY-LICENSE-BSD-3-CLAUSE      d25e82e26acd42ca3ccc9993622631163425b869b9e16284226d534cff6470f2
SIMDUTF-LICENSE-APACHE-2.0        3d34610fc6b5e1b0bfe4e2f36171c2d62c28ef05cb8d704f5a0073be41a43b3d
SIMDUTF-LICENSE-MIT               fc8dbc04e03ad4efc08a647ffe7f995b811a95bc04c0e85a56d5277c6593fa5f
```
