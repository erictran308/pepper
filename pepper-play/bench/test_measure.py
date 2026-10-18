"""measure.py's parsing and sampling, without a Pepper or a player.

    python3 -I bench/test_measure.py
"""

import importlib.util
import os
import tempfile
import unittest
import unicodedata

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("measure", os.path.join(HERE, "measure.py"))
measure = importlib.util.module_from_spec(spec)
spec.loader.exec_module(measure)

BOX = "pepper-play: Pepper says the box is BoxSize {{ width: {}, height: {}, visible: true{} }}"


class BoxLine(unittest.TestCase):
    def test_every_form_pepper_play_prints(self):
        # As pepper-play's BoxSize prints with Debug: with the display, and
        # without it, as an older one says it.
        for display in (", display: Some(2)", ", display: Some(4128917)", ", display: None", ""):
            text = BOX.format(1920, 1080, display) + "\n"
            self.assertEqual(measure.parse_stderr(text).get("box_px"), [1920, 1080], display)

    def test_the_last_one_is_the_box(self):
        text = "\n".join([BOX.format(640, 360, ", display: None"),
                          "mpv [vo] something",
                          BOX.format(1280, 720, ", display: Some(1)"),
                          BOX.format(1916, 1076, ", display: Some(1)")]) + "\n"
        self.assertEqual(measure.parse_stderr(text)["box_px"], [1916, 1076])

    def test_only_its_own_whole_lines(self):
        for text in (
            # Inside one of mpv's lines, which can hold a video's tags.
            "mpv [cplayer] Title: " + BOX.format(1, 1, ", display: None") + "\n",
            # Not placed, or something else after it.
            BOX.format(1, 1, "").replace("true", "false") + "\n",
            BOX.format(1, 1, ", display: Some(x)") + "\n",
            BOX.format(1, 1, ", display: None") + " and more\n",
        ):
            self.assertNotIn("box_px", measure.parse_stderr(text), text)


class ReadTail(unittest.TestCase):
    def setUp(self):
        self.tail = measure.STDERR_TAIL
        measure.STDERR_TAIL = 200

    def tearDown(self):
        measure.STDERR_TAIL = self.tail

    def tail_of(self, data):
        with tempfile.TemporaryFile() as f:
            f.write(data)
            f.flush()
            return measure.read_tail(f)

    def test_a_cut_line_is_dropped(self):
        fake = BOX.format(9999, 9999, ", display: None")
        real = BOX.format(1280, 720, ", display: Some(1)")
        # The cut lands exactly where a fragment of mpv's line looks like
        # one of pepper-play's.
        after = f"{fake}\n{real}\n".encode()
        before = b"mpv [cplayer] Title: " + b"x" * 500
        measure.STDERR_TAIL = len(after)
        text = self.tail_of(before + after)
        self.assertLessEqual(len(text), measure.STDERR_TAIL)
        self.assertTrue(text.startswith(real))
        self.assertEqual(measure.parse_stderr(text)["box_px"], [1280, 720])
        # The old way: the fragment was taken for a line.
        old = (before + after)[-measure.STDERR_TAIL:].decode()
        self.assertTrue(old.startswith(fake))

    def test_a_whole_line_at_the_cut_stays(self):
        first = BOX.format(640, 360, ", display: None") + "\n"
        line = "pepper-play: " + "y" * (measure.STDERR_TAIL - len(first) - 14) + "\n"
        data = b"z" * 300 + b"\n" + (first + line).encode()
        self.assertEqual(len(first + line), measure.STDERR_TAIL)
        self.assertEqual(self.tail_of(data), first + line)

    def test_a_short_file_whole(self):
        data = BOX.format(1, 2, "") + "\nend"
        self.assertEqual(self.tail_of(data.encode()), data)


class Table(unittest.TestCase):
    def test_counts_from_a_file_are_inert(self):
        # --table reads a file the bench may not have written.
        hostile = "\x1b]52;c;aGk=\x07‮"
        result = {"clip": "c", "kind": "pepper", "player": {}, "counts": {
            "frame-drop-count": hostile, "mistimed-frame-count": hostile,
            "vo-delayed-frame-count": hostile}, "props": {}}
        out = measure.table([result])
        for c in out.replace("\n", ""):
            self.assertNotIn(unicodedata.category(c), ("Cc", "Cf"), repr(out))

    def test_counts_print_as_before(self):
        result = {"clip": "c", "kind": "pepper", "player": {}, "counts": {
            "frame-drop-count": 3, "mistimed-frame-count": None,
            "vo-delayed-frame-count": 0}, "props": {}}
        row = measure.table([result]).splitlines()[1]
        self.assertIn("     3     None       0", row)


def sampler(samples, pids=None):
    s = measure.Sampler.__new__(measure.Sampler)
    s.pids = pids or {"p": 1}
    s.started = {}
    s.samples = samples
    return s


class Sampling(unittest.TestCase):
    def test_a_ps_row_among_rusage_rows(self):
        rows = [(0.0, {"p": (1.0, 10, 100 << 20, 200 << 20, ("rusage", 5))}, None),
                (1.0, {"p": (1.5, 20, 150 << 20, 250 << 20, ("rusage", 5))}, None),
                (2.0, {"p": (2.0, None, None, None, ("ps", "x"))}, None)]
        out = sampler(rows).summary()["p"]
        self.assertEqual(out["wakeups_per_s"], 10)
        self.assertEqual(out["rss_peak_mb"], 150)
        self.assertEqual(out["footprint_peak_mb"], 250)
        self.assertAlmostEqual(out["cpu_mean"], 50)

    def test_ps_rows_alone(self):
        rows = [(0.0, {"p": (1.0, None, None, None, ("ps", "x"))}, None),
                (2.0, {"p": (2.0, None, None, None, ("ps", "x"))}, None)]
        out = sampler(rows).summary()["p"]
        self.assertAlmostEqual(out["cpu_mean"], 50)
        self.assertNotIn("wakeups_per_s", out)

    def take_with(self, rusage, ps):
        saved = measure.rusage, measure.ps_usage, measure.gpu_percent
        measure.rusage = lambda pid: rusage.pop(0)
        measure.ps_usage = lambda pid: ps.pop(0)
        measure.gpu_percent = lambda: None
        try:
            s = sampler([])
            for _ in range(3):
                s.take()
            return [row["p"] for _, row, _ in s.samples]
        finally:
            measure.rusage, measure.ps_usage, measure.gpu_percent = saved

    def test_a_reused_pid_through_ps(self):
        # Another account's process both times; the second took the pid.
        rows = self.take_with([None, None, None],
                              [(1.0, "Sat Oct 18 10:00:00 2026"),
                               (1.1, "Sat Oct 18 10:00:00 2026"),
                               (0.2, "Sat Oct 18 10:00:07 2026")])
        self.assertIsNotNone(rows[0])
        self.assertIsNotNone(rows[1])
        self.assertIsNone(rows[2])

    def test_rusage_then_ps_is_another_process(self):
        rows = self.take_with([(1.0, 1, 1, 1, 77), None, (1.2, 1, 1, 1, 77)],
                              [(5.0, "Sat Oct 18 10:00:07 2026")])
        self.assertIsNotNone(rows[0])
        self.assertIsNone(rows[1])
        self.assertIsNotNone(rows[2])


if __name__ == "__main__":
    unittest.main()
