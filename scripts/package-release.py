#!/usr/bin/env python3
"""Create deterministic Catify release artifacts using only the stdlib."""
import argparse, gzip, hashlib, json, os, pathlib, shutil, subprocess, tarfile, zipfile


def sha256(path):
    h=hashlib.sha256()
    with open(path,'rb') as f:
        for block in iter(lambda:f.read(1024*1024),b''): h.update(block)
    return h.hexdigest()


def git_log():
    try:
        return subprocess.check_output(['git','log','-10','--pretty=format:- %s'], text=True).strip()
    except (OSError, subprocess.CalledProcessError):
        return '- Release changes unavailable outside a git checkout'


def main():
    p=argparse.ArgumentParser()
    p.add_argument('--binary', required=True, type=pathlib.Path)
    p.add_argument('--version', required=True)
    p.add_argument('--target', required=True)
    p.add_argument('--output', default='dist', type=pathlib.Path)
    p.add_argument('--windows-status', default='not-published')
    ns=p.parse_args()
    if not ns.binary.is_file(): p.error(f'binary not found: {ns.binary}')
    name=f'cfy-v{ns.version}-{ns.target}'
    stage=ns.output/'staging'/name
    shutil.rmtree(stage, ignore_errors=True); stage.mkdir(parents=True)
    binary_name='cfy.exe' if 'windows' in ns.target else 'cfy'
    shutil.copy2(ns.binary, stage/binary_name)
    os.chmod(stage/binary_name, 0o755)
    (stage/'VERSION').write_text(ns.version+'\n')
    (stage/'README.txt').write_text(f'Catify {ns.version} ({ns.target})\nBinary: {binary_name}\n')
    ns.output.mkdir(parents=True, exist_ok=True)
    archive=ns.output/(name + ('.zip' if 'windows' in ns.target else '.tar.gz'))
    if archive.exists(): archive.unlink()
    if archive.suffix == '.zip':
        with zipfile.ZipFile(archive,'w',zipfile.ZIP_DEFLATED) as z:
            for path in sorted(stage.rglob('*')):
                if path.is_file():
                    info=zipfile.ZipInfo(f'{name}/{path.relative_to(stage)}', date_time=(1980,1,1,0,0,0)); info.external_attr=0o100755<<16
                    z.writestr(info,path.read_bytes())
    else:
        with open(archive, 'wb') as raw:
            with gzip.GzipFile(fileobj=raw, mode='wb', mtime=0) as gz:
                with tarfile.open(fileobj=gz, mode='w', format=tarfile.PAX_FORMAT) as t:
                    for path in sorted(stage.rglob('*')):
                        if path.is_file():
                            info=t.gettarinfo(path, arcname=f'{name}/{path.relative_to(stage)}'); info.mtime=0; info.uid=0; info.gid=0; info.uname=''; info.gname=''; info.mode=0o755 if path.name=='cfy' else 0o644
                            with open(path,'rb') as f: t.addfile(info,f)
    digest=sha256(archive)
    sums=ns.output/'SHA256SUMS'
    entries={}
    if sums.exists():
        for line in sums.read_text().splitlines():
            parts=line.split(maxsplit=1)
            if len(parts)==2: entries[parts[1].strip()] = parts[0]
    entries[archive.name] = digest
    sums.write_text(''.join(f'{entries[key]}  {key}\n' for key in sorted(entries)))
    notes=ns.output/'RELEASE_NOTES.md'
    notes.write_text(f'# Catify {ns.version}\n\nTarget: `{ns.target}`\nWindows status: `{ns.windows_status}`\n\n## Changes\n{git_log()}\n')
    print(json.dumps({'archive':str(archive),'sha256':digest,'target':ns.target,'windows_status':ns.windows_status}))

if __name__=='__main__': main()
