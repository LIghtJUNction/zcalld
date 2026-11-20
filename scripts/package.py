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
import re
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import zipfile

ROOT = Path(__file__).resolve().parent.parent
PLUGINS = ('zcalld_aliases', 'zcalld_policy', 'zcalld_redact', 'zcalld_community', 'zcalld_plugin_routing', 'zcalld_desktop')
TARGETS = ('x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu',
           'x86_64-pc-windows-msvc', 'aarch64-apple-darwin', 'x86_64-apple-darwin')
COMPAT_TARGETS = dict(zip(TARGETS, ('linux-x86_64', 'linux-aarch64',
                                  'windows-x86_64', 'macos-aarch64', 'macos-x86_64')))
COMPAT_HELPERS = ('zcalld-pluginctl', 'zcalld-privacy', 'zcalld-claude-tui')


def compat_required(target):
    suffix = '.exe' if 'windows' in target else ''
    return ([f'libexec/zcalld/{name}{suffix}' for name in COMPAT_HELPERS]
            + ['libexec/zcalld/' + name for name in
               ('host.js', 'pi.js', 'mcp-tools.mjs', 'builtin/catalog.json')]
            + ['share/doc/zcalld-compat/BUILD.json',
               'share/licenses/zcalld-compat/LICENSE',
               'share/licenses/zcalld-compat/LICENSE.magpie'])


def compat_source():
    """Build the exact gitlink, including official Version/Revision injection."""
    source = ROOT / 'extensions/compat'
    entry = run('git', 'ls-tree', 'HEAD', '--', 'extensions/compat').split()
    if (len(entry) != 4 or entry[:2] != ['160000', 'commit']
            or entry[3] != 'extensions/compat' or not re.fullmatch('[0-9a-f]{40}', entry[2])):
        raise ValueError('native packaging requires the pinned compat gitlink')
    revision = entry[2]
    if (run('git', 'rev-parse', 'HEAD', cwd=source) != revision
            or run('git', 'status', '--porcelain', '--untracked-files=all', cwd=source)):
        raise ValueError('compat checkout differs from its clean pinned source')
    version = (source / 'VERSION').read_text(encoding='utf-8').strip()
    if not re.fullmatch(r'[0-9]+\.[0-9]+\.[0-9]+', version):
        raise ValueError('invalid pinned compat version')
    return source, version, revision


