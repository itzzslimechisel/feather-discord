"""Runtime baseline: same 500-row page (or native equivalent) in each shell, under Xvfb.
Usage: python3 measure.py <name> <marker> -- cmd...
"""
import os, sys, time, subprocess, json, statistics, select
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "..", "bench"))
import proctree

def run(cmd, marker, idle_s):
    t0 = time.time()
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    t_mark = None
    deadline = time.time() + 30
    while time.time() < deadline:
        r, _, _ = select.select([p.stdout], [], [], 0.5)
        if r:
            line = p.stdout.readline()
            if line.startswith(marker):
                t_mark = int(line.split()[1]) / 1000.0
                break
    time.sleep(5)
    m = proctree.tree_mem_kib(p.pid)
    cpu, ws = proctree.idle_cpu(p.pid, idle_s)
    p.terminate()
    try: p.wait(5)
    except subprocess.TimeoutExpired: p.kill()
    return {"startup_ms": round((t_mark - t0) * 1000) if t_mark else None, **m, "idle_cpu_pct": round(cpu, 3), "ctx_switch_per_s": round(ws, 1)}

if __name__ == "__main__":
    name, marker = sys.argv[1], sys.argv[2]
    cmd = sys.argv[sys.argv.index("--") + 1:]
    trials = int(os.environ.get("TRIALS", "3"))
    res = [run(cmd, marker, int(os.environ.get("IDLE_S", "20"))) for _ in range(trials)]
    med = {k: statistics.median([r[k] for r in res if r[k] is not None]) for k in res[0] if any(r[k] is not None for r in res)}
    print(json.dumps({"name": name, "trials": res, "median": med}))
