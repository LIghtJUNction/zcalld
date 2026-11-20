#!/usr/bin/env python3
"""Produce native archives and a complete, pinned recursive source archive."""
from __future__ import annotations
import argparse
import gzip
import hashlib
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
PLUGINS = ('zcalld_aliases', 'zcalld_policy', 'zcalld_redact', 'zcalld_community', 'zcalld_plugin_routing', 'zcalld_desktop')
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


def install_manifest(stage, version, target, source):
    """The installer checks this entire cohort before switching directories."""
    windows = 'windows' in target
    darwin = 'darwin' in target
    suffix = '.exe' if windows else ''
    required = [f'bin/zcalld{suffix}', f'bin/zcalld-installer{suffix}']
    required.extend('lib/zcalld/plugins/' + (name + '.dll' if windows else 'lib' + name + ('.dylib' if darwin else '.so'))
                    for name in (*PLUGINS, *(() if 'zcalld_desktop' in PLUGINS else ('zcalld_desktop',))))
    if windows:
        required.append('libexec/zcalld/zcalld-desktop-notify.exe')
    elif darwin:
        required.extend(('libexec/zcalld/zcalld-desktop-notify.app/Contents/Info.plist',
                         'libexec/zcalld/zcalld-desktop-notify.app/Contents/MacOS/zcalld-desktop-notify'))
    files = []
    for path in sorted(stage.rglob('*')):
        if path.is_symlink():
            raise ValueError('native installation archives must not contain links')
        if path.is_dir():
            continue
        if not path.is_file():
            raise ValueError('native installation archives require regular files')
        relative = path.relative_to(stage).as_posix()
        mode = 0o755 if path.stat().st_mode & 0o111 else 0o644
        # Windows binaries have no POSIX executable bit; manifest modes remain
        # portable and are only used as actual permissions on Unix.
        if relative.startswith('bin/') or relative.endswith(('.exe', '.dll', '.so', '.dylib')):
            mode = 0o755
        path.chmod(mode)
        with path.open('rb') as stream:
            digest = hashlib.file_digest(stream, 'sha256').hexdigest()
        files.append(dict(path=relative, size=path.stat().st_size, sha256=digest, mode=mode))
    paths = {item['path'] for item in files if item['size'] > 0}
    if not set(required) <= paths:
        raise ValueError('native archive is missing a required core/plugin/OS helper component')
    manifest = dict(schema=1, product='zcalld', version=version, target=target,
                    source=source, files=files, required=required)
    (stage / 'INSTALL.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
    (stage / 'INSTALL.json').chmod(0o644)


def native_archive(args, output):
    target = args.target or next(line.split(': ', 1)[1] for line in run('rustc', '-vV').splitlines() if line.startswith('host: '))
    if target not in TARGETS:
        raise ValueError(f'unsupported release target: {target}')
    if not args.no_build:
        subprocess.run(['cargo', 'build', '--release', '--workspace', '--locked'], cwd=ROOT, check=True)
    metadata = json.loads(run('cargo', 'metadata', '--no-deps', '--format-version', '1', '--locked', '--offline'))
    build = Path(metadata['target_directory']) / 'release'
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
        helper_name = "zcalld-installer.exe" if windows else "zcalld-installer"
        shutil.copy2(build / helper_name, binary.parent / helper_name)
        plugins = stage / 'lib/zcalld/plugins'
        plugins.mkdir(parents=True)
        for plugin in PLUGINS:
            name = plugin + '.dll' if windows else 'lib' + plugin + ('.dylib' if darwin else '.so')
            shutil.copy2(build / name, plugins / name)
        if windows or darwin:
            native = build / 'desktop-native'
            destination = stage / 'libexec/zcalld'
            destination.mkdir(parents=True)
            if windows:
                helper = native / 'zcalld-desktop-notify.exe'
                if not helper.is_file():
                    raise ValueError('build and self-test the native desktop helper before packaging')
                shutil.copy2(helper, destination / helper.name)
            else:
                helper = native / 'zcalld-desktop-notify.app'
                if not (helper / 'Contents/MacOS/zcalld-desktop-notify').is_file():
                    raise ValueError('build and self-test the desktop application identity before packaging')
                shutil.copytree(helper, destination / helper.name)
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
        install_manifest(stage, ver(), target, run('git', 'rev-parse', 'HEAD'))
        if windows:
            name = output / (stem + '.zip')
            with zipfile.ZipFile(name, 'w', compression=zipfile.ZIP_DEFLATED) as archive:
                for path in sorted(stage.rglob('*')):
                    if path.is_file():
                        archive.write(path, path.relative_to(stage.parent))
        else:
            name = output / (stem + '.tar.gz')
            tar_tree(stage, name, epoch)
        # Keep the standalone core asset for existing clients; new installers
        # replace only a verified full archive, and bootstrap with this helper.
        shutil.copy2(binary, output / (stem + ('.exe' if windows else '')))
        shutil.copy2(binary.parent / helper_name, output / f"zcalld-installer-{ver()}-{target}{'.exe' if windows else ''}")
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
