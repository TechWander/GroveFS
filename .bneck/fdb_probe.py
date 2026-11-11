#!/usr/bin/env python3
"""One-line cluster facts for the four-zone FDB fixtures.

Reads `fdbcli --exec 'status json'` and prints:

  available=<bool> full_replication=<bool> min_replicas=<n> state=<name>
  zones=<comma list> coordinators=<reachable>/<total>

Usage: fdb_probe.py <cluster-file> <fdbcli> <libdir>

Kept separate from the shell scripts because the same facts are needed before,
during and after a fault, and parsing JSON in shell is how transcripts drift.
"""
import json
import subprocess
import sys

cluster, cli, libdir = sys.argv[1], sys.argv[2], sys.argv[3]


def fail(why):
    print("available=False full_replication=False min_replicas=? state=%s zones=? coordinators=0/?" % why)
    sys.exit(0)


try:
    out = subprocess.run(
        [cli, "-C", cluster, "--exec", "status json"],
        capture_output=True, text=True, timeout=30,
        env={"LD_LIBRARY_PATH": libdir, "PATH": "/usr/local/bin:/usr/bin:/bin"},
    )
except Exception as exc:                                    # noqa: BLE001
    fail("probe-failed:%s" % type(exc).__name__)

if out.returncode != 0 or not out.stdout.strip():
    fail("cli-rc-%s" % out.returncode)

try:
    doc = json.loads(out.stdout)
except json.JSONDecodeError:
    fail("not-json")

cluster_json = doc["cluster"]
data_state = cluster_json.get("data", {}).get("state", {})
zones = sorted({
    p.get("locality", {}).get("zoneid", "?")
    for p in cluster_json.get("processes", {}).values()
})
coords = doc.get("client", {}).get("coordinators", {})
coord_list = coords.get("coordinators", [])
print(
    "available=%s full_replication=%s min_replicas=%s state=%s zones=%s coordinators=%d/%d"
    % (
        cluster_json.get("database_available"),
        cluster_json.get("full_replication"),
        data_state.get("min_replicas_remaining"),
        data_state.get("name"),
        ",".join(zones),
        sum(1 for c in coord_list if c.get("reachable")),
        len(coord_list),
    )
)
