#!/usr/bin/env python3
"""Focused C boundary ownership/failure tests; uses no Python geospatial packages."""
import ctypes as C
import json
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import threading
import unittest


class Grid(C.Structure):
    _fields_ = [(name, C.c_double) for name in ("xmin", "ymin", "xmax", "ymax", "dx", "dy")] + [(name, C.c_uint64) for name in ("width", "height")]


class Wkb(C.Structure):
    _fields_ = [("data", C.POINTER(C.c_uint8)), ("length", C.c_size_t)]


class Window(C.Structure):
    _fields_ = [("values", C.POINTER(C.c_double)), ("valid", C.POINTER(C.c_uint8)), ("length", C.c_size_t), ("lease", C.c_void_p)]


Read = C.CFUNCTYPE(C.c_int32, C.c_void_p, C.c_size_t, *([C.c_uint64] * 4), C.POINTER(Window), C.c_void_p, C.c_size_t)
Release = C.CFUNCTYPE(None, C.c_void_p, C.c_void_p)
Cancel = C.CFUNCTYPE(C.c_int32, C.c_void_p)


class Request(C.Structure):
    _fields_ = [("abi_version", C.c_uint32), ("strategy", C.c_uint32), ("sources", C.POINTER(Grid)), ("source_count", C.c_size_t),
                ("features", C.POINTER(Wkb)), ("feature_count", C.c_size_t), ("max_cells", C.c_uint64), ("max_live_window_bytes", C.c_uint64),
                ("context", C.c_void_p), ("read", Read), ("release", Release), ("cancelled", Cancel),
                ("statistics_mask", C.c_uint32), ("reserved", C.c_uint32)]


class Metrics(C.Structure):
    _fields_ = [(name, C.c_uint64) for name in ("read_calls", "read_cells", "read_bytes", "peak_live_window_bytes", "callback_nanoseconds", "upstream_nanoseconds", "total_nanoseconds")]


def rectangle(x0, y0, x1, y1):
    return struct.pack("<BIII", 1, 3, 1, 5) + b"".join(struct.pack("<dd", x, y) for x, y in [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)])


LIBRARY = Path(sys.argv.pop(1)).resolve() if len(sys.argv) > 1 and not sys.argv[1].startswith("-") else Path("target/exactextract-control/libskarve_exactextract_bridge.so").resolve()
LIB = C.CDLL(str(LIBRARY))
LIB.skarve_ee_execute_v2.argtypes = [C.POINTER(Request), C.POINTER(C.c_double), C.POINTER(C.c_uint8), C.c_size_t, C.POINTER(Metrics), C.c_void_p, C.c_size_t]
LIB.skarve_ee_execute_v2.restype = C.c_int32


