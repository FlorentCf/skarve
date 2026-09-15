"""Focused checks on the real byte/latency accounting used in cold evidence."""
from pathlib import Path
import sys,tempfile,time,unittest,urllib.request,urllib.error
sys.path.insert(0,str(Path(__file__).resolve().parents[1]/'benchmarks/skv-v0'))
from range_server import served

class ControlledTransport(unittest.TestCase):
    def test_ranges_conditions_denial_and_actual_delay(self):
        with tempfile.TemporaryDirectory() as temporary:
            path=Path(temporary)/'known';path.write_bytes(bytes(range(256))*32)
            with served({'source.skv':path},delay_ms=12,mib_s=1,forbidden=[('source.skv',4096,8191)]) as server:
                def request(method='GET',**headers):
                    return urllib.request.urlopen(urllib.request.Request(server.url('source.skv'),method=method,headers=headers))
                with request('HEAD') as response:etag=response.headers['ETag'];self.assertEqual(response.headers['Content-Length'],'8192')
                start=time.perf_counter()
                with request(Range='bytes=17-1040',**{'If-Match':etag}) as response:self.assertEqual(response.read(),path.read_bytes()[17:1041])
                self.assertGreaterEqual(time.perf_counter()-start,.012)
                for headers,status in [({'Range':'bytes=0-1','If-Match':'"wrong"'},412),({'Range':'bytes=4096-4097'},403),({'Range':'bytes=9000-9001'},416),({},400)]:
                    with self.assertRaises(urllib.error.HTTPError) as error:request(**headers)
                    self.assertEqual(error.exception.code,status)
                # A completed close-delimited response precedes the process-level
                # consumer's completion; allow the server's bookkeeping to finish.
                for _ in range(100):
                    records=server.snapshot()
                    if len(records)==6:break
                    time.sleep(.001)
                self.assertEqual(len(records),6);self.assertEqual(sum(r['body_bytes'] for r in records),1024)
                self.assertTrue(all(r['end_ms']>=r['start_ms']+12 for r in records))

if __name__=='__main__':unittest.main()
