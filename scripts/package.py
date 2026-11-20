#!/usr/bin/env python3
"""Produce native archives and a complete, pinned recursive source archive."""
from __future__ import annotations
import argparse
import gzip
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parent.parent
PLUGINS = ('zcalld_aliases', 'zcalld_policy', 'zcalld_redact', 'zcalld_community', 'zcalld_plugin_routing')
TARGETS = ('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu',
           'x86_64-pc-windows-msvc', 'aarch64-apple-darwin', 'x86_64-apple-darwin')


def run(*args, cwd=ROOT):
    return subprocess.check_output(args, cwd=cwd).decode().strip()


def ver():
    return tomllib.loads((ROOT / 'Cargo.toml').read_text())['workspace']['package']['version']


def tar_tree(source, destination, epoch):
    with destination.open('wb') as raw, gzip.GzipFile(fileobj=raw, mode='wb', mtime=0) as compressed:
        with tarfile.open(fileobj=compressed, mode='w') as archive:
            for path in sorted(source.rglob('*')):
                info = archive.gettarinfo(str(path), arcname=str(path.relative_to(source.parent)))
                info.uid = info.gid = 0
                info.uname = info.gname = ''
                info.mtime = epoch
                if path.is_file() and not path.is_symlink():
                    with path.open('rb') as stream:
                        archive.addfile(info, stream)
                else:
                    archive.addfile(info)


def source_archive(output):
    revision = run('git', 'rev-parse', 'HEAD')
    epoch = int(run('git', 'show', '-s', '--format=%ct', revision))
    manifest = {'repository': 'LIghtJUNction/zcalld', 'commit': revision, 'submodules': {}}
    with tempfile.TemporaryDirectory(prefix='zcalld-source-') as directory:
        stage = Path(directory) / f'zcalld-{ver()}'
        stage.mkdir()

        def export(repo, commit, relative):
            if run('git', 'rev-parse', 'HEAD', cwd=repo) != commit:
                raise ValueError(f'{relative or "."} does not match its pinned gitlink')
            data = subprocess.check_output(['git', 'archive', '--format=tar', commit], cwd=repo)
            destination = stage / relative
            destination.mkdir(parents=True, exist_ok=True)
            with tarfile.open(fileobj=io.BytesIO(data)) as archive:
                archive.extractall(destination, filter='data')
            tree = subprocess.check_output(['git', 'ls-tree', '-rz', commit], cwd=repo)
            for entry in tree.split(b'\0'):
                if not entry:
                    continue
                meta, path = entry.split(b'\t', 1)
                mode, kind, sha = meta.split()
                if mode != b'160000':
                    continue
                child = Path(path.decode())
                prefix = relative / child
                manifest['submodules'][prefix.as_posix()] = sha.decode()
                export(repo / child, sha.decode(), prefix)

        export(ROOT, revision, Path())
        (stage / 'SOURCE.json').write_text(json.dumps(manifest, indent=2) + '\n')
        name = output / f'zcalld-{ver()}-source.tar.gz'
        tar_tree(stage, name, epoch)
        print(name)