class Call:
    def __init__(self, strategy=0, values=(1, 2, 3, 4), mask=(1, 1, 1, 1), polygons=None, bands=1):
        self.values, self.mask = values, mask
        self.grid = (Grid * bands)(*[Grid(0, 0, 2, 2, 1, 1, 2, 2) for _ in range(bands)])
        self.wkb_bytes = polygons or [rectangle(0, 0, 2, 2)]
        self.buffers = [(C.c_uint8 * len(wkb)).from_buffer_copy(wkb) for wkb in self.wkb_bytes]
        self.features = (Wkb * len(self.buffers))(*[Wkb(buf, len(buf)) for buf in self.buffers])
        self.leases = {}
        self.reads = self.releases = 0
        self.fail = self.cancel = self.reenter = self.malformed = False
        self.reentrant_status = None
        self.cancel_on_read = False
        self.entered = self.proceed = None
        self.read_fn, self.release_fn, self.cancel_fn = Read(self.read), Release(self.release), Cancel(lambda _: int(self.cancel))
        self.request = Request(2, strategy, self.grid, bands, self.features, len(self.features), 262144, 1048576, None, self.read_fn, self.release_fn, self.cancel_fn, 31, 0)

    def read(self, _, source, x0, y0, width, height, out, error, capacity):
        self.reads += 1
        if self.entered:
            self.entered.set()
            if not self.proceed.wait(5):
                return 1
        if self.reenter:
            self.reenter = False
            self.reentrant_status = self.execute()[0]
        if self.fail:
            C.memmove(error, b"injected source failure\0", min(capacity, 24))
            return 1
        cells = [row * 2 + col for row in range(y0, y0 + height) for col in range(x0, x0 + width)]
        values = (C.c_double * len(cells))(*[self.values[i] + source for i in cells])
        valid = (C.c_uint8 * len(cells))(*[self.mask[i] for i in cells])
        key = self.reads
        self.leases[key] = values, valid
        out[0] = Window(values, valid, len(cells) + int(self.malformed), key)
        if self.cancel_on_read:
            self.cancel = True
        return 0

    def release(self, _, key):
        self.releases += 1
        del self.leases[key]

    def execute(self):
        size = len(self.grid) * len(self.features) * 5
        values, defined, metrics, error = (C.c_double * size)(), (C.c_uint8 * size)(), Metrics(), C.create_string_buffer(512)
        status = LIB.skarve_ee_execute_v2(C.byref(self.request), values, defined, size, C.byref(metrics), error, len(error))
        return status, list(values), list(defined), metrics, error.value.decode()


