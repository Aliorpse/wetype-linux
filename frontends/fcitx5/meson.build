project('wetype-fcitx5', 'cpp',
  version: '0.1.0',
  license: 'MIT',
  default_options: ['cpp_std=c++20', 'warning_level=2'])

# Fcitx5Core ships the engine/panel/candidate APIs; 5.1.x is what this was
# written against.
fcitx5 = dependency('Fcitx5Core', version: '>=5.1.0')

repo_root = meson.project_source_root() / '../..'
rust_target_dir = meson.project_build_root() / 'rust'

# Build the Rust static library from Meson so a single `meson setup && ninja`
# produces the whole addon. `cargo` writes into its own target dir and the
# result is copied to the declared output.
cargo = find_program('cargo', required: true)
sh = find_program('sh', required: true)
rust_lib = custom_target('wetype-ffi',
  output: 'libwetype_ffi.a',
  command: [
    sh, '-c',
    '"$1" build --release --locked --manifest-path "$2/Cargo.toml" -p wetype-ffi --target-dir "$3" && cp "$3/release/libwetype_ffi.a" "$4"',
    'sh',              # $0
    cargo.full_path(), # $1
    repo_root,         # $2
    rust_target_dir,   # $3
    '@OUTPUT@',        # $4 (relative to the build dir, which stays current)
  ],
  build_by_default: true,
  # Meson cannot see the Rust sources, so without this the archive is treated as
  # up to date forever and the addon links against a stale staticlib (which is
  # exactly how a symbol ended up undefined). cargo itself no-ops cheaply when
  # nothing changed, so always asking it is the simple correct answer.
  build_always_stale: true,
)

# The frontend includes "wetype.h", generated from the Rust C ABI. It is a
# declared output in the build directory on purpose: Meson wires generated
# sources as *order-only* dependencies, so a header regenerated in the source
# tree would only reach the C++ objects on the next build - the stale-header
# class of bug we are trying to remove. As a declared output, Ninja's restat
# logic rebuilds the objects in the same run.
#
# The generator only rewrites the file when the rendered content changed, so an
# unchanged header keeps its mtime and nothing is recompiled. A best-effort copy
# keeps the checked-in header in sync for non-Meson consumers and for `git
# diff`; a read-only source tree simply skips it.
header = custom_target('wetype-header',
  output: 'wetype.h',
  command: [
    sh, '-c',
    '"$1" run --quiet --locked --manifest-path "$2/Cargo.toml" -p wetype-ffi --features cbindgen --bin wetype-ffi-header -- "$3" || exit 1; { cmp -s "$3" "$4" 2>/dev/null || cp "$3" "$4" 2>/dev/null; } || true',
    'sh',              # $0
    cargo.full_path(), # $1
    repo_root,         # $2
    '@OUTPUT@',        # $3 (relative to the build dir, which stays current)
    meson.project_source_root() / 'include/wetype/wetype.h', # $4
  ],
  build_by_default: true,
  # Same reason as the staticlib: Meson cannot see the Rust sources. cargo
  # no-ops cheaply when nothing changed, and the generator skips the write.
  build_always_stale: true,
)

addon = shared_module('wetype',
  sources: ['src/addon.cpp', 'src/engine.cpp', header],
  # The build directory comes first, so the generated header shadows the
  # checked-in fallback.
  include_directories: include_directories('include/wetype'),
  dependencies: [fcitx5],
  link_with: rust_lib,
  # The Rust staticlib needs these; Meson does not see them across the FFI.
  link_args: ['-lpthread', '-ldl', '-lm'],
)