def merge_compat_archive(archive, stage, target, version, revision):
    """Merge only regular, bounded official compat files into the core prefix."""
    windows = 'windows' in target
    if archive.stat().st_size > 256 * 1024 * 1024:
        raise ValueError('compat archive exceeds its limit')
    helpers = set(compat_required(target)[:3])
    with (zipfile.ZipFile(archive) if windows else tarfile.open(archive, 'r:gz')) as package:
        members = package.infolist() if windows else package.getmembers()
        if len(members) > 2048:
            raise ValueError('compat archive has too many entries')
        files, folded, names, total = {}, {}, set(), 0
        for member in members:
            name = member.filename if windows else member.name
            directory = member.is_dir() if windows else member.isdir()
            if windows:
                kind = stat.S_IFMT(member.external_attr >> 16)
                regular = kind in (0, stat.S_IFREG) and not member.flag_bits & 1
            else:
                regular = member.isfile()
            if (not directory and not regular) or (windows and directory and kind not in (0, stat.S_IFDIR)):
                raise ValueError('compat archive contains a link or special entry')
            plain = name[:-1] if directory and name.endswith('/') else name
            parts = plain.split('/')
            devices = {'CON', 'PRN', 'AUX', 'NUL'} | {f'{p}{n}' for p in ('COM', 'LPT') for n in range(1, 10)}
            if (not re.fullmatch(r'[A-Za-z0-9/._+@-]+', plain) or len(plain) > 384
                    or len(parts) > 20 or any(p in ('', '.', '..') or len(p) > 128
                    or p.endswith('.') or p.split('.')[0].upper() in devices for p in parts)):
                raise ValueError('compat archive contains an unsafe path')
            if plain.casefold() in names:
                raise ValueError('compat archive contains duplicate paths')
            names.add(plain.casefold())
            if not windows:
                if parts[0] != 'usr':
                    raise ValueError('compat archive has an unexpected prefix')
                parts = parts[1:]
            for count in range(1, len(parts) + 1):
                prefix = '/'.join(parts[:count])
                if folded.setdefault(prefix.casefold(), prefix) != prefix:
                    raise ValueError('compat archive contains case-colliding paths')
            if directory:
                continue
            path = '/'.join(parts)
            if not (path.startswith(('libexec/zcalld/', 'share/doc/zcalld-compat/',
                                     'share/licenses/zcalld-compat/')) or path in (
                    'lib/systemd/user/zcalld-maintenance.service',
                    'lib/systemd/user/zcalld-maintenance.timer')):
                raise ValueError('compat archive contains an unexpected component')
            size = member.file_size if windows else member.size
            total += size
            if size < 0 or size > 128 * 1024 * 1024 or total > 512 * 1024 * 1024:
                raise ValueError('compat contents exceed their size limit')
            files[path] = (member, size)
        for path in compat_required(target):
            if path not in files or files[path][1] == 0:
                raise ValueError('compat archive is missing a required helper or runtime asset')
        for path in files:
            if any('/'.join(path.split('/')[:i]) in files for i in range(1, len(path.split('/')))):
                raise ValueError('compat file is also a parent directory')
        opener = package.open if windows else package.extractfile
        build_member, build_size = files['share/doc/zcalld-compat/BUILD.json']
        if build_size > 65536:
            raise ValueError('compat build metadata exceeds its limit')
        with opener(build_member) as stream:
            build = json.loads(stream.read(65537))
        if (not isinstance(build, dict) or build.get('version') != version
                or build.get('revision') != revision or build.get('target') != COMPAT_TARGETS[target]
                or build.get('cgo') is not False or build.get('model_weights_included') is not False):
            raise ValueError('compat archive does not match the pinned release build')
        existing = {p.relative_to(stage).as_posix().casefold(): p
                    for p in stage.rglob('*')}
        for path in files:
            if path.casefold() in existing:
                raise ValueError('compat archive would overwrite a core component')
            for parent in Path(path).parents:
                if parent == Path('.'):
                    continue
                found = existing.get(parent.as_posix().casefold())
                if found is not None and (found.is_symlink() or not found.is_dir()
                        or found.relative_to(stage).as_posix() != parent.as_posix()):
                    raise ValueError('compat archive has a conflicting parent')
        for path, (member, size) in files.items():
            destination = stage / path
            destination.parent.mkdir(parents=True, exist_ok=True)
            with opener(member) as source, destination.open('xb') as output:
                remaining = size
                while remaining:
                    chunk = source.read(min(remaining, 65536))
                    if not chunk:
                        raise ValueError('compat archive entry ended early')
                    output.write(chunk)
                    remaining -= len(chunk)
                if source.read(1):
                    raise ValueError('compat archive entry exceeds its declared size')
            destination.chmod(0o755 if path in helpers else 0o644)


def stage_compat(stage, target):
    source, version, revision = compat_source()
    with tempfile.TemporaryDirectory(prefix='zcalld-compat-package-') as directory:
        output = Path(directory)
        subprocess.run([sys.executable, str(source / 'scripts/release.py'), 'build',
                        '--target', COMPAT_TARGETS[target], '--out', str(output)],
                       cwd=source, check=True)
        if compat_source() != (source, version, revision):
            raise ValueError('compat source changed during its build')
        suffix = '.zip' if 'windows' in target else '.tar.gz'
        archive = output / f'zcalld-compat-{version}-{COMPAT_TARGETS[target]}{suffix}'
        merge_compat_archive(archive, stage, target, version, revision)


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
    # Keep the installer's legacy minimum compatible with old prefix receipts.
    # New portable archives explicitly require the complete compat cohort.
    required.extend(compat_required(target))
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
        stage_compat(stage, target)
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