def native_archive(args, output):
    target = args.target or next(line.split(': ', 1)[1] for line in run('rustc', '-vV').splitlines() if line.startswith('host: '))
    if target not in TARGETS:
        raise ValueError(f'unsupported release target: {target}')
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--release', '--workspace', '--locked'], cwd=ROOT, check=True)
    build = Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target'))
    if not build.is_absolute():
        build = ROOT / build
    build /= 'release'
    windows = 'windows' in target
    darwin = 'darwin' in target
    executable = 'zcalld.exe' if windows else 'zcalld'
    stem = f'zcalld-{ver()}-{target}'
    epoch = int(os.environ.get('SOURCE_DATE_EPOCH') or run('git', 'show', '-s', '--format=%ct', 'HEAD'))
    with tempfile.TemporaryDirectory(prefix='zcalld-package-') as directory:
        stage = Path(directory) / stem
        binary = stage / 'bin' / executable
        binary.parent.mkdir(parents=True)
        shutil.copy2(build / executable, binary)
        plugins = stage / 'lib/zcalld/plugins'
        plugins.mkdir(parents=True)
        for plugin in PLUGINS:
            name = plugin + '.dll' if windows else 'lib' + plugin + ('.dylib' if darwin else '.so')
            shutil.copy2(build / name, plugins / name)
        docs = stage / 'share/doc/zcalld'
        docs.mkdir(parents=True)
        shutil.copy2(ROOT / 'README.md', docs / 'README.md')
        shutil.copytree(ROOT / 'docs', docs / 'docs')
        shutil.copy2(ROOT / 'examples/config.conf', docs / 'config.conf')
        example = stage / 'etc/zcalld'
        example.mkdir(parents=True)
        shutil.copy2(ROOT / 'examples/config.conf', example / 'config.conf')
        licenses = stage / 'share/licenses/zcalld'
        licenses.mkdir(parents=True)
        shutil.copy2(ROOT / 'LICENSE', licenses / 'LICENSE')
        for codec_license in (ROOT / 'packaging/licenses').glob('*.txt'):
            shutil.copy2(codec_license, licenses / codec_license.name)
        manual = stage / 'share/man/man1'
        manual.mkdir(parents=True)
        shutil.copy2(ROOT / 'packaging/man/zcalld.1', manual / 'zcalld.1')
        if not windows:
            shutil.copy2(ROOT / 'packaging/install.sh', stage / 'install.sh')
            (stage / 'install.sh').chmod(0o755)
        if not windows and not darwin:
            shutil.copytree(ROOT / 'packaging/systemd', stage / 'lib/systemd')
            # The source layout keeps the system unit one level above user/.
            (stage / 'lib/systemd/system').mkdir()
            (stage / 'lib/systemd/zcalld.service').rename(stage / 'lib/systemd/system/zcalld.service')
            shutil.copytree(ROOT / 'packaging/sysusers.d', stage / 'lib/sysusers.d')
            shutil.copytree(ROOT / 'packaging/tmpfiles.d', stage / 'lib/tmpfiles.d')
        if windows:
            dlls = Path(os.environ['LIBCURL_DLL_DIR'])
            copied = list(dlls.glob('*.dll'))
            if not any('curl' in p.name.lower() for p in copied):
                raise ValueError('Windows archive needs the libcurl runtime DLL')
            for path in copied:
                shutil.copy2(path, binary.parent / path.name)
            license_root = Path(os.environ['LIBCURL_LICENSE_DIR'])
            for copyright_file in license_root.glob('*/copyright'):
                shutil.copy2(copyright_file, licenses / (copyright_file.parent.name + '.txt'))
            codec_licenses = os.environ.get('CODEC_LICENSE_DIR')
            if codec_licenses:
                for copyright_file in Path(codec_licenses).glob('*/copyright'):
                    shutil.copy2(copyright_file, licenses / (copyright_file.parent.name + '.txt'))
            name = output / (stem + '.zip')
            with zipfile.ZipFile(name, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
                for path in sorted(stage.rglob('*')):
                    if path.is_file():
                        archive.write(path, path.relative_to(stage.parent))
        else:
            name = output / (stem + '.tar.gz')
            tar_tree(stage, name, epoch)
        # Updaters validate this binary against SHA256SUMS and --version before replacing it.
        shutil.copy2(binary, output / (stem + ('.exe' if windows else '')))
        print(name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--no-build', action='store_true')
    parser.add_argument('--source', action='store_true')
    parser.add_argument('--target', choices=TARGETS)
    args = parser.parse_args()
    output = ROOT / 'dist'
    output.mkdir(exist_ok=True)
    if args.source:
        source_archive(output)
    else:
        native_archive(args, output)


if __name__ == '__main__':
    main()