class BridgeTests(unittest.TestCase):
    def test_subset_ignores_unrequested_overflow_and_validates_requested_output(self):
        maximum = sys.float_info.max
        for strategy in [0, 1]:
            for field in [1, 3, 4]:
                call = Call(strategy, values=[maximum] * 4)
                call.request.statistics_mask = 1 << field
                status, values, defined, _, _ = call.execute()
                self.assertEqual(status, 0)
                self.assertEqual(values[1], 4)
                self.assertEqual(defined[1], 1)
                self.assertEqual(values[field], 4 if field == 1 else maximum)
                self.assertEqual(defined[field], 1)
                self.assertEqual(defined[0], 0)
                self.assertEqual(defined[2], 0)
                self.assertFalse(call.leases)
            for field in [0, 2]:
                call = Call(strategy, values=[maximum] * 4)
                call.request.statistics_mask = 1 << field
                self.assertEqual(call.execute()[0], 1)
                self.assertFalse(call.leases)

    def test_subset_mask_admission_and_empty_semantics(self):
        for mask in [0, 32, 0xFFFFFFFF]:
            call = Call(); call.request.statistics_mask = mask
            self.assertEqual(call.execute()[0], 1)
            self.assertEqual(call.reads, 0)
        for strategy in [0, 1]:
            for mask in range(1, 32):
                call = Call(strategy, mask=[0, 0, 0, 0])
                call.request.statistics_mask = mask
                status, values, defined, _, _ = call.execute()
                self.assertEqual(status, 0)
                self.assertEqual(values, [0] * 5)
                self.assertEqual(defined, [int(bool(mask & 1)), 1, 0, 0, 0])

    def test_both_strategies_all_band_feature_order(self):
        for strategy in [0, 1]:
            call = Call(strategy=strategy, polygons=[rectangle(0, 0, 2, 2), rectangle(0, 0, 1, 1)], bands=2)
            status, values, defined, metrics, _ = call.execute()
            self.assertEqual(status, 0)
            self.assertEqual(values, [10, 4, 2.5, 1, 4, 14, 4, 3.5, 2, 5, 3, 1, 3, 3, 3, 4, 1, 4, 4, 4])
            self.assertEqual(defined, [1] * 20)
            self.assertFalse(call.leases)
            self.assertEqual(call.reads, call.releases)
            self.assertGreater(metrics.read_bytes, 0)

    def test_mask_and_empty(self):
        for strategy in [0, 1]:
            call = Call(strategy, mask=[0, 0, 0, 0])
            status, values, defined, _, _ = call.execute()
            self.assertEqual((status, values, defined), (0, [0] * 5, [1, 1, 0, 0, 0]))

    def test_callback_failure_is_not_success(self):
        call = Call(); call.fail = True
        self.assertEqual(call.execute()[0], 3)
        self.assertEqual(call.releases, 0)

    def test_malformed_lease_released(self):
        call = Call(); call.malformed = True
        self.assertEqual(call.execute()[0], 1)
        self.assertEqual(call.releases, 1)
        self.assertFalse(call.leases)

    def test_cancel_before_and_after_read_releases_lease(self):
        call = Call(); call.cancel = True
        self.assertEqual(call.execute()[0], 2)
        self.assertEqual(call.reads, 0)
        call = Call(); call.cancel_on_read = True
        self.assertEqual(call.execute()[0], 2)
        self.assertEqual(call.releases, 1)
        self.assertFalse(call.leases)

    def test_reentrancy_fails_without_deadlock(self):
        call = Call(); call.reenter = True
        self.assertEqual(call.execute()[0], 0)
        self.assertEqual(call.reentrant_status, 1)

    def test_queued_call_can_cancel_without_entering_callback(self):
        first = Call()
        first.entered, first.proceed = threading.Event(), threading.Event()
        completed = []
        worker = threading.Thread(target=lambda: completed.append(first.execute()))
        worker.start()
        try:
            self.assertTrue(first.entered.wait(5))
            second = Call(); second.cancel = True
            self.assertEqual(second.execute()[0], 2)
            self.assertEqual(second.reads, 0)
        finally:
            first.proceed.set()
            worker.join(5)
        self.assertFalse(worker.is_alive())
        self.assertEqual(completed[0][0], 0)

    def test_live_window_budget_fails_without_source_read(self):
        call = Call(); call.request.max_live_window_bytes = 1
        self.assertEqual(call.execute()[0], 4)
        self.assertEqual(call.reads, 0)

    def test_invalid_geometry_caught(self):
        call = Call(polygons=[b"bad"])
        self.assertEqual(call.execute()[0], 1)
        self.assertFalse(call.leases)

    def test_invalid_finite_mask_input_released(self):
        call = Call(values=[float("inf"), 1, 2, 3])
        self.assertEqual(call.execute()[0], 1)
        self.assertFalse(call.leases)
        call = Call(mask=[2, 1, 1, 1])
        self.assertEqual(call.execute()[0], 1)
        self.assertFalse(call.leases)

    def test_same_build_direct_control(self):
        with tempfile.TemporaryDirectory() as temp:
            job = Path(temp) / "job.bin"
            for strategy in [0, 1]:
                geometries = [rectangle(0.3, 0.2, 1.7, 1.9), rectangle(3, 3, 4, 4)]
                job.write_bytes(b"SKAREE01" + struct.pack("<6Q6d", strategy, 2, 2, 1, 2, 262144, 0, 0, 2, 2, 1, 1)
                                + struct.pack("<4d", 1, 2, 3, 4) + bytes([1, 1, 1, 1])
                                + b"".join(struct.pack("<Q", len(geometry)) + geometry for geometry in geometries))
                outputs = [json.loads(subprocess.check_output([str(LIBRARY.parent / "skarve-ee-control"), mode, str(job), "1"])) for mode in ["upstream", "bridge"]]
                self.assertEqual(outputs[0]["runs"][0]["values"], outputs[1]["runs"][0]["values"])
                self.assertEqual(outputs[0]["runs"][0]["defined"], outputs[1]["runs"][0]["defined"])
                self.assertEqual(outputs[0]["runs"][0]["values"][-5:], [0, 0, 0, 0, 0])
                self.assertEqual(outputs[0]["runs"][0]["defined"][-5:], [1, 1, 0, 0, 0])


if __name__ == "__main__":
    unittest.main()
