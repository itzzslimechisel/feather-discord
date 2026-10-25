"""Process-tree sampling helpers (Linux /proc). Stdlib only.

Memory metrics, summed over the launched process and all descendants:
  rss  - resident set size (double-counts shared pages between processes)
  pss  - proportional set size (shared pages split between sharers; best "total app" figure)
  uss  - private_clean + private_dirty (memory freed if the app exited)
CPU: utime+stime ticks of every process in the tree, sampled at the start and end of a window.
"""
import os, time

CLK = os.sysconf("SC_CLK_TCK")

def children_map():
    m = {}
    for d in os.listdir("/proc"):
        if not d.isdigit():
            continue
        try:
            with open(f"/proc/{d}/stat") as f:
                s = f.read()
            ppid = int(s[s.rfind(")") + 2:].split()[1])
            m.setdefault(ppid, []).append(int(d))
        except (OSError, ValueError, IndexError):
            pass
    return m

def tree(root):
    m, out, stack = children_map(), [], [root]
    while stack:
        p = stack.pop()
        out.append(p)
        stack.extend(m.get(p, []))
    return out

def mem(pid):
    r = {"rss": 0, "pss": 0, "uss": 0}
    try:
        with open(f"/proc/{pid}/smaps_rollup") as f:
            for line in f:
                k, _, v = line.partition(":")
                if k == "Rss": r["rss"] = int(v.split()[0])
                elif k == "Pss": r["pss"] = int(v.split()[0])
                elif k in ("Private_Clean", "Private_Dirty"): r["uss"] += int(v.split()[0])
    except OSError:
        pass
    return r

def tree_mem_kib(root):
    tot = {"rss": 0, "pss": 0, "uss": 0}
    procs = tree(root)
    for p in procs:
        for k, v in mem(p).items():
            tot[k] += v
    tot["procs"] = len(procs)
    tot["threads"] = sum(threads(p) for p in procs)
    return tot

def threads(pid):
    try:
        return len(os.listdir(f"/proc/{pid}/task"))
    except OSError:
        return 0

def cpu_ticks(pids):
    t = 0
    for p in pids:
        try:
            with open(f"/proc/{p}/stat") as f:
                s = f.read()
            fields = s[s.rfind(")") + 2:].split()
            t += int(fields[11]) + int(fields[12])
        except (OSError, ValueError, IndexError):
            pass
    return t

def wakeups(pids):
    """Voluntary + involuntary context switches across all threads (proxy for timer wakeups)."""
    n = 0
    for p in pids:
        try:
            for t in os.listdir(f"/proc/{p}/task"):
                with open(f"/proc/{p}/task/{t}/status") as f:
                    for line in f:
                        if line.startswith(("voluntary_ctxt_switches", "nonvoluntary_ctxt_switches")):
                            n += int(line.split()[1])
        except OSError:
            pass
    return n

def idle_cpu(root, seconds):
    """Return (cpu percent of one core, context switches per second) over the window."""
    pids = tree(root)
    c0, w0, t0 = cpu_ticks(pids), wakeups(pids), time.monotonic()
    time.sleep(seconds)
    pids = tree(root)
    c1, w1, t1 = cpu_ticks(pids), wakeups(pids), time.monotonic()
    dt = t1 - t0
    return 100.0 * (c1 - c0) / CLK / dt, (w1 - w0) / dt


def io_bytes(pids):
    """(read_bytes, write_bytes) that actually hit storage, summed over processes."""
    r = w = 0
    for p in pids:
        try:
            with open(f"/proc/{p}/io") as f:
                for line in f:
                    if line.startswith("read_bytes:"): r += int(line.split()[1])
                    elif line.startswith("write_bytes:"): w += int(line.split()[1])
        except OSError:
            pass
    return r, w
