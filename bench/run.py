#!/usr/bin/env python3
"""Feather benchmark harness (Linux, stdlib only).

Runs the release binary against the local mock Discord server under a virtual X display and
records, per scenario: startup marks, memory (RSS/PSS/USS summed over the process tree), CPU,
context switches, frame CPU time, network volume and disk activity. Results are written to
bench/out/<name>.json; `--check` compares them with bench/budgets.json and exits non-zero on a
regression.

    python3 bench/run.py --all                 # everything with default durations
    python3 bench/run.py cold warm idle        # selected scenarios
    python3 bench/run.py --all --check         # fail on budget regressions
    python3 bench/run.py long --duration 600   # 10 minute growth test

Requires: Xvfb (or an existing $DISPLAY), cargo-built release binaries
(`cargo build --release -p feather-app -p feather-mock`).
"""
import argparse, json, os, shutil, signal, statistics, subprocess, sys, tempfile, threading, time, urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
sys.path.insert(0, HERE)
import proctree  # noqa: E402

TARGET = os.environ.get("CARGO_TARGET_DIR", os.path.join(ROOT, "target"))
APP = os.path.join(TARGET, "release", "feather")
MOCK = os.path.join(TARGET, "release", "feather-mock")
TOKEN = "MTAwMA.mock.feather-test-token-not-a-secret"
OUT = os.path.join(HERE, "out")
GENERAL = 2000001  # first channel of the first mock guild
IMAGES = 2000002   # the image channel when --image-history > 0


def free_port():
    import socket
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p


class Mock:
    def __init__(self, *args):
        self.port = free_port()
        self.p = subprocess.Popen([MOCK, "--port", str(self.port), *map(str, args)], stdout=subprocess.DEVNULL)
        t = time.time()
        while True:
            try:
                urllib.request.urlopen(f"http://127.0.0.1:{self.port}/control/stats", data=b"").read()
                break
            except Exception:
                if time.time() - t > 15: raise RuntimeError("mock did not start")
                time.sleep(0.05)

    def control(self, path):
        try:
            return urllib.request.urlopen(f"http://127.0.0.1:{self.port}/control/{path}", data=b"").read()
        except Exception:
            return b""

    def stats(self):
        try: return json.loads(self.control("stats") or b"{}")
        except Exception: return {}

    def stop(self):
        self.p.terminate()
        try: self.p.wait(5)
        except subprocess.TimeoutExpired: self.p.kill()


class App:
    def __init__(self, mock, data_dir, env=None):
        e = dict(os.environ, FEATHER_BENCH="1", FEATHER_DATA_DIR=data_dir, FEATHER_API_BASE=f"http://127.0.0.1:{mock.port}",
                 FEATHER_DEV_TOKEN=TOKEN, FEATHER_NO_KEYRING="1", FEATHER_NO_NOTIFY="1", FEATHER_LOG="warn")
        e.update(env or {})
        e.setdefault("DISPLAY", ":99")
        self.marks, self.stats, self.lock = {}, {}, threading.Lock()
        self.t0 = time.time()
        self.p = subprocess.Popen([APP], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=e)
        self.reader = threading.Thread(target=self._read, daemon=True); self.reader.start()

    def _read(self):
        for line in self.p.stdout:
            parts = line.split()
            with self.lock:
                if len(parts) == 3 and parts[0] == "FEATHER_MARK":
                    self.marks[parts[1]] = float(parts[2])
                elif len(parts) == 3 and parts[0] == "FEATHER_STAT":
                    self.stats.setdefault(parts[1], []).append(parts[2])

    def wait_mark(self, name, timeout=20):
        t = time.time()
        while time.time() - t < timeout:
            with self.lock:
                if name in self.marks: return self.marks[name]
            time.sleep(0.01)
        return None

    def last_stat(self, k):
        with self.lock:
            v = self.stats.get(k)
        return float(v[-1]) if v else None

    def mem(self): return proctree.tree_mem_kib(self.p.pid)

    def stop(self):
        self.p.terminate()
        try: self.p.wait(5)
        except subprocess.TimeoutExpired: self.p.kill()


