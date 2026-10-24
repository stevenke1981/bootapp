"""Download an official evaluation image using checked HTTP ranges; resumable locally.

This prepares a lab image only. It never installs or activates an operating system.
"""
import argparse
import concurrent.futures
import hashlib
import json
import pathlib
import time
import urllib.request
import urllib.parse


def main():
    p = argparse.ArgumentParser()
    p.add_argument('url')
    p.add_argument('output', type=pathlib.Path)
    args = p.parse_args()
    with urllib.request.urlopen(urllib.request.Request(args.url, method='HEAD'), timeout=30) as r:
        size = int(r.headers['Content-Length'])
        final_url = r.url
    parsed = urllib.parse.urlparse(final_url)
    if parsed.scheme != 'https' or parsed.hostname not in {
        'software-static.download.prss.microsoft.com',
        'software-download.microsoft.com',
        'download.microsoft.com',
    }:
        raise ValueError('Expected the Microsoft official evaluation CDN')
    args.output.parent.mkdir(parents=True, exist_ok=True)
    statefile = args.output.with_suffix(args.output.suffix + '.download.json')
    state = {'url': final_url, 'size': size, 'done': []}
    if statefile.exists():
        state = json.loads(statefile.read_text())
        if state['url'] != final_url or state['size'] != size:
            raise ValueError('Resume provenance differs')
        if not args.output.exists() or args.output.stat().st_size != size:
            raise ValueError('Resume output missing or wrong size')
    else:
        with args.output.open('xb') as out:
            out.truncate(size)
    chunk_size = 32 * 1024 * 1024
    count = (size + chunk_size - 1) // chunk_size
    done = set(state['done'])
    started = time.monotonic()

    def download(index):
        begin = index * chunk_size
        end = min(size, begin + chunk_size) - 1
        request = urllib.request.Request(final_url, headers={'Range': f'bytes={begin}-{end}'})
        with urllib.request.urlopen(request, timeout=60) as response:
            if response.status != 206 or response.headers.get('Content-Range') != f'bytes {begin}-{end}/{size}':
                raise ValueError(f'Invalid response range for chunk {index}')
            with args.output.open('r+b') as out:
                out.seek(begin)
                remaining = end - begin + 1
                while remaining:
                    data = response.read(min(1024 * 1024, remaining))
                    if not data:
                        raise ValueError(f'Truncated range {index}')
                    out.write(data)
                    remaining -= len(data)
        return index

    with concurrent.futures.ThreadPoolExecutor(max_workers=8) as executor:
        tasks = [executor.submit(download, i) for i in range(count) if i not in done]
        for completed in concurrent.futures.as_completed(tasks):
            done.add(completed.result())
            state['done'] = sorted(done)
            statefile.write_text(json.dumps(state), encoding='utf-8')
            if len(done) % 16 == 0 or len(done) == count:
                print(f'{len(done)}/{count} chunks; elapsed {time.monotonic()-started:.0f}s', flush=True)
    digest = hashlib.file_digest(args.output.open('rb'), 'sha256').hexdigest()
    state['sha256'] = digest
    state['complete'] = True
    statefile.write_text(json.dumps(state, indent=2), encoding='utf-8')
    print(json.dumps({'path': str(args.output), 'sha256': digest, 'size': size}))


if __name__ == '__main__':
    main()
