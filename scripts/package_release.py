"""Package the tested native binary, rejecting developer-machine library paths.

Run after `cargo build --release --locked --bin ttybird` on a clean CI runner.
Usage: python3 scripts/package_release.py TARGET_TRIPLE
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile

root = Path(__file__).resolve().parent.parent
binary = root / 'target/release/ttybird'
target = sys.argv[1]
expected = {('Darwin', 'arm64'): 'aarch64-apple-darwin', ('Linux', 'x86_64'): 'x86_64-unknown-linux-gnu'}
if expected.get((platform.system(), platform.machine())) != target:
    raise SystemExit('Only the tested native runner architecture can be packaged')
if platform.system() == 'Darwin':
    linked = subprocess.check_output(['otool', '-L', str(binary)], text=True)
    dependencies = [line.strip().split(' (')[0] for line in linked.splitlines()[1:]]
    if any(not path.startswith(('/usr/lib/', '/System/Library/')) for path in dependencies):
        raise SystemExit('Non-system dynamic library reference; build on a clean runner')
else:
    linked = subprocess.check_output(['ldd', str(binary)], text=True)
    if any(value in linked for value in ('not found', '/nix/store/', '/home/', '/opt/')):
        raise SystemExit('Unresolved or developer-specific dynamic library reference')
version_output = subprocess.check_output([str(binary), '--version'], text=True).strip().split()
if len(version_output) != 2 or version_output[0] != 'ttybird':
    raise SystemExit('Unexpected ttybird --version output')
version = version_output[1]
metadata = json.loads(subprocess.check_output(['cargo', 'metadata', '--locked', '--format-version', '1'], cwd=root))
root_package_id = metadata['resolve']['root']
manifest_version = next(
    package['version'] for package in metadata['packages'] if package['id'] == root_package_id
)
if version != manifest_version:
    raise SystemExit(
        f'Binary version {version} does not match Cargo manifest version {manifest_version}'
    )
if os.environ.get('GITHUB_REF_TYPE') == 'tag':
    tag = os.environ.get('GITHUB_REF_NAME')
    if tag != f'v{version}':
        raise SystemExit(f'Release tag {tag!r} does not match binary version v{version}')
dist = root / 'dist'
dist.mkdir(exist_ok=True)
name = f'ttybird-{version}-{target}'
with tempfile.TemporaryDirectory(prefix='ttybird-release-') as temporary:
    package = Path(temporary) / name
    package.mkdir()
    shutil.copy2(binary, package / 'ttybird')
    for filename in ('LICENSE', 'README.md', 'README.ja.md'):
        shutil.copy2(root / filename, package / filename)
    shutil.copytree(root / 'docs', package / 'docs')
    licenses = package / 'licenses'
    licenses.mkdir()
    records = []
    for dependency in metadata['packages']:
        if dependency['name'] == 'ttybird':
            continue
        source = Path(dependency['manifest_path']).parent
        destination = licenses / f"{dependency['name']}-{dependency['version']}"
        destination.mkdir()
        for path in source.iterdir():
            if path.is_file() and path.name.lower().startswith(('license', 'licence', 'copying', 'notice', 'copyright')):
                shutil.copy2(path, destination / path.name)
        records.append(f"{dependency['name']} {dependency['version']}: {dependency.get('license') or 'see crate license'}")
    # The native Ghostty library is built by the sys crate, not a Cargo package.
    shutil.copy2(root / 'nix/GHOSTTY-LICENSE', licenses / 'GHOSTTY-LICENSE')
    shutil.copytree(root / 'licenses/native', licenses / 'native')
    (licenses / 'INDEX.txt').write_text(
        '\n'.join(records)
        + '\nGhostty: MIT; see GHOSTTY-LICENSE\n'
        + 'Native dependencies: see native/PROVENANCE.md and accompanying license texts\n'
    )
    archive = dist / f'{name}.tar.gz'
    with tarfile.open(archive, 'w:gz') as output:
        output.add(package, arcname=name)
    checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
    (dist / f'{name}.sha256').write_text(f'{checksum}  {archive.name}\n')
print(f'Packaged {name}; dependency check passed.')