def ensure_display():
    if os.environ.get("DISPLAY"):
        return None
    os.environ["DISPLAY"] = ":99"
    return subprocess.Popen(["Xvfb", ":99", "-screen", "0", "1400x900x24"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def mib(kib): return round(kib / 1024, 1)


def snapshot(app, mock, seconds, label=""):
    """Idle/active measurement window: CPU, switches, memory before/after, io."""
    m0 = app.mem(); pids = proctree.tree(app.p.pid)
    io0 = proctree.io_bytes(pids); wire0 = mock.stats()
    cpu, sw = proctree.idle_cpu(app.p.pid, seconds)
    m1 = app.mem(); io1 = proctree.io_bytes(proctree.tree(app.p.pid))
    return {"cpu_pct": round(cpu, 3), "ctx_switch_per_s": round(sw, 1),
            "rss_mib": mib(m1["rss"]), "pss_mib": mib(m1["pss"]), "uss_mib": mib(m1["uss"]),
            "pss_start_mib": mib(m0["pss"]), "procs": m1["procs"], "threads": m1["threads"],
            "disk_write_kib_per_s": round((io1[1] - io0[1]) / 1024 / seconds, 2), "disk_read_kib": round((io1[0] - io0[0]) / 1024, 1)}


def frame_stats(app):
    return {k: app.last_stat(k) for k in ("frames", "frame_cpu_ms_p50", "frame_cpu_ms_p95", "frame_cpu_ms_max")}


def net_stats(app, mock):
    time.sleep(0.1)
    ms = mock.stats()
    return {"rest_requests": app.last_stat("rest_requests"), "rest_kib": round((app.last_stat("rest_bytes") or 0) / 1024, 1),
            "gateway_wire_kib": round((app.last_stat("gw_bytes_wire") or 0) / 1024, 1),
            "gateway_json_kib": round((app.last_stat("gw_bytes_json") or 0) / 1024, 1),
            "db_kib": round((app.last_stat("db_bytes") or 0) / 1024, 1), "db_messages": app.last_stat("db_messages"),
            "mock_requests": {k: v for k, v in ms.items()}}


def median_of(trials):
    keys = trials[0].keys(); out = {}
    for k in keys:
        vals = [t[k] for t in trials if isinstance(t.get(k), (int, float))]
        if vals:
            out[k] = {"median": round(statistics.median(vals), 3), "min": min(vals), "max": max(vals)}
    return out

# ----------------------------------------------------------------------------------------- scenarios


def startup_trial(mock_args, data_dir, env=None, live=True):
    mock = Mock(*mock_args)
    try:
        app = App(mock, data_dir, env)
        res = {}
        for name in ("ui_first_frame", "ui_interactive", "ui_cached_content", "ui_gateway_connected"):
            res[name + "_ms"] = app.wait_mark(name, 15 if name != "ui_cached_content" else 3)
        if live:
            app.wait_mark("ui_network_content", 10)
            mock.control(f"emit?channel={GENERAL}&content=live-probe")
            res["ui_first_live_message_ms"] = app.wait_mark("ui_first_live_message", 10)
        time.sleep(3)
        res.update(snapshot(app, mock, 2))
        app.stop()
        return {k: v for k, v in res.items() if v is not None}
    finally:
        mock.stop()


def sc_cold(a):
    trials = []
    for _ in range(a.trials):
        d = tempfile.mkdtemp(prefix="feather-cold-")
        try: trials.append(startup_trial(["--history", 300, "--guilds", 3], d))
        finally: shutil.rmtree(d, ignore_errors=True)
    return {"trials": trials, "median": median_of(trials)}


def sc_warm(a):
    d = tempfile.mkdtemp(prefix="feather-warm-")
    try:
        startup_trial(["--history", 300, "--guilds", 3], d)  # populate cache and settings
        trials = [startup_trial(["--history", 300, "--guilds", 3], d) for _ in range(a.trials)]
        return {"trials": trials, "median": median_of(trials)}
    finally:
        shutil.rmtree(d, ignore_errors=True)


def run_phase(a, name, mock_args, env, settle=6, window=None, live=False, post=None):
    window = window or a.duration
    d = tempfile.mkdtemp(prefix=f"feather-{name}-")
    mock = Mock(*mock_args)
    try:
        app = App(mock, d, env)
        app.wait_mark("ui_network_content", 20)
        time.sleep(settle)
        res = snapshot(app, mock, window)
        time.sleep(1)
        res["frames"] = frame_stats(app)
        res["network"] = net_stats(app, mock)
        res["marks"] = dict(app.marks)
        if post: res.update(post(app, mock))
        app.stop()
        return res
    finally:
        mock.stop(); shutil.rmtree(d, ignore_errors=True)


def sc_idle(a): return run_phase(a, "idle", ["--history", 300], {}, window=a.duration)
def sc_active(a): return run_phase(a, "active", ["--history", 300, "--chatter", 2, "--chatter-channels", 1], {}, window=a.duration)
def sc_large(a): return run_phase(a, "large", ["--history", 20000, "--channels", 4], {"FEATHER_BENCH_SCENARIO": "scroll"}, window=a.duration)
def sc_servers(a): return run_phase(a, "servers", ["--guilds", 25, "--channels", 40, "--history", 200, "--chatter", 1, "--chatter-channels", 3], {"FEATHER_BENCH_SCENARIO": "switch", "FEATHER_BENCH_SWITCH_MS": "500"}, window=a.duration)
def sc_images(a): return run_phase(a, "images", ["--image-history", 600, "--history", 100], {"FEATHER_BENCH_CHANNEL": str(IMAGES), "FEATHER_BENCH_SCENARIO": "scroll"}, window=a.duration)
def sc_switch(a): return run_phase(a, "switch", ["--guilds", 3, "--channels", 8, "--history", 500], {"FEATHER_BENCH_SCENARIO": "switch", "FEATHER_BENCH_SWITCH_MS": "150"}, window=a.duration)


def sc_long(a):
    """Memory growth under chatter plus continuous channel switching; sampled every 15 s."""
    d = tempfile.mkdtemp(prefix="feather-long-")
    mock = Mock("--guilds", 5, "--channels", 10, "--history", 500, "--chatter", 5, "--chatter-channels", 5, "--image-history", 100)
    try:
        app = App(mock, d, {"FEATHER_BENCH_SCENARIO": "switch", "FEATHER_BENCH_SWITCH_MS": "800"})
        app.wait_mark("ui_network_content", 20)
        time.sleep(10)
        series, t0 = [], time.time()
        while time.time() - t0 < a.duration:
            m = app.mem(); series.append({"t": round(time.time() - t0), "pss_mib": mib(m["pss"]), "uss_mib": mib(m["uss"]), "rss_mib": mib(m["rss"]), "threads": m["threads"]})
            time.sleep(15)
        # Least-squares slope over the second half (after warm-up) in MiB per hour.
        half = series[len(series) // 2:]
        n = len(half); xs = [p["t"] for p in half]; ys = [p["pss_mib"] for p in half]
        slope = 0.0
        if n > 2:
            mx, my = sum(xs) / n, sum(ys) / n
            den = sum((x - mx) ** 2 for x in xs) or 1
            slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / den * 3600
        res = {"series": series, "pss_growth_mib_per_hour": round(slope, 2), "pss_start_mib": series[0]["pss_mib"], "pss_end_mib": series[-1]["pss_mib"],
               "frames": frame_stats(app), "network": net_stats(app, mock)}
        app.stop()
        return res
    finally:
        mock.stop(); shutil.rmtree(d, ignore_errors=True)


def sc_reconnect(a):
    """Repeated forced disconnects; every one must recover and no message may be lost."""
    d = tempfile.mkdtemp(prefix="feather-reconnect-")
    mock = Mock("--history", 100, "--heartbeat-ms", 2000)
    try:
        app = App(mock, d, {})
        app.wait_mark("ui_network_content", 20)
        ok = 0
        rounds = max(5, int(a.duration // 6))
        for i in range(rounds):
            before = (mock.stats().get("gateway resume", 0), mock.stats().get("gateway identify", 0))
            mock.control("drop" if i % 3 else "reconnect")
            time.sleep(2.0)
            mock.control(f"emit?channel={GENERAL}&content=r{i}")
            time.sleep(1.5)
            after = (mock.stats().get("gateway resume", 0), mock.stats().get("gateway identify", 0))
            if after[0] > before[0] or after[1] > before[1]:
                ok += 1
        res = {"rounds": rounds, "recovered": ok, "recovery_rate": round(ok / rounds, 3), "mock": mock.stats()}
        app.stop()
        return res
    finally:
        mock.stop(); shutil.rmtree(d, ignore_errors=True)


SCENARIOS = {"cold": sc_cold, "warm": sc_warm, "idle": sc_idle, "active": sc_active, "large": sc_large, "servers": sc_servers,
             "images": sc_images, "switch": sc_switch, "long": sc_long, "reconnect": sc_reconnect}

# ----------------------------------------------------------------------------------------- budgets


def dig(d, path):
    for k in path.split("."):
        if isinstance(d, dict) and k in d: d = d[k]
        else: return None
    return d


def check(results, budgets):
    failures = []
    for scen, rules in budgets.items():
        r = results.get(scen)
        if r is None: continue
        for path, limit in rules.items():
            v = dig(r, path)
            if v is None: continue
            if isinstance(v, dict): v = v.get("median")
            lo = path.endswith("_min") or path.endswith("recovery_rate")
            bad = (v < limit) if lo else (v > limit)
            print(f"  {'FAIL' if bad else 'ok  '} {scen}.{path} = {v} (budget {'>=' if lo else '<='} {limit})")
            if bad: failures.append(f"{scen}.{path}")
    return failures


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("scenarios", nargs="*")
    ap.add_argument("--all", action="store_true")
    ap.add_argument("--trials", type=int, default=3)
    ap.add_argument("--duration", type=float, default=30, help="measurement window / scenario length in seconds")
    ap.add_argument("--check", action="store_true")
    ap.add_argument("--budgets", default=os.path.join(HERE, "budgets.json"))
    a = ap.parse_args()
    names = list(SCENARIOS) if a.all else a.scenarios
    if not names:
        ap.error("name scenarios or use --all")
    for p in (APP, MOCK):
        if not os.path.exists(p):
            sys.exit(f"missing {p}: run `cargo build --release -p feather-app -p feather-mock`")
    os.makedirs(OUT, exist_ok=True)
    xvfb = ensure_display(); time.sleep(1)
    results = {}
    try:
        for n in names:
            print(f"== {n}", flush=True)
            t = time.time()
            r = SCENARIOS[n](a)
            r["_env"] = {"duration_s": a.duration, "trials": a.trials, "wall_s": round(time.time() - t), "display": os.environ.get("DISPLAY")}
            results[n] = r
            json.dump(r, open(os.path.join(OUT, f"{n}.json"), "w"), indent=1)
            brief = r.get("median") or {k: v for k, v in r.items() if not isinstance(v, (dict, list))}
            print(json.dumps(brief, indent=1)[:1500], flush=True)
    finally:
        if xvfb: xvfb.terminate()
    if a.check:
        budgets = json.load(open(a.budgets))
        print("== budgets")
        f = check(results, budgets)
        if f:
            sys.exit("budget regressions: " + ", ".join(f))
        print("all budgets met")


if __name__ == "__main__":
    main()
