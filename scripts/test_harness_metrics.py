import importlib.util
import json
import os
import sqlite3
import tempfile
import unittest

SCRIPT = os.path.join(os.path.dirname(__file__), "harness-metrics.py")
spec = importlib.util.spec_from_file_location("harness_metrics", SCRIPT)
hm = importlib.util.module_from_spec(spec)
spec.loader.exec_module(hm)


class HarnessMetricsTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.db = os.path.join(self.tmp.name, "state.sqlite")
        c = sqlite3.connect(self.db)
        c.executescript("""
        CREATE TABLE threads(id TEXT PRIMARY KEY, parent_thread_id TEXT, relationship TEXT);
        CREATE TABLE runs(id TEXT PRIMARY KEY, thread_id TEXT, provider TEXT, model TEXT,
          status TEXT, input_tokens INTEGER, output_tokens INTEGER, reasoning_tokens INTEGER,
          duration_ms INTEGER, error_message TEXT, started_at TEXT);
        CREATE TABLE swarm_boards(id TEXT PRIMARY KEY, coordinator_thread_id TEXT);
        CREATE TABLE swarm_members(thread_id TEXT PRIMARY KEY, role TEXT);
        INSERT INTO threads VALUES ('direct', NULL, NULL), ('coord', NULL, NULL),
          ('worker', NULL, NULL), ('child', 'direct', 'delegated');
        INSERT INTO swarm_boards VALUES ('board', 'coord');
        INSERT INTO swarm_members VALUES ('worker', 'worker');
        INSERT INTO runs VALUES ('r1','direct','"claude"','m','"completed"',100,20,0,1000,NULL,'2026-01-01T00:00:00Z');
        INSERT INTO runs VALUES ('r2','coord','"codex"','m','"failed"',50,0,5,2000,'oops','2026-01-02T00:00:00Z');
        INSERT INTO runs VALUES ('r3','worker','"codex"','m','"cancelled"',25,0,0,3000,'oops','2026-01-03T00:00:00Z');
        INSERT INTO runs VALUES ('r4','child','"claude"','m','"completed"',10,10,0,500,NULL,'2026-01-04T00:00:00Z');
        """)
        c.commit(); c.close()

    def tearDown(self):
        self.tmp.cleanup()

    def test_summary_and_roles(self):
        report = hm.summarize(hm.load_rows(self.db))
        self.assertEqual(report["runs"], 4)
        self.assertEqual(report["completed_runs"], 2)
        self.assertEqual(report["wasted_tokens"], 80)
        self.assertEqual(report["total_tokens"], 220)
        self.assertEqual(report["duration_ms_p50"], 1500)
        self.assertEqual(set(report["by_role"]), {"direct:user", "swarm:coordinator", "swarm:worker", "delegated:child"})
        self.assertEqual(report["by_provider"]["claude"]["runs"], 2)
        self.assertEqual(report["top_errors"][0], {"message": "oops", "count": 2})

    def test_time_filter_and_comparison(self):
        rows = hm.load_rows(self.db, since="2026-01-02T00:00:00Z", until="2026-01-04T00:00:00Z")
        self.assertEqual(len(rows), 2)
        self.assertEqual(hm.main(["--db", self.db, "--compare-before", "2026-01-03T00:00:00Z", "--json"]), 0)

    def test_read_only_connection(self):
        hm.load_rows(self.db)
        with self.assertRaises(sqlite3.OperationalError):
            sqlite3.connect("file:" + self.db + "?mode=ro", uri=True).execute("INSERT INTO threads VALUES ('x',NULL,NULL)")


if __name__ == "__main__":
    unittest.main()
