"""BIOS fixture acceptance; no real-OS claim. Uses isolated snapshot images."""
import argparse,pathlib,subprocess,time,socket,json

def main():
    p=argparse.ArgumentParser(description=__doc__)
    p.add_argument('--qemu-dir',required=True,type=pathlib.Path)
    p.add_argument('--nasm',required=True,type=pathlib.Path)
    p.add_argument('--image',required=True,type=pathlib.Path)
    p.add_argument('--case',choices=['auto','keyboard','empty'],default='auto')
    a=p.parse_args();root=pathlib.Path(__file__).resolve().parents[1]
    run=root/'out'/f'test-{a.case}-{time.time_ns()}';run.mkdir(parents=True)
    fixture=run/'fixture.img'
    subprocess.run([str(a.nasm),'-f','bin',str(root/'crates/bootapp-bios/asm/fixture.asm'),'-o',str(fixture)],check=True)
    with fixture.open('ab') as f:f.write(bytes(1024*1024-512))
    sock=socket.socket();sock.bind(('127.0.0.1',0));port=sock.getsockname()[1];sock.close()
    cmd=[str(a.qemu_dir/'qemu-system-i386.exe'),'-machine','pc','-accel','tcg','-cpu','pentium3','-m','64',
         '-display','none','-serial','file:serial.log','-monitor','none','-nic','none','-no-reboot',
         '-qmp',f'tcp:127.0.0.1:{port},server=on,wait=off',
         '-drive',f'file={a.image.resolve()},format=raw,if=ide,index=0,snapshot=on']
    if a.case!='empty':cmd+=['-drive',f'file={fixture},format=raw,if=ide,index=1,snapshot=on']
    result={'case':a.case,'command':cmd,'result':'FAIL'}
    conn=None;wire=None
    def qmp(command,arguments=None):
        wire.write((json.dumps({'execute':command,'arguments':arguments or {}})+'\n').encode());wire.flush()
        while True:
            response=json.loads(wire.readline())
            if 'error' in response:raise RuntimeError(response)
            if 'return' in response:return response['return']
    with (run/'stderr.log').open('wb') as errors:
        proc=subprocess.Popen(cmd,cwd=run,stdout=subprocess.DEVNULL,stderr=errors)
        try:
            deadline=time.monotonic()+25
            while time.monotonic()<deadline:
                try:conn=socket.create_connection(('127.0.0.1',port),timeout=1);break
                except OSError:time.sleep(.1)
            if conn is None:raise RuntimeError('QMP connection timeout')
            conn.settimeout(3);wire=conn.makefile('rwb');wire.readline();qmp('qmp_capabilities')
            ready=None;sent=False;entered=False
            while time.monotonic()<deadline:
                serial=(run/'serial.log').read_text(errors='replace') if (run/'serial.log').exists() else ''
                if 'BOOTAPP_BIOS_READY' in serial and ready is None:ready=time.monotonic()
                if a.case=='empty' and ready and time.monotonic()-ready>2:
                    assert 'DETECTED' not in serial and 'CHAINLOAD' not in serial
                    result['result']='PASS';break
                if a.case=='keyboard' and ready:
                    if not sent:qmp('send-key',{'keys':[{'type':'qcode','data':'esc'}]});sent=True
                    if time.monotonic()-ready>7 and not entered:
                        assert 'FIXTURE_OK' not in serial,'timeout was not cancelled'
                        qmp('send-key',{'keys':[{'type':'qcode','data':'ret'}]});entered=True
                if 'BOOTAPP_BIOS_FIXTURE_OK DL=81 REALMODE=1 SEGMENTS=0' in serial:
                    result['result']='PASS';break
                if proc.poll() is not None:raise RuntimeError(f'QEMU exited {proc.returncode}')
                time.sleep(.1)
            else:raise RuntimeError('fixture marker timeout')
            qmp('screendump',{'filename':str(run/'screen.ppm')})
            result['serial']=serial
        except Exception as e:result['error']=str(e)
        finally:
            if wire:
                try:qmp('quit')
                except Exception:pass
                wire.close()
            if conn:conn.close()
            try:proc.wait(timeout=3)
            except subprocess.TimeoutExpired:proc.terminate();proc.wait(timeout=3)
            result['qemu_exit_code']=proc.returncode
    (run/'result.json').write_text(json.dumps(result,indent=2))
    print(json.dumps({'result':result['result'],'report':str(run/'result.json'),'error':result.get('error')}))
    return 0 if result['result']=='PASS' else 1
if __name__=='__main__':raise SystemExit(main())
