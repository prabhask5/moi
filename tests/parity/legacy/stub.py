"""Stand-ins for the commands service-check.sh calls, so the script itself can judge the parity cases.

`stub.py <tool> <args…>` answers like the tool would for the machine a case describes: the fixture host
`$MOI_BASE` with the case `$MOI_CASE` layered over it (same lookup rules as FixtureHost). Only the call
shapes the script uses are understood. `$MOI_SANDBOX` holds what must persist between calls.
"""
import json
import os
import sys

BASE, CASE, SANDBOX = os.environ["MOI_BASE"], os.environ["MOI_CASE"], os.environ["MOI_SANDBOX"]


def find(rel):
    for top in (CASE, BASE):
        path = os.path.join(top, rel)
        if os.path.exists(path):
            return path
    return None


def read(rel, default=""):
    path = find(rel)
    return open(path).read() if path else default


def key(line):
    h = 0xCBF29CE484222325
    for b in line.encode():
        h = ((h ^ b) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return "%016x" % h


def containers():
    out = []
    for summary in json.loads(read("docker/containers/json.json", "[]")):
        out.append(json.loads(read(f"docker/containers/{summary['Id']}/json.json")))
    return out


def container(name):
    return next((c for c in containers() if c["Name"] == "/" + name), None)


def micros(text):
    whole, _, fraction = text.partition(".")
    return int(whole) * 1_000_000 + int(fraction.ljust(6, "0"))


def network():
    dns, tcp, http = {}, {}, {}
    for top in (BASE, CASE):
        path = os.path.join(top, "net")
        if not os.path.exists(path):
            continue
        for line in open(path):
            words = line.split()
            if not words or words[0].startswith("#"):
                continue
            if words[0] == "dns":
                took = micros(words[3].split("=")[1]) if len(words) > 3 else 0
                dns[words[1]] = (words[2], took)
            elif words[0] == "tcp":
                entry = {"behaviour": words[2], "message": "", "connect": 0, "tls": 0, "first": 0, "rest": 0}
                if words[2] == "tls-error":
                    entry["message"] = " ".join(words[3:])
                else:
                    for word in words[3:]:
                        stage, value = word.split("=")
                        entry[stage] = micros(value)
                tcp[words[1]] = entry
            elif words[0] == "http":
                if words[2] == "-":
                    http.pop(words[1], None)
                else:
                    http[words[1]] = ([int(s) for s in words[2].split(",")], " ".join(words[3:]))
    return dns, tcp, http


def docker(args):
    if args[:2] == ["ps", "-aq"]:
        print("\n".join(c["Id"] for c in containers()))
    elif args[:2] == ["inspect", "-f"]:
        template, names = args[2], args[3:]
        if template.startswith("{{.Name}} {{.State.Status}}"):
            if not names:
                sys.exit(1)
            for c in containers():
                health = (c["State"].get("Health") or {}).get("Status", "-")
                task = c["Config"]["Labels"].get("com.docker.swarm.task.id", "")
                print(f"{c['Name']} {c['State']['Status']} {health} {task}")
            return
        c = container(names[0])
        if c is None:
            sys.exit(1)
        networks = c["NetworkSettings"]["Networks"]
        if '(index .NetworkSettings.Networks "homelab").IPAddress' in template:
            if "homelab" not in networks:
                sys.exit(1)  # the template fails on the missing key
            print(networks["homelab"]["IPAddress"])
        elif "range $k,$v := .NetworkSettings.Networks" in template:
            print("".join(name + " " for name in networks))
        elif template == "{{.State.Health.Status}}":
            if not c["State"].get("Health"):
                sys.exit(1)
            print(c["State"]["Health"]["Status"])
        else:
            sys.exit(f"stub: unknown inspect template {template}")
    elif args[:2] == ["service", "ls"]:
        status = json.loads(read("docker/service-status.json", "{}"))
        for service in sorted(json.loads(read("docker/services.json", "[]")), key=lambda s: s["Spec"]["Name"]):
            name = service["Spec"]["Name"]
            wanted = service["Spec"]["Mode"].get("Replicated", {}).get("Replicas", 1)
            print(name, status.get(name, f"{wanted}/{wanted}"))
    elif args[0] == "exec":
        base = f"docker/exec/{args[1]}/{key(' '.join(args[2:]))}"
        if find(base + ".out") is None:
            sys.exit(126)
        sys.stdout.write(read(base + ".out"))
        sys.exit(int(read(base + ".exit", "0")))
    elif args[:2] == ["network", "connect"]:
        for line in read("docker/fail").splitlines():
            method, prefix = line.split()[:2]
            if method == "POST" and f"/networks/{args[2]}/connect".startswith(prefix):
                sys.exit(1)
    elif args[0] == "logs":
        c = container(args[-1])
        if c is None:
            sys.exit(1)
        sys.stdout.write(read(f"docker/containers/{c['Id']}/logs.txt"))
        sys.stderr.write(read(f"docker/containers/{c['Id']}/logs.err"))
    elif args[0] == "restart":
        pass
    else:
        sys.exit(f"stub: unknown docker call {args}")


def curl(args):
    limit, headers, write_out, to_file, url = 0, [], "", None, None
    i = 0
    while i < len(args):
        arg = args[i]
        if arg in ("-m", "-H", "-w", "-o", "--retry", "--data-urlencode"):
            value = args[i + 1]
            i += 1
            if arg == "-m":
                limit = int(value) * 1_000_000
            elif arg == "-H":
                headers.append(value)
            elif arg == "-w":
                write_out = value
            elif arg == "-o":
                to_file = value
        elif not arg.startswith("-"):
            url = arg
        i += 1
    if "/api/push/" in url:
        return 0  # the push to Kuma goes nowhere
    scheme, rest = url.split("://", 1)
    authority, _, path = rest.partition("/")
    path = "/" + path
    name, _, port = authority.partition(":")
    port = port or ("443" if scheme == "https" else "80")
    site = next((h.split(": ", 1)[1] for h in headers if h.startswith("Host: ")), authority)
    dns, tcp, http = network()

    t = {"lookup": 0, "connect": 0, "tls": 0, "first": 0, "total": 0}
    state = {"code": "000", "ip": "", "body": ""}

    def finish(rc, error=""):
        if error:
            sys.stderr.write(f"curl: ({rc}) {error}\n")
        if to_file is None:
            sys.stdout.write(state["body"])
        fields = {"http_code": state["code"], "time_namelookup": t["lookup"], "time_connect": t["connect"], "time_appconnect": t["tls"],
                  "time_starttransfer": t["first"], "time_total": t["total"], "remote_ip": state["ip"]}
        out = write_out
        for field, value in fields.items():
            shown = value if isinstance(value, str) else "%d.%06d" % divmod(value, 1_000_000)
            out = out.replace("%{" + field + "}", shown)
        sys.stdout.write(out)
        return rc

    def timeout():
        t["total"] = limit
        return finish(28, "Operation timed out")

    # the name
    if name.replace(".", "").isdigit():
        ip = name
    else:
        answer, took = dns.get(name, ("-", 0))
        if answer == "timeout":
            return timeout()
        t["total"] += took
        if answer == "-":
            return finish(6, f"Could not resolve host: {name}")
        ip = answer
        t["lookup"] = t["total"]
    state["ip"] = ip
    # the connection
    how = tcp.get(f"{ip}:{port}")
    if how is None:
        known = any(u.startswith(f"{ip}:{port}/") or (port == "80" and u.startswith(f"{ip}/")) for u in http)
        how = {"behaviour": "ok" if known else "refused", "message": "", "connect": 0, "tls": 0, "first": 0, "rest": 0}
    if how["behaviour"] == "timeout":
        return timeout()
    t["total"] += how["connect"]
    if how["behaviour"] == "refused":
        return finish(7, f"Failed to connect to {name} port {port}: Connection refused")
    t["connect"] = t["total"]
    # TLS
    if scheme == "https":
        if how["behaviour"] == "tls-timeout":
            return timeout()
        if how["behaviour"] == "tls-error":
            return finish(35, how["message"])
        t["total"] += how["tls"]
        t["tls"] = t["total"]
    # the answer
    if how["behaviour"] == "reset":
        # curl says "Recv failure: Connection reset by peer"; the wording of errors is moi's own (spec 05, Deviations)
        return finish(56, "Connection reset by peer")
    if how["behaviour"] == "silent":
        return timeout()
    counter = os.path.join(SANDBOX, "asked-" + key(site + path))
    nth = int(open(counter).read()) if os.path.exists(counter) else 0
    open(counter, "w").write(str(nth + 1))
    statuses, body = http.get(site + path, ([404], "no such fixture answer"))
    state["code"] = str(statuses[min(nth, len(statuses) - 1)])
    t["total"] += how["first"] + (0 if body else how["rest"])
    t["first"] = t["total"]
    if how["behaviour"] == "slow":
        return timeout()
    if body:
        t["total"] += how["rest"]
    state["body"] = body
    return finish(0)


def main():
    tool, args = sys.argv[1], sys.argv[2:]
    if tool == "docker":
        docker(args)
    elif tool == "curl":
        sys.exit(curl(args))
    elif tool == "getent":  # getent ahostsv4 <name>
        answer = network()[0].get(args[1], ("-", 0))[0]
        if answer in ("-", "timeout"):
            sys.exit(2)
        print(f"{answer}       STREAM {args[1]}")
    elif tool == "iptables":  # iptables -S DOCKER-USER
        base = "commands/" + key("iptables " + " ".join(args))
        if find(base + ".missing") and (find(base + ".out") is None or find(base + ".missing").startswith(CASE)):
            sys.exit(127)
        sys.stdout.write(read(base + ".out"))
    else:
        sys.exit(f"stub: unknown tool {tool}")


main()
