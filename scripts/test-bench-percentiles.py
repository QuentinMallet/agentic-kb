#!/usr/bin/env python3
import importlib.util, pathlib, unittest
spec=importlib.util.spec_from_file_location("bench_percentiles",pathlib.Path(__file__).with_name("bench-percentiles.py")); module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module)
class PercentileTests(unittest.TestCase):
    def test_nearest_rank_exact(self):
        self.assertEqual(module.percentiles(list(range(1,21))),{"p50_seconds":10,"p95_seconds":19,"max_seconds":20})
if __name__=="__main__": unittest.main()
