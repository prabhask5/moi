#!/usr/bin/env python3
"""serverstate helper — JSON normalisation, compose discovery, NOTES/CONTEXT generation, restore helpers.
Standard library only (python 3.6+)."""
import datetime
import glob
import json
import os
import re
import shlex
import sys

# --------------------------------------------------------------------------- utils

def load(path, default=None):
    try:
        with open(path) as f:
            return json.load(f)
    except Exception:
        return default


def read(path, default=''):
    try:
        with open(path, errors='replace') as f:
            return f.read()
    except Exception:
        return default


def lines(path):
    return [l for l in read(path).splitlines() if l.strip()]


def dump(obj):
    json.dump(obj, sys.stdout, indent=2, sort_keys=True)
    sys.stdout.write('\n')


def strip_empty(d):
    return {k: v for k, v in d.items() if v not in (None, '', [], {}, False)}


def is_binary(path):
    try:
        with open(path, 'rb') as f:
            return b'\0' in f.read(4096)
    except Exception:
        return True


HEX12 = re.compile(r'^[0-9a-f]{12}$')
HEX64 = re.compile(r'^[0-9a-f]{64}$')

# --------------------------------------------------------------------------- redaction
SENS_KEY = re.compile(r'(PASS|PWD|SECRET|TOKEN|KEY|PRIVATE|CRED|AUTH|SALT|COOKIE|SESSION|SIGNATURE|DSN|WEBHOOK|LICENSE|ACCESS)', re.I)


def redact_text(s):
    if not s:
        return s
    s = re.sub(r'(://[^/\s:@]+:)[^@\s/]+@', r'\1***@', s)
    s = re.sub(r'(?i)\b((?:pass(?:word)?|passwd|pwd|token|secret|api[_-]?key|apikey|access[_-]?key|auth)\s*[=:]\s*)("[^"]*"|\'[^\']*\'|\S+)', r'\1***', s)
    s = re.sub(r'(?i)(bearer\s+)\S+', r'\1***', s)
    s = re.sub(r'(?i)(authorization:\s*)[^"\']+', r'\1***', s)
    s = re.sub(r'(?<![\w/.-])[A-Za-z0-9_+=-]{40,}(?![\w/.-])', '***', s)
    return s


def redact_env(e):
    k, _, v = e.partition('=')
    if v and SENS_KEY.search(k):
        return k + '=***'
    return k + '=' + redact_text(v)


# --------------------------------------------------------------------------- docker normalisation
HC_KEYS = ['Binds', 'PortBindings', 'RestartPolicy', 'NetworkMode', 'Privileged', 'CapAdd', 'CapDrop', 'Devices',
           'DeviceRequests', 'ExtraHosts', 'Dns', 'DnsSearch', 'LogConfig', 'Memory', 'MemoryReservation', 'NanoCpus',
           'CpuShares', 'CpusetCpus', 'ShmSize', 'Tmpfs', 'Sysctls', 'SecurityOpt', 'PidMode', 'IpcMode', 'UsernsMode',
           'VolumesFrom', 'GroupAdd', 'Runtime', 'Init', 'Ulimits', 'ReadonlyRootfs', 'OomKillDisable', 'PidsLimit']


def norm_containers(data):
    out = []
    for c in data or []:
        cfg = c.get('Config') or {}
        if 'com.docker.swarm.task.id' in (cfg.get('Labels') or {}):
            continue
        hc = c.get('HostConfig') or {}
        ns = c.get('NetworkSettings') or {}
        cid = c.get('Id', '')
        nets = {}
        for n, v in sorted((ns.get('Networks') or {}).items()):
            v = v or {}
            ipam = v.get('IPAMConfig') or {}
            aliases = [a for a in (v.get('Aliases') or []) if a != cid[:12] and not HEX12.match(a)]
            nets[n] = strip_empty({'aliases': sorted(set(aliases)), 'ipv4': ipam.get('IPv4Address'),
                                   'ipv6': ipam.get('IPv6Address')})
        hostname = cfg.get('Hostname')
        if hostname and HEX12.match(hostname):
            hostname = None
        mounts = []
        for m in c.get('Mounts') or []:
            name = m.get('Name')
            anon = bool(name and HEX64.match(name))
            mounts.append(strip_empty({
                'type': m.get('Type'), 'name': None if anon else name, 'anonymous': anon,
                'source': None if anon else m.get('Source'), 'destination': m.get('Destination'),
                'ro': not m.get('RW', True), 'driver': m.get('Driver') if m.get('Driver') != 'local' else None}))
        pb = hc.get('PortBindings') or {}
        hcn = strip_empty({k: hc.get(k) for k in HC_KEYS})
        if pb:
            hcn['PortBindings'] = pb
        hcn.pop('Binds', None)  # duplicated by Mounts
        out.append(strip_empty({
            'name': c.get('Name', '').lstrip('/'),
            'image': cfg.get('Image'),
            'image_id': c.get('Image'),
            'cmd': cfg.get('Cmd'),
            'entrypoint': cfg.get('Entrypoint'),
            'env': sorted(cfg.get('Env') or []),
            'labels': cfg.get('Labels') or {},
            'user': cfg.get('User'),
            'hostname': hostname,
            'healthcheck': cfg.get('Healthcheck'),
            'exposed_ports': sorted((cfg.get('ExposedPorts') or {}).keys()),
            'host_config': hcn,
            'mounts': sorted(mounts, key=lambda m: m.get('destination', '')),
            'networks': nets,
        }))
    return sorted(out, key=lambda c: c['name'])


def norm_networks(data):
    out = []
    for n in data or []:
        ipam = n.get('IPAM') or {}
        out.append(strip_empty({
            'name': n.get('Name'), 'id': n.get('Id'), 'driver': n.get('Driver'), 'scope': n.get('Scope'),
            'internal': n.get('Internal'), 'attachable': n.get('Attachable'), 'ipv6': n.get('EnableIPv6'),
            'subnets': [strip_empty({'subnet': x.get('Subnet'), 'gateway': x.get('Gateway'),
                                     'ip_range': x.get('IPRange'), 'aux': x.get('AuxiliaryAddresses')})
                        for x in (ipam.get('Config') or [])],
            'ipam_driver': ipam.get('Driver') if ipam.get('Driver') not in (None, 'default') else None,
            'ipam_options': ipam.get('Options'), 'options': n.get('Options'), 'labels': n.get('Labels'),
            'config_only': n.get('ConfigOnly'), 'config_from': (n.get('ConfigFrom') or {}).get('Network')}))
    return sorted(out, key=lambda n: n['name'])


def norm_volumes(data):
    out = []
    for v in data or []:
        if HEX64.match(v.get('Name', '')):
            continue
        out.append(strip_empty({'name': v.get('Name'), 'driver': v.get('Driver'), 'mountpoint': v.get('Mountpoint'),
                                'labels': v.get('Labels'), 'options': v.get('Options')}))
    return sorted(out, key=lambda v: v['name'])


def docker_diff(old_f, new_f):
    old = {c['name']: c for c in (load(old_f, []) or [])}
    new = {c['name']: c for c in (load(new_f, []) or [])}
    for n in sorted(set(new) - set(old)):
        print('+ container %s (%s)' % (n, new[n].get('image')))
    for n in sorted(set(old) - set(new)):
        print('- container %s (%s)' % (n, old[n].get('image')))
    for n in sorted(set(old) & set(new)):
        changed = [k for k in sorted(set(old[n]) | set(new[n])) if old[n].get(k) != new[n].get(k)]
        if changed:
            print('~ container %s: %s' % (n, ', '.join(changed)))


# --------------------------------------------------------------------------- compose discovery
COMPOSE_NAMES = ('compose.yaml', 'compose.yml', 'docker-compose.yaml', 'docker-compose.yml')


def norm_proj(name):
    return re.sub(r'[^a-z0-9_-]', '', name.lower()) or 'project'


def compose_services(path):
    """Crude, dependency-free list of top-level service names in a compose file."""
    names, in_svc, indent = [], False, None
    for line in read(path).splitlines():
        if not line.strip() or line.lstrip().startswith('#'):
            continue
        if re.match(r'^services:\s*$', line):
            in_svc, indent = True, None
            continue
        if in_svc:
            if not line[0].isspace():
                break
            cur = len(line) - len(line.lstrip())
            if indent is None:
                indent = cur
            if cur == indent:
                m = re.match(r'^\s*["\']?([A-Za-z0-9._-]+)["\']?\s*:', line)
                if m:
                    names.append(m.group(1))
    return names


def compose_projects(containers_f, ls_f, found_f):
    containers = load(containers_f, []) or []
    projects = {}
    pmap = None  # portainer keeps stacks at /data/compose/<id> inside its volume
    for c in containers:
        if 'portainer' in (c.get('image') or ''):
            for m in c.get('mounts', []):
                if m.get('destination') == '/data' and m.get('source'):
                    pmap = m['source']

    def hostpath(p, wd=None):
        p = (p or '').strip()
        if not p:
            return p
        if not p.startswith('/') and wd:
            p = os.path.join(wd, p)
        if pmap and (p == '/data' or p.startswith('/data/')) and not os.path.exists(p):
            return pmap + p[len('/data'):]
        return p

    def entry(p, source):
        return projects.setdefault(p, {'project': p, 'workdir': None, 'config_files': [], 'env_files': [],
                                       'services': [], 'containers': [], 'source': source})

    for c in containers:
        L = c.get('labels', {})
        p = L.get('com.docker.compose.project')
        if not p:
            continue
        e = entry(p, 'running containers')
        wd = hostpath(L.get('com.docker.compose.project.working_dir'))
        if wd:
            e['workdir'] = wd
        for f in (L.get('com.docker.compose.project.config_files') or '').split(','):
            f = hostpath(f, wd)
            if f and f not in e['config_files']:
                e['config_files'].append(f)
        for f in (L.get('com.docker.compose.project.environment_file') or '').split(','):
            f = hostpath(f, wd)
            if f and f not in e['env_files']:
                e['env_files'].append(f)
        s = L.get('com.docker.compose.service')
        if s and s not in e['services']:
            e['services'].append(s)
        e['containers'].append(c['name'])
    for item in load(ls_f, []) or []:
        p = item.get('Name')
        if not p:
            continue
        e = entry(p, 'docker compose ls')
        for f in (item.get('ConfigFiles') or '').split(','):
            f = hostpath(f)
            if f and f not in e['config_files']:
                e['config_files'].append(f)
        if not e['workdir'] and e['config_files']:
            e['workdir'] = os.path.dirname(e['config_files'][0])
    known_files = set(f for e in projects.values() for f in e['config_files'])
    known_dirs = set(e['workdir'] for e in projects.values() if e['workdir'])
    for f in read(found_f).splitlines():
        f = f.strip()
        if not f or f in known_files:
            continue
        d = os.path.dirname(f)
        if d in known_dirs:
            continue
        p = norm_proj(os.path.basename(d))
        base, i = p, 2
        while p in projects:
            p, i = '%s_%d' % (base, i), i + 1
        projects[p] = {'project': p, 'workdir': d, 'config_files': [f], 'env_files': [], 'services': [],
                       'containers': [], 'source': 'found on disk (no containers)'}
        known_dirs.add(d)
    for e in projects.values():
        defined = []
        for f in e['config_files']:
            defined += [s for s in compose_services(f) if s not in defined]
        e['defined_services'] = defined
        e['containers'].sort()
        e['services'].sort()
    return sorted(projects.values(), key=lambda e: e['project'])


def compose_paths(projects_f):
    for e in load(projects_f, []) or []:
        if e.get('workdir'):
            print('dir\t%s' % e['workdir'])
        for f in e.get('config_files', []) + e.get('env_files', []):
            print('file\t%s' % f)


def compose_restore_list(projects_f, ps_f):
    state = {}
    for l in lines(ps_f):
        parts = l.split('\t')
        if len(parts) >= 2:
            state[parts[0]] = parts[1]
    projs = load(projects_f, []) or []

    def prio(e):  # reverse proxies / shared infra first
        return (0 if re.search(r'proxy|traefik|caddy|nginx|swag|infra|network|core', e['project']) else 1, e['project'])
    for e in sorted(projs, key=prio):
        running = any(state.get(c) == 'running' for c in e.get('containers', []))
        print('\x1f'.join([e['project'], e.get('workdir') or '', '|'.join(e.get('config_files', [])),
                         '|'.join(e.get('env_files', [])), 'running' if running else 'stopped']))


# --------------------------------------------------------------------------- standalone containers
SKIP_LABEL_PREFIX = ('org.opencontainers.', 'org.label-schema.', 'maintainer', 'com.docker.', 'build_version',
                     'description', 'vendor', 'version', 'name', 'license', 'url', 'summary')


def standalone_run(containers_f, outdir):
    os.makedirs(outdir, exist_ok=True)
    for old in glob.glob(os.path.join(outdir, '*.sh')):
        os.remove(old)
    for c in load(containers_f, []) or []:
        L = c.get('labels', {})
        if 'com.docker.compose.project' in L:
            continue
        hc = c.get('host_config', {})
        a = [['docker', 'run', '-d'], ['--name', c['name']]]
        rp = hc.get('RestartPolicy') or {}
        if rp.get('Name') and rp.get('Name') != 'no':
            r = rp['Name']
            if r == 'on-failure' and rp.get('MaximumRetryCount'):
                r += ':%d' % rp['MaximumRetryCount']
            a.append(['--restart', r])
        nm = hc.get('NetworkMode') or 'default'
        nets = c.get('networks') or {}
        primary = None
        if nm.startswith('container:'):
            a.append(['--network', nm])
        elif nm in ('host', 'none'):
            a.append(['--network', nm])
        elif nm not in ('default', 'bridge'):
            primary = nm
            a.append(['--network', nm])
        if primary and primary in nets:
            for al in nets[primary].get('aliases', []):
                if al != c['name']:
                    a.append(['--network-alias', al])
            if nets[primary].get('ipv4'):
                a.append(['--ip', nets[primary]['ipv4']])
        if c.get('hostname'):
            a.append(['--hostname', c['hostname']])
        if c.get('user'):
            a.append(['--user', c['user']])
        seen = set()
        for k, binds in sorted((hc.get('PortBindings') or {}).items()):
            for b in binds or []:
                hip, hp = b.get('HostIp') or '', b.get('HostPort') or ''
                spec = (hip + ':' if hip and hip not in ('0.0.0.0', '::') else '') + (hp + ':' if hp else '') + k.replace('/tcp', '')
                if spec not in seen:
                    seen.add(spec)
                    a.append(['-p', spec])
        for m in c.get('mounts', []):
            ro = ':ro' if m.get('ro') else ''
            if m.get('type') == 'bind':
                a.append(['-v', '%s:%s%s' % (m['source'], m['destination'], ro)])
            elif m.get('type') == 'volume' and m.get('name'):
                a.append(['-v', '%s:%s%s' % (m['name'], m['destination'], ro)])
        for k, v in sorted((hc.get('Tmpfs') or {}).items()):
            a.append(['--tmpfs', k + (':' + v if v else '')])
        for e in c.get('env', []):
            if not e.startswith('PATH='):
                a.append(['-e', e])
        for k, v in sorted(L.items()):
            if not k.startswith(SKIP_LABEL_PREFIX):
                a.append(['--label', '%s=%s' % (k, v)])
        if hc.get('Privileged'):
            a.append(['--privileged'])
        for x in hc.get('CapAdd') or []:
            a.append(['--cap-add', x])
        for x in hc.get('CapDrop') or []:
            a.append(['--cap-drop', x])
        for d in hc.get('Devices') or []:
            a.append(['--device', '%s:%s' % (d.get('PathOnHost'), d.get('PathInContainer'))])
        if any('gpu' in str(r.get('Capabilities')) for r in (hc.get('DeviceRequests') or [])):
            a.append(['--gpus', 'all'])
        for h in hc.get('ExtraHosts') or []:
            a.append(['--add-host', h])
        for d in hc.get('Dns') or []:
            a.append(['--dns', d])
        lc = hc.get('LogConfig') or {}
        if lc.get('Type') and lc.get('Type') != 'json-file':
            a.append(['--log-driver', lc['Type']])
        for k, v in sorted((lc.get('Config') or {}).items()):
            a.append(['--log-opt', '%s=%s' % (k, v)])
        if hc.get('Memory'):
            a.append(['--memory', str(hc['Memory'])])
        if hc.get('NanoCpus'):
            a.append(['--cpus', '%g' % (hc['NanoCpus'] / 1e9)])
        if hc.get('ShmSize') and hc['ShmSize'] != 67108864:
            a.append(['--shm-size', str(hc['ShmSize'])])
        for k, v in sorted((hc.get('Sysctls') or {}).items()):
            a.append(['--sysctl', '%s=%s' % (k, v)])
        for x in hc.get('SecurityOpt') or []:
            a.append(['--security-opt', x])
        if hc.get('PidMode'):
            a.append(['--pid', hc['PidMode']])
        if hc.get('IpcMode') and hc['IpcMode'] not in ('private', 'shareable'):
            a.append(['--ipc', hc['IpcMode']])
        if hc.get('Init'):
            a.append(['--init'])
        for g in hc.get('GroupAdd') or []:
            a.append(['--group-add', g])
        if hc.get('Runtime') and hc['Runtime'] != 'runc':
            a.append(['--runtime', hc['Runtime']])
        for u in hc.get('Ulimits') or []:
            a.append(['--ulimit', '%s=%s:%s' % (u.get('Name'), u.get('Soft'), u.get('Hard'))])
        args = list(c.get('cmd') or [])
        ep = c.get('entrypoint') or []
        if ep:
            a.append(['--entrypoint', ep[0]])
            args = ep[1:] + args
        a.append([c['image']] + args)
        body = ' \\\n  '.join(' '.join(shlex.quote(x) for x in grp) for grp in a)
        extra = ''
        for n, v in nets.items():
            if n in (primary, 'bridge') or nm in ('host', 'none') or nm.startswith('container:'):
                continue
            cmd = ['docker', 'network', 'connect'] + sum([['--alias', al] for al in v.get('aliases', []) if al != c['name']], [])
            if v.get('ipv4'):
                cmd += ['--ip', v['ipv4']]
            extra += ' '.join(shlex.quote(x) for x in cmd + [n, c['name']]) + '\n'
        path = os.path.join(outdir, re.sub(r'[^A-Za-z0-9_.-]', '_', c['name']) + '.sh')
        with open(path, 'w') as f:
            f.write('#!/bin/sh\n# Recreate standalone container "%s" — generated by serverstate from docker inspect.\n'
                    '# Review before running. Better: move it into a docker compose file.\nset -e\n%s\n%s' % (c['name'], body, extra))
        os.chmod(path, 0o700)


def network_create_cmds(networks_f):
    for n in load(networks_f, []) or []:
        if n['name'] in ('bridge', 'host', 'none') or n.get('driver') in ('host', 'null', 'overlay') or n.get('scope') == 'swarm':
            continue
        if (n.get('labels') or {}).get('com.docker.stack.namespace'):
            continue  # created by the swarm stack itself
        a = ['docker', 'network', 'create', '--driver', n.get('driver', 'bridge')]
        for s in n.get('subnets', []):
            if s.get('subnet'):
                a += ['--subnet', s['subnet']]
            if s.get('gateway'):
                a += ['--gateway', s['gateway']]
            if s.get('ip_range'):
                a += ['--ip-range', s['ip_range']]
        if n.get('internal'):
            a.append('--internal')
        if n.get('attachable'):
            a.append('--attachable')
        if n.get('ipv6'):
            a.append('--ipv6')
        if n.get('ipam_driver'):
            a += ['--ipam-driver', n['ipam_driver']]
        for k, v in sorted((n.get('options') or {}).items()):
            a += ['--opt', '%s=%s' % (k, v)]
        for k, v in sorted((n.get('labels') or {}).items()):
            a += ['--label', '%s=%s' % (k, v)]
        if n.get('config_only'):
            a.append('--config-only')
        if n.get('config_from'):
            a += ['--config-from', n['config_from']]
        a.append(n['name'])
        print('%s\t%s' % (n['name'], ' '.join(shlex.quote(x) for x in a)))


# --------------------------------------------------------------------------- referenced scripts
SKIP_PREFIX = ('/usr/bin/', '/bin/', '/usr/sbin/', '/sbin/', '/usr/lib/', '/lib/', '/lib64/', '/usr/share/', '/dev/',
               '/proc/', '/sys/', '/tmp/', '/run/', '/var/run/', '/var/log/', '/etc/', '/var/lib/docker/',
               '/var/lib/serverstate', '/var/cache/', '/snap/', '/usr/libexec/', '/usr/local/lib/python')
PATH_RX = re.compile(r'(?<![\w.$~-])(/(?:[\w.@%+:,=-]+/)*[\w.@%+:,=-]+)')


def referenced_paths(files_root, maxkb):
    maxb = int(maxkb) * 1024
    pats = ['etc/crontab', 'etc/anacrontab', 'etc/rc.local', 'etc/cron.d/*', 'etc/cron.daily/*', 'etc/cron.hourly/*',
            'etc/cron.weekly/*', 'etc/cron.monthly/*', 'var/spool/cron/crontabs/*', 'var/spool/cron/*',
            'etc/systemd/system/*.service', 'etc/systemd/system/*.timer', 'etc/systemd/system/*/*.conf',
            'home/*/.config/systemd/user/*.service', 'root/.config/systemd/user/*.service']
    queue = []
    for pat in pats:
        for p in glob.glob(os.path.join(files_root, pat)):
            if os.path.isfile(p):
                queue.append((read(p), 0))
    seen, out = set(), []
    while queue:
        text, depth = queue.pop()
        for line in text.splitlines():
            if line.lstrip().startswith('#'):
                continue
            for m in PATH_RX.findall(line):
                p = m.rstrip('.,:;')
                if p in seen:
                    continue
                seen.add(p)
                if p.startswith(SKIP_PREFIX) or not os.path.isfile(p):
                    continue
                try:
                    if os.path.getsize(p) > maxb or is_binary(p):
                        continue
                except OSError:
                    continue
                out.append(p)
                if depth < 2:
                    queue.append((read(p), depth + 1))
    print('\n'.join(sorted(out)))


def referenced_etc(files_root):
    """/etc files read by custom units, scripts and crontabs — restore must put these back."""
    srcs = []
    for pat in ['etc/systemd/system/*.service', 'etc/systemd/system/*.path', 'etc/systemd/system/*/*.conf', 'etc/cron.d/*',
                'etc/crontab', 'var/spool/cron/crontabs/*', 'usr/local/bin/*', 'usr/local/sbin/*', 'opt/scripts/*', 'root/bin/*']:
        srcs += [p for p in glob.glob(os.path.join(files_root, pat))
                 if os.path.isfile(p) and not os.path.islink(p) and not is_binary(p) and 'serverstate' not in os.path.basename(p)]
    out = set()
    for p in srcs:
        for m in re.findall(r'(?<![\w.-])(/etc/[\w.@+-]+(?:/[\w.@+-]+)*)', read(p)):
            m = m.rstrip('.')
            if os.path.exists(os.path.join(files_root, m.lstrip('/'))) and not under(m, HANDLED_ETC + GENERATED_ETC):
                d = os.path.dirname(m)
                # a file in a custom dir (e.g. /etc/host-firewall/rules4.txt) -> restore the whole dir
                out.add(d if d.count('/') >= 2 and not any(d == x for x in ('/etc/default', '/etc/systemd', '/etc/security')) else m)
    return sorted(out)


# --------------------------------------------------------------------------- storage helpers

def human(n):
    try:
        n = float(n)
    except (TypeError, ValueError):
        return str(n or '')
    for u in ('B', 'K', 'M', 'G', 'T', 'P'):
        if n < 1024:
            return ('%.1f%s' % (n, u)).replace('.0', '')
        n /= 1024
    return '%.1fE' % n


def flat_blk(devs, parent=None):
    for d in devs or []:
        d = dict(d)
        d['_parent'] = parent
        yield d
        for c in flat_blk(d.get('children'), d.get('name')):
            yield c


def disk_kind(d):
    if (d.get('tran') or '') == 'nvme' or (d.get('name') or '').startswith('nvme'):
        return 'NVMe SSD'
    if d.get('rota') in (True, '1', 1):
        return 'HDD'
    if d.get('rota') in (False, '0', 0):
        return 'SSD'
    return ''


def lsblk_rows(path):
    data = load(path, {}) or {}
    rows = []
    for d in flat_blk(data.get('blockdevices')):
        rows.append([('  ' if d['_parent'] else '') + (d.get('name') or ''), human(d.get('size')), d.get('type') or '',
                     disk_kind(d) if d.get('type') == 'disk' else '', d.get('fstype') or '', d.get('label') or '',
                     d.get('mountpoint') or '', (d.get('model') or '').strip(), d.get('uuid') or ''])
    return rows


def lsblk_table(path):
    rows = [['NAME', 'SIZE', 'TYPE', 'KIND', 'FSTYPE', 'LABEL', 'MOUNT', 'MODEL', 'UUID']] + lsblk_rows(path)
    w = [max(len(str(r[i])) for r in rows) for i in range(len(rows[0]))]
    for r in rows:
        print('  '.join(str(x).ljust(w[i]) for i, x in enumerate(r)).rstrip())


def lsblk_match(path, src):
    data = load(path, {}) or {}
    key, _, val = src.partition('=')
    for d in flat_blk(data.get('blockdevices')):
        if (key == 'UUID' and d.get('uuid') == val) or (key == 'LABEL' and d.get('label') == val) or \
           (key == 'PARTUUID' and d.get('partuuid') == val) or src in (d.get('path'), '/dev/' + (d.get('name') or '')):
            return d
    return None


def lsblk_find(path, src):
    d = lsblk_match(path, src)
    if not d:
        print('(not found in old lsblk)')
        return
    print('%s %s %s label=%s model=%s' % (d.get('name'), human(d.get('size')), d.get('fstype') or '',
                                          d.get('label') or '-', (d.get('model') or '').strip() or '-'))


def lsblk_label(path, src):
    d = lsblk_match(path, src)
    print((d or {}).get('label') or '')


# --------------------------------------------------------------------------- data locations / coverage
IGNORE_BIND = ('/var/run/docker.sock', '/run/docker.sock', '/etc/localtime', '/etc/timezone', '/dev', '/proc', '/sys',
               '/run', '/var/run', '/tmp', '/etc/hostname', '/etc/hosts', '/etc/resolv.conf', '/lib/modules', '/usr',
               '/etc/machine-id', '/var/log', '/etc/ssl', '/etc/os-release')


def data_locations(snap):
    """(path, what, ro) for everything that holds application data."""
    locs = []

    def ok(src):
        return src and not any(src == p or src.startswith(p + '/') for p in IGNORE_BIND)
    for c in load(os.path.join(snap, 'docker/containers.json'), []) or []:
        for m in c.get('mounts', []):
            src = m.get('source')
            if m.get('type') == 'bind' and ok(src):
                locs.append((src, 'bind mount → %s:%s' % (c['name'], m.get('destination')), bool(m.get('ro'))))
            elif m.get('type') == 'volume' and m.get('name'):
                locs.append((src or '(docker volume %s)' % m['name'], 'volume %s → %s' % (m['name'], c['name']), bool(m.get('ro'))))
    for sv in load(os.path.join(snap, 'docker/services.json'), []) or []:
        for m in sv.get('mounts', []):
            src = m.get('source')
            if m.get('type') == 'bind' and ok(src):
                locs.append((src, 'bind mount → swarm service %s:%s' % (sv['name'], m.get('destination')), bool(m.get('ro'))))
            elif m.get('type') == 'volume' and src:
                locs.append(('/var/lib/docker/volumes/%s/_data' % src, 'volume %s → swarm service %s' % (src, sv['name']), bool(m.get('ro'))))
    for e in load(os.path.join(snap, 'docker/compose-projects.json'), []) or []:
        if e.get('workdir'):
            locs.append((e['workdir'], 'compose stack dir (%s)' % e['project'], False))
    if os.path.isdir(os.path.join(snap, 'incus')):
        for f in sorted(glob.glob(os.path.join(snap, 'incus/instances/*.yaml'))):
            locs.append(('/var/lib/incus', 'Incus instance %s (disk + config)' % os.path.basename(f)[:-5], False))
    seen, out = set(), []
    for p, w, ro in sorted(locs):
        if (p, w) not in seen:
            seen.add((p, w))
            out.append((p, w, ro))
    return out


def intentional():
    return os.environ.get('SS_NOT_BACKED_UP_OK', '').split()


def covered(path, backup_paths):
    if under(path, intentional()):
        return 'intentional'
    if not backup_paths:
        return None
    for b in backup_paths:
        b = b.rstrip('/') or '/'
        if path == b or path.startswith(b + '/') or b == '/':
            return True
    return False


def print_data_locations(snap, backup):
    bp = backup.split()
    for p, w, ro in data_locations(snap):
        c = covered(p, bp)
        mark = '?' if c is None else ('intentional' if c == 'intentional' else ('✓' if c else ('read-only view' if ro else '✗ NOT IN BACKUP_PATHS')))
        print('%-55s %-45s %s' % (p, w, mark))


# --------------------------------------------------------------------------- notes
NOTES_HEADER = """# Owner notes — {host}

Free-form notes that serverstate merges into CONTEXT.md (the file you give to Claude).
Write *why* things exist, who uses them, URLs, gotchas, plans. Keep the `## ` headings:
serverstate adds a stub heading for every new stack / container / unit / scheduled script,
and never deletes or rewrites what you wrote. Lines that are just "TODO" are ignored.

## Overview
TODO: what this server is for, who uses it, how it's reached (domain, Tailscale, LAN IP…)

## Gotchas / do not touch
TODO

## Planned changes / ideas
TODO
"""


def parse_notes(text):
    notes, cur, buf = {}, None, []
    for line in text.splitlines():
        m = re.match(r'^##\s+(.*?)\s*$', line)
        if m:
            if cur is not None:
                notes[cur] = '\n'.join(buf).strip()
            cur, buf = m.group(1), []
        elif cur is not None:
            buf.append(line)
    if cur is not None:
        notes[cur] = '\n'.join(buf).strip()
    return notes


def meaningful(t):
    t = (t or '').strip()
    return bool(t) and not re.fullmatch(r'TODO[^\n]*', t)


def notes_stubs(state):
    snap = os.path.join(state, 'snapshot')
    path = os.path.join(state, 'NOTES.md')
    host = read(os.path.join(snap, 'meta/hostname')).strip() or os.uname()[1]
    text = read(path) or NOTES_HEADER.format(host=host)
    have = set(parse_notes(text))
    want = []
    for e in load(os.path.join(snap, 'docker/compose-projects.json'), []) or []:
        want.append(('Stack: %s' % e['project'], 'TODO: what it does, URL, who uses it, anything non-obvious'))
    for c in load(os.path.join(snap, 'docker/containers.json'), []) or []:
        if 'com.docker.compose.project' not in c.get('labels', {}):
            want.append(('Container: %s' % c['name'], 'TODO: what it is (standalone container — consider moving to compose)'))
    for f in sorted(glob.glob(os.path.join(snap, 'incus/instances/*.yaml'))):
        want.append(('Instance: %s' % os.path.basename(f)[:-5], 'TODO: what this VM/container is for; is its disk backed up or rebuildable?'))
    for sv in load(os.path.join(snap, 'docker/services.json'), []) or []:
        want.append(('Service: %s' % sv['name'], 'TODO: what this swarm/Dokploy service is'))
    for u in lines(os.path.join(snap, 'systemd/custom-units.txt')):
        if not u.endswith('.conf'):
            want.append(('Unit: %s' % os.path.basename(u), 'TODO: what this service/timer does'))
    for s in sched_scripts(snap):
        want.append(('Script: %s' % s, 'TODO: what this script does'))
    add = ['\n## %s\n%s\n' % (h, t) for h, t in want if h not in have]
    if add or not os.path.exists(path):
        with open(path, 'w') as f:
            f.write(text.rstrip('\n') + '\n' + ''.join(add))
        os.chmod(path, 0o600)


# --------------------------------------------------------------------------- cron parsing
def parse_cron(text, system):
    jobs, env = [], []
    for raw in text.splitlines():
        l = raw.strip()
        if not l or l.startswith('#'):
            continue
        if re.match(r'^[A-Za-z_][A-Za-z0-9_]*\s*=', l):
            env.append(l)
            continue
        if l.startswith('@'):
            parts = l.split(None, 2 if system else 1)
            sched = parts[0]
            rest = parts[1:]
        else:
            parts = l.split(None, 6 if system else 5)
            sched = ' '.join(parts[:5])
            rest = parts[5:]
        if system:
            user = rest[0] if rest else '?'
            cmd = rest[1] if len(rest) > 1 else ''
        else:
            user, cmd = None, rest[0] if rest else ''
        jobs.append((sched, user, cmd))
    return jobs, env


def all_cron(snap):
    F = os.path.join(snap, 'files')
    out = []  # (source, sched, user, cmd)
    for f in sorted(glob.glob(os.path.join(snap, 'cron/user-*.crontab'))):
        u = os.path.basename(f)[5:-8]
        for s, _, c in parse_cron(read(f), False)[0]:
            out.append(('crontab -u %s' % u, s, u, c))
    for f in [os.path.join(F, 'etc/crontab')] + sorted(glob.glob(os.path.join(F, 'etc/cron.d/*'))):
        if os.path.isfile(f):
            for s, u, c in parse_cron(read(f), True)[0]:
                if 'run-parts' in c and '/etc/cron.' in c:
                    continue
                out.append(('/' + os.path.relpath(f, F), s, u, c))
    return out


def sched_scripts(snap):
    """Scripts that are actually invoked by cron or systemd (used for notes stubs)."""
    idx = set(lines(os.path.join(snap, 'scripts/index.txt')))
    text = '\n'.join(c for _, _, _, c in all_cron(snap))
    for u in lines(os.path.join(snap, 'systemd/custom-units.txt')):
        text += '\n' + read(os.path.join(snap, 'files', u.lstrip('/')))
    return sorted(s for s in idx if s in text)


def unit_info(path):
    info = {}
    for l in read(path).splitlines():
        m = re.match(r'^\s*(Description|ExecStart|ExecStartPre|User|WorkingDirectory|OnCalendar|OnBootSec|OnUnitActiveSec|Requires|After|What|Where|Type)\s*=\s*(.*)$', l)
        if m:
            info.setdefault(m.group(1), []).append(m.group(2))
    return info


# --------------------------------------------------------------------------- CONTEXT.md
def md_table(headers, rows):
    def esc(x):
        return str(x if x is not None else '').replace('|', '\\|').replace('\n', ' ')
    out = ['| ' + ' | '.join(headers) + ' |', '|' + '---|' * len(headers)]
    out += ['| ' + ' | '.join(esc(x) for x in r) + ' |' for r in rows]
    return '\n'.join(out)


def code(text, lang='', cap=150):
    ls = text.rstrip('\n').splitlines()
    more = ''
    if len(ls) > cap:
        more = '\n… (%d more lines — see full file)' % (len(ls) - cap)
        ls = ls[:cap]
    return '```%s\n%s%s\n```' % (lang, '\n'.join(ls), more)


INTERESTING_LABEL = re.compile(r'^(traefik\.|caddy|homepage\.|com\.centurylinklabs\.watchtower|diun\.|autoheal|swag|tsdproxy|glance|kuma|wud\.|dockflare|cloudflare)')


def port_list(hc):
    out = []
    for k, binds in sorted((hc.get('PortBindings') or {}).items()):
        for b in binds or []:
            hip = b.get('HostIp') or ''
            s = ('%s:' % hip if hip not in ('', '0.0.0.0', '::') else '') + '%s→%s' % (b.get('HostPort'), k)
            if s not in out:
                out.append(s)
    return out


def proxy_routes(containers, F):
    routes = []  # (domain, target, via)
    for c in containers:
        L = c.get('labels', {})
        for k, v in L.items():
            m = re.match(r'^traefik\.http\.routers\.([^.]+)\.rule$', k)
            if m:
                for d in re.findall(r'Host(?:Regexp)?\(([^)]*)\)', v):
                    for h in re.findall(r'[`"\']([^`"\']+)[`"\']', d):
                        routes.append((h, c['name'], 'traefik label (router %s)' % m.group(1)))
            if re.match(r'^caddy(_\d+)?$', k):
                for h in v.replace(',', ' ').split():
                    routes.append((h, c['name'], 'caddy-docker-proxy label'))
    for f in glob.glob(os.path.join(F, '**', 'Caddyfile*'), recursive=True):
        depth = 0
        for l in read(f).splitlines():
            s = l.split('#')[0].strip()
            if not s:
                continue
            if depth == 0 and s.endswith('{') and not s.startswith(('(', '{')):
                for h in s[:-1].replace(',', ' ').split():
                    routes.append((h, '', 'Caddyfile /' + os.path.relpath(f, F)))
            depth += s.count('{') - s.count('}')
    confs = glob.glob(os.path.join(F, 'etc/nginx/sites-enabled/*')) + glob.glob(os.path.join(F, 'etc/nginx/conf.d/*.conf'))
    confs += [p for p in glob.glob(os.path.join(F, '**', '*.conf'), recursive=True) if '/nginx' in p and '/etc/nginx' not in p][:200]
    for f in confs:
        for m in re.findall(r'server_name\s+([^;]+);', read(f)):
            for h in m.split():
                if h != '_':
                    routes.append((h, '', 'nginx /' + os.path.relpath(f, F)))
    for f in glob.glob(os.path.join(F, '**', 'cloudflared', '**', '*.y*ml'), recursive=True) + \
            glob.glob(os.path.join(F, 'etc/cloudflared/*.y*ml')):
        host = None
        for l in read(f).splitlines():
            m = re.match(r'^\s*-?\s*hostname:\s*(\S+)', l)
            if m:
                host = m.group(1).strip('"\'')
            m = re.match(r'^\s*-?\s*service:\s*(\S+)', l)
            if m and host:
                routes.append((host, m.group(1), 'cloudflared tunnel /' + os.path.relpath(f, F)))
                host = None
    seen, out = set(), []
    for r in sorted(routes):
        if r not in seen:
            seen.add(r)
            out.append(r)
    return out


def tree_summary(tsv, maxdepth=2, cap=120):
    rows = [l.split('\t') for l in lines(tsv)]
    if not rows:
        return ''
    root = rows[0][3]
    out = []
    for mode, user, group, p in rows[1:]:
        rel = os.path.relpath(p, root)
        d = rel.count('/') + 1
        if d > maxdepth:
            continue
        own = '' if (user, group) == ('root', 'root') else ' (%s:%s %s)' % (user, group, mode)
        out.append('%s- %s/%s' % ('  ' * (d - 1), os.path.basename(p), own if d == 1 else ''))
    more = ''
    if len(out) > cap:
        more = '\n… %d more entries' % (len(out) - cap)
        out = out[:cap]
    return '`%s` — %d directories captured (depth limit)\n%s%s' % (root, len(rows), '\n'.join(out), more)


def script_desc(path):
    desc = []
    for l in read(path).splitlines()[1:15]:
        s = l.strip()
        if s.startswith('#') and not re.match(r'#\s*(shellcheck|-\*-|vim:)', s) and len(s) > 2:
            desc.append(s.lstrip('#').strip())
        elif s and not s.startswith('#'):
            break
        if len(desc) >= 3:
            break
    return ' '.join(desc)[:200]


def context(state, backup_paths, remote, encrypted):
    S = os.path.join(state, 'snapshot')
    V = os.path.join(state, 'volatile')
    F = os.path.join(S, 'files')
    o = []
    w = o.append
    warnings = []
    host = read(os.path.join(S, 'meta/hostname')).strip()
    captured = read(os.path.join(V, 'captured-at')).strip()
    osr = dict(re.findall(r'^(\w+)="?(.*?)"?$', read(os.path.join(S, 'meta/os-release')), re.M))
    notes = parse_notes(read(os.path.join(state, 'NOTES.md')))
    used_notes = set()

    def note(key):
        used_notes.add(key)
        t = notes.get(key)
        return ('\n> ' + t.replace('\n', '\n> ') + '\n') if meaningful(t) else ''

    containers = load(os.path.join(S, 'docker/containers.json'), []) or []
    projects = load(os.path.join(S, 'docker/compose-projects.json'), []) or []
    networks = load(os.path.join(S, 'docker/networks.json'), []) or []
    volumes = load(os.path.join(S, 'docker/volumes.json'), []) or []
    ps = {}
    for l in lines(os.path.join(V, 'docker-ps.tsv')):
        p = l.split('\t')
        if len(p) >= 3:
            ps[p[0]] = (p[1], p[2])
    by_name = {c['name']: c for c in containers}

    w('# Server context: %s' % host)
    w('')
    w('_Auto-generated by **serverstate** from the live machine at **%s**. Regenerated after every change '
      '(watcher), daily (timer) and before each backup. Do not edit — edit NOTES.md via `sudo serverstate notes`._' % captured)
    w('')
    w('## How to use this document (for AI assistants)')
    w('- This is the ground truth for what runs on this server as of the timestamp above. If something matters, verify live (`docker ps`, `systemctl status`, `crontab -l`).')
    w('- Before proposing a change, check it against what is here: **ports in use, docker network names/subnets, bind-mount paths, volume names, cron schedules, firewall rules, disk space**.')
    w('- Keep changes reproducible so they are captured and can be rebuilt: docker services go in a compose file inside an existing stack dir (see Docker), scripts in `/usr/local/bin` or another captured script dir, services as systemd units in `/etc/systemd/system`, jobs via `crontab`. Avoid one-off `docker run` and ad-hoc edits outside these places.')
    w('- After making changes: `sudo serverstate snapshot` (also happens automatically within minutes), then record the *why* with `sudo serverstate notes`.')
    w('- Secrets are redacted (`***`). Real values live in the referenced files on the server (root-only).')
    w('- Full raw data: `%s/snapshot/` (configs, compose files, inspect JSON) — history: `sudo serverstate log`.' % state)
    w('')

    # ---------------- owner notes
    gen = ['Overview', 'Gotchas / do not touch', 'Planned changes / ideas']
    others = [k for k in notes if not re.match(r'^(Stack|Container|Service|Unit|Script|Instance): ', k) and k not in gen]
    body = []
    for k in gen + others:
        if meaningful(notes.get(k)):
            body.append('### %s\n%s\n' % (k, notes[k]))
        used_notes.add(k)
    w('## Owner notes')
    w('\n'.join(body) if body else '_No notes yet — run `sudo serverstate notes` to describe what this server is for._')
    w('')

    # ---------------- system
    w('## System')
    dmi = dict(l.split(': ', 1) for l in lines(os.path.join(S, 'meta/dmi')) if ': ' in l)
    lscpu = dict((k.strip(), v.strip()) for k, _, v in (l.partition(':') for l in lines(os.path.join(S, 'meta/lscpu'))))
    mem = read(os.path.join(S, 'meta/memory'))
    mt = re.search(r'MemTotal:\s+(\d+)', mem)
    sw = re.search(r'SwapTotal:\s+(\d+)', mem)
    rows = [
        ('Hostname', host),
        ('OS', osr.get('PRETTY_NAME', '')),
        ('Kernel', read(os.path.join(S, 'meta/kernel')).strip()),
        ('Hardware', ' '.join(x for x in (dmi.get('sys_vendor', ''), dmi.get('product_name', ''), dmi.get('board_name', '')) if x and x != 'None').strip()),
        ('Virtualization', read(os.path.join(S, 'meta/virt')).strip()),
        ('CPU', '%s — %s cores/threads' % (lscpu.get('Model name', '?'), lscpu.get('CPU(s)', '?'))),
        ('RAM', human(int(mt.group(1)) * 1024) if mt else '?'),
        ('Swap', human(int(sw.group(1)) * 1024) if sw else '?'),
        ('Timezone', read(os.path.join(S, 'meta/timezone')).strip()),
        ('Uptime', read(os.path.join(V, 'uptime')).strip()),
        ('Docker', '%s (compose %s), data-root %s' % (read(os.path.join(S, 'docker/version.txt')).strip() or 'not installed',
                                                      read(os.path.join(S, 'docker/compose-version.txt')).strip() or '-',
                                                      read(os.path.join(S, 'docker/root-dir.txt')).strip() or '-')),
    ]
    w(md_table(['', ''], rows))
    w('')

    # ---------------- storage
    w('## Storage')
    br = lsblk_rows(os.path.join(S, 'storage/lsblk.json'))
    if br:
        w('### Disks & partitions')
        w(md_table(['Device', 'Size', 'Type', 'Kind', 'FS', 'Label', 'Mounted at', 'Model', 'UUID'], br))
        w('')
    fstab = [l for l in read(os.path.join(F, 'etc/fstab')).splitlines() if l.strip() and not l.strip().startswith('#')]
    if fstab:
        w('### /etc/fstab')
        w(code('\n'.join(fstab)))
    df = read(os.path.join(V, 'df.txt'))
    if df:
        w('### Usage')
        w(code(df))
        for l in df.splitlines()[1:]:
            p = l.split()
            if len(p) >= 7 and p[5].endswith('%') and p[5][:-1].isdigit() and int(p[5][:-1]) >= 85:
                warnings.append('%s is %s full' % (p[6], p[5]))
    for f, title in (('mdadm-scan.txt', 'mdadm RAID'), ('lvm.txt', 'LVM'), ('zpool-list.txt', 'ZFS pools'),
                     ('zfs-datasets.txt', 'ZFS datasets'), ('btrfs-subvolumes.txt', 'btrfs subvolumes')):
        t = read(os.path.join(S, 'storage', f))
        if t.strip():
            w('### %s' % title)
            w(code(t, cap=60))
    for f in ('mdstat.txt', 'zpool-status.txt'):
        t = read(os.path.join(V, f))
        if re.search(r'DEGRADED|FAULTED|\[U*_+U*\]', t):
            warnings.append('storage array reports a problem (%s)' % f)
    smart = read(os.path.join(V, 'smart.txt'))
    if re.search(r'FAILED', smart):
        warnings.append('a disk reports SMART FAILED')
    trees = sorted(glob.glob(os.path.join(V, 'trees/*.tsv')))
    if trees:
        w('### Directory structure (data roots, top 2 levels)')
        for t in trees:
            s = tree_summary(t)
            if s:
                w(s)
                w('')
    w('')

    # ---------------- network
    w('## Network')
    for f, title in (('addresses.txt', 'Interfaces'), ('routes.txt', 'Routes'), ('dns.txt', 'DNS')):
        t = read(os.path.join(S, 'network', f))
        if t.strip():
            w('**%s**' % title)
            w(code(t, cap=40))
    ts = read(os.path.join(S, 'network/tailscale-ip.txt')).strip()
    if ts:
        w('**Tailscale IP:** %s' % ts)
    wg = read(os.path.join(S, 'network/wireguard-interfaces.txt')).strip()
    if wg:
        w('**WireGuard interfaces:** %s (configs in /etc/wireguard)' % wg)
    lst = []
    for l in lines(os.path.join(V, 'listening.txt')):
        p = l.split(None, 2)
        if len(p) >= 2:
            procs = ', '.join(sorted(set(re.findall(r'"([^"]+)"', p[2] if len(p) > 2 else ''))))
            addr, _, port = p[1].rpartition(':')
            lst.append((p[0], addr, port, procs))
    if lst:
        lst.sort(key=lambda r: (int(r[2]) if r[2].isdigit() else 99999, r[0]))
        w('### Listening ports (host)')
        w(md_table(['Proto', 'Address', 'Port', 'Process'], lst))
    w('')

    # ---------------- reverse proxy
    routes = proxy_routes(containers, F)
    if routes:
        w('## Domains / reverse-proxy routes')
        w(md_table(['Host', 'Target', 'Defined in'], routes[:200]))
        w('')

    # ---------------- docker
    if containers or projects:
        w('## Docker')
        w('%d containers (%d running), %d compose stacks, %d networks, %d named volumes.' % (
            len(containers), sum(1 for c in containers if ps.get(c['name'], ('',))[0] == 'running'),
            len(projects), len(networks), len(volumes)))
        w('')
        att = {}
        for c in containers:
            for n in c.get('networks', {}):
                att.setdefault(n, []).append(c['name'])
        if networks:
            w('### Networks')
            rows = []
            for n in networks:
                sub = ', '.join('%s gw %s' % (s.get('subnet', ''), s.get('gateway', '-')) for s in n.get('subnets', []))
                owner = (n.get('labels') or {}).get('com.docker.compose.project')
                rows.append((n['name'], n.get('driver'), sub, 'yes' if n.get('internal') else '',
                             'compose: ' + owner if owner else ('builtin' if n['name'] in ('bridge', 'host', 'none') else 'manual/external'),
                             ', '.join(sorted(att.get(n['name'], [])))))
            w(md_table(['Name', 'Driver', 'Subnet', 'Internal', 'Created by', 'Containers'], rows))
            w('')
        if volumes:
            sizes = dict(l.split('\t', 1) for l in lines(os.path.join(V, 'docker-volume-sizes.tsv')) if '\t' in l)
            users = {}
            for c in containers:
                for m in c.get('mounts', []):
                    if m.get('type') == 'volume' and m.get('name'):
                        users.setdefault(m['name'], []).append(c['name'])
            for sv in load(os.path.join(S, 'docker/services.json'), []) or []:
                for m in sv.get('mounts', []):
                    if m.get('type') == 'volume' and m.get('source'):
                        users.setdefault(m['source'], []).append('svc:' + sv['name'])
            rows = [(v['name'], v.get('driver'), sizes.get(v['name'], ''), ', '.join(users.get(v['name'], [])) or '(unused)',
                     v.get('mountpoint', '')) for v in volumes]
            w('### Named volumes')
            w(md_table(['Volume', 'Driver', 'Size', 'Used by', 'Mountpoint'], rows))
            dang = read(os.path.join(V, 'docker-dangling-volumes.txt')).strip()
            if dang and dang != '0':
                w('_%s dangling/anonymous volumes not shown (`docker volume ls -f dangling=true`)._' % dang)
            w('')

        def container_block(c):
            hc = c.get('host_config', {})
            st = ps.get(c['name'], ('?', ''))
            rp = (hc.get('RestartPolicy') or {}).get('Name') or 'no'
            b = ['- **%s** — `%s` · %s (%s) · restart: %s' % (c['name'], c.get('image'), st[0], st[1], rp)]
            if st[0] not in ('running', '?') and rp in ('always', 'unless-stopped'):
                warnings.append('container %s is %s' % (c['name'], st[0]))
            pl = port_list(hc)
            if pl:
                b.append('  - ports: ' + ', '.join(pl))
            nm = hc.get('NetworkMode', '')
            if nm in ('host', 'none') or nm.startswith('container:'):
                b.append('  - network_mode: ' + nm)
            elif c.get('networks'):
                b.append('  - networks: ' + ', '.join(
                    n + (' (%s)' % ', '.join(filter(None, [('ip ' + v['ipv4']) if v.get('ipv4') else '',
                                                          ('aliases ' + ','.join(a for a in v.get('aliases', []) if a != c['name'])) if [a for a in v.get('aliases', []) if a != c['name']] else ''])) if (v.get('ipv4') or [a for a in v.get('aliases', []) if a != c['name']]) else '')
                    for n, v in c['networks'].items()))
            ms = []
            for m in c.get('mounts', []):
                src = m.get('source') if m.get('type') == 'bind' else ('vol:' + m['name'] if m.get('name') else ('anon-volume' if m.get('anonymous') else m.get('type')))
                ms.append('%s → %s%s' % (src, m.get('destination'), ' (ro)' if m.get('ro') else ''))
            if ms:
                b.append('  - mounts: ' + '; '.join(ms))
            env = [redact_env(e) for e in c.get('env', []) if not re.match(r'^(PATH|HOME|LANG|LC_ALL|TERM|HOSTNAME|S6_|PYTHON|GPG_KEY|JAVA_HOME|NODE_VERSION|YARN_VERSION|GOLANG_VERSION|PHP_)', e)]
            if env:
                b.append('  - env: ' + ', '.join('`` %s ``' % e[:120] for e in env[:40]) + (' … (+%d)' % (len(env) - 40) if len(env) > 40 else ''))
            lab = ['%s=%s' % (k, v) for k, v in sorted(c.get('labels', {}).items()) if INTERESTING_LABEL.match(k)]
            if lab:
                b.append('  - labels: ' + ', '.join('`` %s ``' % l[:160] for l in lab[:25]))
            flags = []
            if hc.get('Privileged'):
                flags.append('privileged')
            if hc.get('CapAdd'):
                flags.append('cap_add ' + ','.join(hc['CapAdd']))
            if hc.get('Devices'):
                flags.append('devices ' + ','.join(d.get('PathOnHost', '') for d in hc['Devices']))
            if hc.get('DeviceRequests'):
                flags.append('GPU')
            if c.get('healthcheck') and c['healthcheck'].get('Test') not in (None, ['NONE']):
                flags.append('healthcheck')
            if c.get('user'):
                flags.append('user ' + c['user'])
            if flags:
                b.append('  - ' + ' · '.join(flags))
            return '\n'.join(b)

        w('### Compose stacks')
        for e in projects:
            w('#### Stack `%s`' % e['project'])
            w('- dir: `%s` · files: %s%s · source: %s' % (
                e.get('workdir'), ', '.join('`%s`' % f for f in e.get('config_files', [])),
                (' · env: ' + ', '.join('`%s`' % f for f in e['env_files'])) if e.get('env_files') else '', e.get('source')))
            if e.get('defined_services'):
                missing = [s for s in e['defined_services'] if s not in e.get('services', [])]
                w('- services defined: %s%s' % (', '.join(e['defined_services']),
                                                (' — **not created:** ' + ', '.join(missing)) if missing and e.get('containers') else ''))
            n = note('Stack: %s' % e['project'])
            if n:
                w(n)
            for cn in e.get('containers', []):
                if cn in by_name:
                    w(container_block(by_name[cn]))
            w('')
        svcs = load(os.path.join(S, 'docker/services.json'), []) or []
        if svcs:
            cur = dict((l.split('\t') + ['', ''])[:2] for l in lines(os.path.join(V, 'service-ls.tsv')))
            w('### Swarm services (%d) — Dokploy / docker stack' % len(svcs))
            stacks = lines(os.path.join(S, 'docker/stacks.txt'))
            if stacks:
                w('Stacks: ' + ', '.join(x.split('\t')[0] for x in stacks))
            rows = []
            for sv in svcs:
                r = cur.get(sv['name'], '')
                want_ = sv.get('replicas')
                if r and '/' in r and r.split('/')[0] != r.split('/')[1].split()[0]:
                    warnings.append('swarm service %s has %s replicas running' % (sv['name'], r))
                rows.append((sv['name'], sv.get('image'), r or want_,
                             ', '.join('%s→%s' % (x.get('published'), x.get('target')) for x in sv.get('ports', [])),
                             ', '.join(n.get('name', '') for n in sv.get('networks', [])),
                             '; '.join('%s → %s' % (m.get('source'), m.get('destination')) for m in sv.get('mounts', []))))
                note('Service: %s' % sv['name'])
            w(md_table(['Service', 'Image', 'Replicas', 'Ports', 'Networks', 'Mounts'], rows))
            for sv in svcs:
                n = note('Service: %s' % sv['name'])
                if n:
                    w('**%s**%s' % (sv['name'], n))
            w('')
        standalone = [c for c in containers if 'com.docker.compose.project' not in c.get('labels', {})]
        if standalone:
            w('### Standalone containers (not in compose — recreate scripts in `%s/snapshot/docker/standalone/`)' % state)
            for c in standalone:
                w(container_block(c))
                n = note('Container: %s' % c['name'])
                if n:
                    w(n)
            w('')
        w('')

    # ---------------- incus
    if os.path.isdir(os.path.join(S, 'incus')):
        w('## VMs & system containers (Incus)')
        lst = read(os.path.join(V, 'incus-list.txt'))
        if lst.strip():
            w(code(lst, cap=40))
        for f in sorted(glob.glob(os.path.join(S, 'incus/instances/*.yaml'))):
            name = os.path.basename(f)[:-5]
            cfg = read(f)
            typ = re.search(r'^\s*image\.type:\s*(\S+)', cfg, re.M)
            snaps = lines(f[:-5] + '.snapshots')
            limits = ', '.join(re.findall(r'^\s*(limits\.\w+:\s*\S+)', cfg, re.M))
            w('- **%s** %s %s — snapshots: %s' % (name, typ.group(1) if typ else '', ('(' + limits + ')') if limits else '',
                                                 ', '.join(snaps) or 'none'))
            n = note('Instance: %s' % name)
            if n:
                w(n)
        w('_Server-level Incus config (pools, networks, profiles) is in `snapshot/incus/preseed.yaml` — restorable with `incus admin init --preseed`. VM disks are data: back them up (e.g. `incus export`) or note them as rebuildable._')
        w('')

    # ---------------- monitoring
    km = load(os.path.join(S, 'monitoring/kuma-monitors.json'), {}) or {}
    ig = load(os.path.join(S, 'monitoring/integrations.json'), []) or []
    if km or ig:
        w('## Monitoring & alerting (existing — adopt, never recreate)')
        w('_Uptime Kuma monitors and the scripts that feed them. Anything that automates monitors must match these by push token / name and keep them; push tokens are masked here (full values in the snapshot)._')
        by_tok = {}
        for x in ig:
            by_tok.setdefault(x['token'], []).append(x)
        mons = km.get('monitors', [])
        if mons:
            rows = []
            for m in mons:
                tgt = m.get('url') or m.get('hostname') or m.get('docker_container') or ''
                if m.get('type') in ('dns', 'ping', 'port', 'tailscale-ping') or tgt in ('https://', 'http://'):
                    tgt = (m.get('hostname') or '') + (':%s' % m['port'] if m.get('port') and m.get('type') == 'port' else '')
                if m.get('type') == 'docker':
                    tgt = 'container ' + (m.get('docker_container') or '')
                if m.get('type') == 'push':
                    tgt = 'push ' + mask(m.get('push_token'))
                elif re.search(r'hc-ping|healthchecks', tgt):
                    tgt = re.sub(r'^(https?://[^/]+)/.*$', r'\1/… (key hidden)', tgt)
                if (m.get('last') or {}).get('msg') == '[object Object]':
                    warnings.append('Kuma monitor "%s" receives a malformed message ([object Object]) — its push URL sends status/msg twice' % m.get('name'))
                feeders = ', '.join(sorted(set(x['file'] + (' (stale copy)' if re.search(r'(\.bak|\.old|~|\.orig)$', x['file']) else '')
                                               for x in by_tok.get(m.get('push_token') or '-', []))))
                if m.get('type') == 'push' and m.get('active') and not feeders:
                    feeders = '⚠ no script on this server pushes to it'
                last = m.get('last') or {}
                rows.append((m.get('name'), m.get('type'), tgt, m.get('interval'), 'yes' if m.get('active') else 'paused',
                             m.get('group') or '', ', '.join(m.get('notifications', [])), feeders,
                             ('%s: %s' % (last.get('status'), redact_text(last.get('msg', ''))[:120])) if last else ''))
            w('Kuma DB: `%s` — %d monitors' % (km.get('db'), len(mons)))
            w(md_table(['Monitor', 'Type', 'Target', 'Every (s)', 'Active', 'Group', 'Notifications', 'Fed by', 'Last result'], rows))
            hc = [m for m in mons if 'hc-ping' in (m.get('url') or '') or 'healthchecks' in (m.get('url') or '')]
            if hc:
                w('**Watchdog:** ' + ', '.join('%s → %s' % (m['name'], re.sub(r'^(https?://[^/]+)/.*$', r'\1/…', m.get('url', ''))) for m in hc) +
                  ' — Kuma pings healthchecks.io, so healthchecks.io alerts if Kuma (or this server) dies.')
        known = set(m.get('push_token') for m in mons)
        orphans = [x for x in ig if x['kind'] == 'uptime-kuma-push' and mons and x['token'] not in known]
        rows = [(x['file'], x['kind'], mask(x['token']), '; '.join(x.get('called_by', []))[:200] or '(not scheduled by cron/systemd — called by another script?)') for x in ig]
        if rows:
            w('')
            w('**Scripts that report out:**')
            w(md_table(['File', 'Reports to', 'Token', 'Scheduled by'], rows))
        for x in ig:
            if re.search(r'(\.bak|\.old|~|\.orig)$', x['file']):
                warnings.append('%s is a stale copy that still contains a Kuma push token — candidate for cleanup' % x['file'])
        for x in orphans:
            warnings.append('%s pushes to Kuma token %s that matches no monitor' % (x['file'], mask(x['token'])))
        w('')

    # ---------------- firewall
    w('## Firewall & access control')
    mg = lines(os.path.join(S, 'firewall/managers.txt'))
    w('Active: %s' % (', '.join(mg) if mg else 'no firewall manager detected (raw iptables / docker only)'))
    w('')
    t = read(os.path.join(S, 'firewall/ufw-status.txt'))
    if t.strip():
        w('### ufw')
        w(code(t, cap=120))
    ipt = read(os.path.join(S, 'firewall/iptables.rules'))
    if ipt.strip():
        filt, cur = [], None
        for l in ipt.splitlines():
            if l.startswith('*'):
                cur = l[1:]
            if cur == 'filter' and (l.startswith('-A') or (l.startswith(':') and not l.endswith('ACCEPT'))):
                if not re.match(r'^-A (ufw6?|f2b)-', l):
                    filt.append(l)
        if filt:
            w('### iptables (filter table, docker-generated and ufw/fail2ban internals removed)')
            w(code('\n'.join(filt), cap=150))
        if 'DOCKER-USER' in ipt and re.search(r'^-A DOCKER-USER (?!-j RETURN$)', ipt, re.M):
            w('_Custom DOCKER-USER rules exist — these filter traffic to published container ports._')
    if 'nftables' in mg:
        t = read(os.path.join(F, 'etc/nftables.conf'))
        if t.strip():
            w('### /etc/nftables.conf')
            w(code(t, cap=150))
    j = lines(os.path.join(S, 'firewall/fail2ban-jails.txt'))
    if j:
        w('**fail2ban jails:** ' + ', '.join(j))
    t = lines(os.path.join(S, 'firewall/crowdsec-collections.txt'))
    if t:
        w('**CrowdSec collections:** ' + ', '.join(x.split(',')[0] for x in t if not x.startswith('name')))
    t = lines(os.path.join(S, 'firewall/ipset-names.txt'))
    if t:
        w('**ipsets:** ' + ', '.join(t))
    sshd = {}
    for f in sorted(glob.glob(os.path.join(F, 'etc/ssh/sshd_config.d/*.conf'))) + [os.path.join(F, 'etc/ssh/sshd_config')]:
        for l in read(f).splitlines():
            m = re.match(r'^\s*(Port|PermitRootLogin|PasswordAuthentication|PubkeyAuthentication|AllowUsers|AllowGroups|ListenAddress|KbdInteractiveAuthentication)\s+(.+)$', l, re.I)
            if m and m.group(1).lower() not in sshd:
                sshd[m.group(1).lower()] = m.group(2).strip()
    if sshd:
        w('**sshd:** ' + ', '.join('%s=%s' % kv for kv in sorted(sshd.items())))
        if sshd.get('passwordauthentication', 'yes').lower() == 'yes':
            warnings.append('SSH password authentication is enabled')
    w('_Not captured here: router/NAT port forwards, Cloudflare (DNS, WAF, Access), VPS-provider firewalls._')
    w('')

    # ---------------- scheduled jobs
    w('## Scheduled jobs')
    cj = all_cron(snap=S)
    if cj:
        w('### cron')
        w(md_table(['Source', 'Schedule', 'User', 'Command'], [(s, sc, u, redact_text(c)[:200]) for s, sc, u, c in cj]))
        w('')
    rp = []
    for d in ('hourly', 'daily', 'weekly', 'monthly'):
        fs = sorted(os.path.basename(p) for p in glob.glob(os.path.join(F, 'etc/cron.%s/*' % d)) if not p.endswith('.placeholder'))
        if fs:
            rp.append('- cron.%s: %s' % (d, ', '.join(fs)))
    if rp:
        w('**run-parts directories:**')
        w('\n'.join(rp))
        w('')
    timers = lines(os.path.join(S, 'cron/enabled-timers.txt'))
    if timers:
        rows = []
        for t in timers:
            uf = os.path.join(F, 'etc/systemd/system', t)
            inf = unit_info(uf) if os.path.exists(uf) else {}
            svc = os.path.join(F, 'etc/systemd/system', t.replace('.timer', '.service'))
            sinf = unit_info(svc) if os.path.exists(svc) else {}
            rows.append((t, 'custom' if inf else 'package', ' '.join(inf.get('OnCalendar', []) + inf.get('OnBootSec', []) + inf.get('OnUnitActiveSec', [])),
                         redact_text(' ; '.join(sinf.get('ExecStart', [])))[:160]))
        sched = read(os.path.join(V, 'systemd-timers-schedule.txt'))
        w('### systemd timers (enabled)')
        w(md_table(['Timer', 'Origin', 'Schedule', 'Runs'], rows))
        if sched:
            w('<details><summary>next/last run times</summary>\n\n' + code(sched, cap=60) + '\n</details>')
        w('')

    # ---------------- systemd
    w('## Services (systemd)')
    cu = [u for u in lines(os.path.join(S, 'systemd/custom-units.txt'))]
    enabled = set(lines(os.path.join(S, 'systemd/enabled-units.txt')))
    running = set(l.split()[0] for l in lines(os.path.join(V, 'running-services.txt')))
    if cu:
        w('### Custom units (/etc/systemd/system)')
        for u in cu:
            base = os.path.basename(u)
            inf = unit_info(os.path.join(F, u.lstrip('/')))
            if u.endswith('.conf'):
                w('- drop-in `%s`' % u)
                continue
            state_ = ('enabled' if base in enabled else 'disabled') + (', running' if base in running else '')
            w('- **%s** (%s) — %s' % (base, state_, '; '.join(inf.get('Description', [])) or ''))
            for k in ('ExecStart', 'User', 'WorkingDirectory', 'OnCalendar', 'What', 'Where'):
                if inf.get(k):
                    w('  - %s: `%s`' % (k, redact_text(' ; '.join(inf[k]))[:200]))
            n = note('Unit: %s' % base)
            if n:
                w(n)
        w('')
    other = sorted(u for u in enabled if u.endswith('.service') and not any(u == os.path.basename(x) for x in cu))
    if other:
        w('**Other enabled services:** ' + ', '.join(other))
        w('')
    failed = lines(os.path.join(V, 'failed-units.txt'))
    if failed:
        w('**Failed units:** ' + ', '.join(l.split()[0] for l in failed))
        for l in failed:
            warnings.append('systemd unit failed: ' + l.split()[0])
        w('')

    # ---------------- scripts
    idx = lines(os.path.join(S, 'scripts/index.txt'))
    if idx:
        w('## Scripts')
        callers = {}
        for s, sc, u, c in cj:
            for p in idx:
                if p in c:
                    callers.setdefault(p, []).append('cron %s' % sc)
        for u in cu:
            t = read(os.path.join(F, u.lstrip('/')))
            for p in idx:
                if p in t:
                    callers.setdefault(p, []).append(os.path.basename(u))
        rows = []
        for p in idx[:150]:
            fp = os.path.join(F, p.lstrip('/'))
            txt = read(fp)
            tags = []
            if re.search(r'\b(rclone|restic|borg|kopia|duplicity|rsync)\b', txt):
                tags.append('backup/sync')
            if re.search(r'\bdocker\b', txt):
                tags.append('docker')
            rows.append((p, script_desc(fp), ', '.join(callers.get(p, [])), ', '.join(tags), len(txt.splitlines())))
            n = notes.get('Script: %s' % p)
            used_notes.add('Script: %s' % p)
            if meaningful(n):
                rows[-1] = (p, n.replace('\n', ' ')[:300], rows[-1][2], rows[-1][3], rows[-1][4])
        w(md_table(['Script', 'Description', 'Called by', 'Tags', 'Lines'], rows))
        if len(idx) > 150:
            w('… %d more in `%s/snapshot/scripts/index.txt`' % (len(idx) - 150, state))
        w('')

    # ---------------- packages
    w('## Installed software')
    am = lines(os.path.join(S, 'packages/apt-manual.txt'))
    if am:
        w('**apt (manually installed, %d):** %s' % (len(am), ', '.join(am)))
        w('')
    for f, title in (('snap.txt', 'snaps'), ('flatpak.txt', 'flatpaks'), ('pipx.txt', 'pipx'), ('npm-global.txt', 'npm -g'),
                     ('dnf-userinstalled.txt', 'dnf (user-installed)')):
        t = lines(os.path.join(S, 'packages', f))
        if t:
            w('**%s:** %s' % (title, ', '.join(x.split('\t')[0].split()[0] for x in t)))
    ul = [l.split('\t') for l in lines(os.path.join(S, 'packages/usr-local-bin.txt'))]
    bins = [os.path.basename(p[0]) for p in ul if len(p) > 1 and not re.search(r'script|text|symbolic', p[1])]
    if bins:
        w('**Manually installed binaries in /usr/local/(s)bin:** ' + ', '.join(bins))
    opt = lines(os.path.join(S, 'packages/opt-listing.txt'))
    if opt:
        w('**/opt:** ' + ', '.join(opt))
    w('')

    # ---------------- users
    w('## Users & access')
    rows = []
    groups = [l.split(':') for l in lines(os.path.join(S, 'users/groups.group'))]
    for l in lines(os.path.join(S, 'users/users.passwd')):
        p = l.split(':')
        if len(p) < 7:
            continue
        g = [x[0] for x in groups if len(x) > 3 and p[0] in x[3].split(',')]
        ak = read(os.path.join(F, p[5].lstrip('/'), '.ssh/authorized_keys'))
        keys = [k.split()[-1] if len(k.split()) > 2 else '(key)' for k in ak.splitlines() if k.strip() and not k.startswith('#')]
        rows.append((p[0], p[2], p[5], p[6], ', '.join(g), '%d: %s' % (len(keys), ', '.join(keys)) if keys else ''))
    w(md_table(['User', 'UID', 'Home', 'Shell', 'Groups', 'SSH keys'], rows))
    w('')

    # ---------------- backup
    w('## Backups & rebuild')
    bj = [(s, sc, u, c) for s, sc, u, c in cj if re.search(r'backup|restic|rclone|borg|kopia|duplicati|r2', c, re.I)]
    if bj:
        w('**Backup-related jobs:**')
        for s, sc, u, c in bj:
            w('- `%s` (%s) — `%s`' % (sc, s, redact_text(c)[:200]))
    w('- serverstate bundle → `%s` (encrypted: %s)' % (remote or 'not configured (RCLONE_REMOTE)', encrypted))
    bp = backup_paths.split()
    w('- BACKUP_PATHS (declared in /etc/serverstate.conf): %s' % (', '.join('`%s`' % b for b in bp) if bp else '_not set — coverage unknown_'))
    locs = data_locations(S)
    if locs:
        rows = []
        for p, what, ro in locs:
            c = covered(p, bp)
            rows.append((p, what, '?' if c is None else ('intentional' if c == 'intentional' else ('✓' if c else ('— (read-only view)' if ro else '✗')))))
            if c is False and not ro and not os.path.exists(os.path.join(state, 'audit.md')):
                warnings.append('data location not covered by backup: %s (%s)' % (p, what))
        w('')
        w('### Where data lives (must be in the data backup)')
        w(md_table(['Path', 'What', 'In backup'], rows[:200]))
    w('')
    w('**Rebuild on new hardware:** `rclone cat %s/bootstrap.sh | sudo bash -s -- %s` (after installing rclone and configuring the R2 remote as root), '
      'or `sudo serverstate pull` + `sudo serverstate restore --bundle <dir>`. Phases: `serverstate restore --list`.' % (remote or '<remote>', remote or '<remote>'))
    w('')
    w('**Not captured by serverstate:** application data itself (comes from the data backup), docker images (re-pulled), '
      'Tailscale node identity (re-auth), DNS/Cloudflare settings, router config, BIOS/RAID-controller settings, '
      'files larger than MAX_FILE_KB, anything outside the captured paths.')
    w('')

    # ---------------- audit
    au = read(os.path.join(state, 'audit.md'))
    if au.strip():
        w('## Coverage & reboot audit')
        w(re.sub(r'^#', '##', au.split('\n', 1)[1] if '\n' in au else '', flags=re.M).strip())
        for l in au.splitlines():
            if l.startswith('- ✗'):
                warnings.append('audit: ' + re.sub(r'\*\*', '', l[4:])[:160])
        w('')

    # ---------------- recent changes
    cl = read(os.path.join(V, 'changelog.txt'))
    if cl.strip():
        w('## Recent changes (newest first)')
        w(code(cl, cap=160))
        w('')

    # ---------------- leftover notes
    left = [k for k in notes if k not in used_notes and meaningful(notes[k])]
    if left:
        w('## Notes about things no longer present')
        for k in left:
            w('### %s\n%s\n' % (k, notes[k]))

    # warnings go near the top
    if warnings:
        idx_ = o.index('## Owner notes')
        o[idx_:idx_] = ['## ⚠ Attention', '\n'.join('- ⚠ ' + x for x in sorted(set(warnings))), '']
    sys.stdout.write('\n'.join(o).rstrip() + '\n')


# --------------------------------------------------------------------------- swarm services
def norm_services(data, networks_f):
    ids = {n.get('id'): n['name'] for n in (load(networks_f, []) or []) if n.get('id')}
    out = []
    for sv in data or []:
        sp = sv.get('Spec') or {}
        tt = sp.get('TaskTemplate') or {}
        cs = tt.get('ContainerSpec') or {}
        img = cs.get('Image') or ''
        mode = sp.get('Mode') or {}
        if 'Replicated' in mode:
            replicas = (mode.get('Replicated') or {}).get('Replicas')
        elif 'Global' in mode:
            replicas = 'global'
        else:
            replicas = None
        nets = []
        for n in (tt.get('Networks') or sp.get('Networks') or []):
            nets.append(strip_empty({'name': ids.get(n.get('Target'), n.get('Target')), 'aliases': n.get('Aliases')}))
        out.append(strip_empty({
            'name': sp.get('Name'), 'image': img.split('@')[0], 'image_digest': img.split('@')[1] if '@' in img else None,
            'replicas': replicas, 'env': sorted(cs.get('Env') or []), 'labels': sp.get('Labels'),
            'container_labels': cs.get('Labels'), 'command': cs.get('Command'), 'args': cs.get('Args'),
            'hostname': cs.get('Hostname'),
            'mounts': [strip_empty({'type': m.get('Type'), 'source': m.get('Source'), 'destination': m.get('Target'),
                                    'ro': m.get('ReadOnly')}) for m in (cs.get('Mounts') or [])],
            'ports': [strip_empty({'published': x.get('PublishedPort'), 'target': x.get('TargetPort'),
                                   'protocol': x.get('Protocol'), 'mode': x.get('PublishMode')})
                      for x in ((sp.get('EndpointSpec') or {}).get('Ports') or [])],
            'networks': nets, 'constraints': (tt.get('Placement') or {}).get('Constraints'),
            'restart': (tt.get('RestartPolicy') or {}).get('Condition'),
            'secrets': [x.get('SecretName') for x in (cs.get('Secrets') or [])],
            'configs': [x.get('ConfigName') for x in (cs.get('Configs') or [])]}))
    return sorted(out, key=lambda x: x.get('name', ''))


# --------------------------------------------------------------------------- monitoring integrations
PUSH_RX = re.compile(r'(https?://[^\s"\'/]+)/api/push/([A-Za-z0-9]{6,})')
HC_RX = re.compile(r'(https?://(?:hc-ping\.com|[^\s"\'/]*healthchecks[^\s"\'/]*))/([0-9a-fA-F-]{20,}|[^\s"\'?]+/[^\s"\'?]+)')


def integrations(files_root):
    """Find every place on disk that reports to Uptime Kuma (push) or healthchecks.io."""
    found = []
    cron_text = []
    for pat in ['etc/crontab', 'etc/cron.d/*', 'var/spool/cron/crontabs/*', 'etc/systemd/system/*.service']:
        for f in glob.glob(os.path.join(files_root, pat)):
            if os.path.isfile(f):
                cron_text.append(('/' + os.path.relpath(f, files_root), read(f)))
    for dirpath, _, fnames in os.walk(files_root):
        for fn in fnames:
            fp = os.path.join(dirpath, fn)
            if os.path.islink(fp) or is_binary(fp):
                continue
            try:
                if os.path.getsize(fp) > 512 * 1024:
                    continue
            except OSError:
                continue
            txt = read(fp)
            if '/api/push/' not in txt and 'hc-ping' not in txt and 'healthchecks' not in txt:
                continue
            real = '/' + os.path.relpath(fp, files_root)
            callers = []
            for src, t in cron_text:
                for line in t.splitlines():
                    if real in line and not line.lstrip().startswith('#'):
                        callers.append('%s: %s' % (src, line.strip()[:160]))
            for m in PUSH_RX.finditer(txt):
                found.append({'file': real, 'kind': 'uptime-kuma-push', 'base': m.group(1), 'token': m.group(2), 'called_by': callers})
            for m in HC_RX.finditer(txt):
                found.append({'file': real, 'kind': 'healthchecks', 'base': m.group(1), 'token': m.group(2), 'called_by': callers})
    seen, out = set(), []
    for f in found:
        k = (f['file'], f['kind'], f['token'])
        if k not in seen:
            seen.add(k)
            out.append(f)
    dump(sorted(out, key=lambda x: (x['file'], x['token'])))


def kuma_inventory(containers_f):
    """Read Uptime Kuma's monitors from its SQLite DB, strictly read-only."""
    import sqlite3
    db = None
    for c in load(containers_f, []) or []:
        if 'uptime-kuma' in (c.get('image') or ''):
            for m in c.get('mounts', []):
                if m.get('destination') == '/app/data' and m.get('source'):
                    cand = os.path.join(m['source'], 'kuma.db')
                    if os.path.exists(cand):
                        db = cand
    if not db:
        return
    con = None
    for uri in ('file:%s?mode=ro' % db, 'file:%s?mode=ro&immutable=1' % db):
        try:
            con = sqlite3.connect(uri, uri=True, timeout=5)
            con.execute('select 1 from monitor limit 1')
            break
        except Exception:
            con = None
    if con is None:
        sys.stderr.write('cannot open %s read-only\n' % db)
        return
    con.row_factory = sqlite3.Row
    cols = [r[1] for r in con.execute('pragma table_info(monitor)')]
    want = [c for c in ('id', 'name', 'type', 'active', 'interval', 'url', 'hostname', 'port', 'keyword', 'push_token',
                        'docker_container', 'docker_host', 'parent', 'description', 'maxretries', 'retry_interval',
                        'upside_down', 'method') if c in cols]
    mons = [dict(r) for r in con.execute('select %s from monitor order by id' % ', '.join(want))]
    notif = {}
    try:
        for r in con.execute('select id, name, config from notification'):
            t = ''
            try:
                t = json.loads(r['config'] or '{}').get('type', '')
            except Exception:
                pass
            notif[r['id']] = '%s (%s)' % (r['name'], t) if t else r['name']
        links = {}
        for r in con.execute('select monitor_id, notification_id from monitor_notification'):
            links.setdefault(r['monitor_id'], []).append(notif.get(r['notification_id'], str(r['notification_id'])))
    except Exception:
        links = {}
    tags = {}
    try:
        for r in con.execute('select mt.monitor_id, t.name, mt.value from monitor_tag mt join tag t on t.id = mt.tag_id'):
            tags.setdefault(r['monitor_id'], []).append(r['name'] + (':' + r['value'] if r['value'] else ''))
    except Exception:
        pass
    names = {m['id']: m['name'] for m in mons}
    for m in mons:
        m['notifications'] = sorted(links.get(m['id'], []))
        m['tags'] = sorted(tags.get(m['id'], []))
        if m.get('parent'):
            m['group'] = names.get(m['parent'])
        try:
            r = con.execute('select status, msg, time from heartbeat where monitor_id=? order by time desc limit 1', (m['id'],)).fetchone()
            if r:
                m['last'] = {'status': {0: 'down', 1: 'up', 2: 'pending', 3: 'maintenance'}.get(r['status'], r['status']),
                             'msg': (r['msg'] or '')[:300], 'time': r['time']}
        except Exception:
            pass
    dump({'db': db, 'monitors': [strip_empty(m) for m in mons]})


def mask(tok):
    return ('…' + tok[-4:]) if tok and len(tok) > 6 else (tok or '')


# --------------------------------------------------------------------------- coverage + reboot audit
REGENERABLE = ('/var/lib/docker/overlay2', '/var/lib/docker/image', '/var/lib/docker/buildkit', '/var/lib/docker/containers',
               '/var/lib/docker/tmp', '/var/lib/containerd', '/var/lib/apt', '/var/lib/snapd', '/var/cache', '/var/lib/dpkg',
               '/var/lib/docker/network', '/var/lib/docker/swarm', '/var/lib/docker/plugins', '/var/lib/systemd',
               '/var/lib/ubuntu-advantage', '/var/lib/command-not-found', '/var/lib/man-db', '/var/lib/fwupd')
HANDLED_ETC = ('/etc/cron', '/etc/crontab', '/etc/anacrontab', '/etc/systemd/system', '/etc/apt/', '/etc/passwd', '/etc/group',
               '/etc/shadow', '/etc/gshadow', '/etc/subuid', '/etc/subgid', '/etc/fstab', '/etc/hostname', '/etc/timezone',
               '/etc/localtime', '/etc/netplan', '/etc/serverstate.conf')
GENERATED_ETC = ('/etc/machine-id', '/etc/hosts', '/etc/resolv.conf', '/etc/mtab', '/etc/adjtime', '/etc/ld.so.cache',
                 '/etc/.pwd.lock', '/etc/ssl/certs', '/etc/apparmor.d/cache', '/etc/alternatives', '/etc/ssl/private/ssl-cert-snakeoil.key',
                 '/etc/.updated', '/etc/cloud/', '/etc/udev/hwdb.bin', '/etc/subuid-', '/etc/subgid-', '/etc/passwd-', '/etc/group-',
                 '/etc/shadow-', '/etc/gshadow-', '/etc/ca-certificates.conf', '/etc/docker/key.json', '/etc/dokploy/applications',
                 '/etc/dokploy/logs', '/etc/dokploy/compose', '/etc/dokploy/volume-backups', '/etc/initramfs-tools/conf.d/resume')


def parse_size(s):
    m = re.match(r'([\d.]+)\s*([kKMGT]?B)', s or '')
    if not m:
        return 0
    return float(m.group(1)) * {'B': 1, 'kB': 1e3, 'KB': 1e3, 'MB': 1e6, 'GB': 1e9, 'TB': 1e12}.get(m.group(2), 1)


def under(path, prefixes):
    return any(path == p.rstrip('/') or path.startswith(p.rstrip('/') + '/') for p in prefixes if p)


def ipt_rules(text):
    out = set()
    cur = None
    for l in text.splitlines():
        if l.startswith('*'):
            cur = l[1:]
        if not l.startswith('-A') or ('DOCKER' in l and 'DOCKER-USER' not in l) or re.search(r'docker0|br-[0-9a-f]+', l) \
                or re.match(r'^-A f2b-\S+ -s ', l):
            continue
        out.add('%s %s' % (cur, l.strip()))
    return out


def audit(state, backup, tmp, restore_etc):
    S = os.path.join(state, 'snapshot')
    V = os.path.join(state, 'volatile')
    F = os.path.join(S, 'files')
    bp = backup.split()
    re_etc = restore_etc.split()
    host = read(os.path.join(S, 'meta/hostname')).strip()
    o = []
    w = o.append
    w('# Coverage & reboot audit — %s' % host)
    w('_Generated %s. Read-only findings. ✗ = likely gap, review = decide. Fix by editing /etc/serverstate.conf '
      '(BACKUP_PATHS, EXTRA_CAPTURE, RESTORE_ETC_PATHS), or mark it intentional in NOTES.md._' % datetime.datetime.now().strftime('%Y-%m-%d %H:%M'))
    w('')

    # 1. reboot survival
    w('## Would it survive a reboot?')
    rb = []
    en_units = set(lines(os.path.join(S, 'systemd/enabled-units.txt')))
    ps = {}
    for l in lines(os.path.join(V, 'docker-ps.tsv')):
        p = l.split('\t')
        if len(p) >= 2:
            ps[p[0]] = p[1]
    for c in load(os.path.join(S, 'docker/containers.json'), []) or []:
        rp = ((c.get('host_config') or {}).get('RestartPolicy') or {}).get('Name') or 'no'
        if ps.get(c['name']) == 'running' and rp in ('no', ''):
            rb.append('✗ container **%s** is running with restart policy "no" — it will NOT come back after a reboot' % c['name'])
    states = dict(l.split(None, 1) for l in lines(os.path.join(S, 'systemd/unit-file-states.txt')) if ' ' in l)
    trig = dict(l.split(None, 1) for l in lines(os.path.join(S, 'systemd/triggered-by.txt')) if ' ' in l)
    for l in lines(os.path.join(V, 'running-services.txt')):
        u = l.split()[0]
        st = states.get(u, '').strip()
        if st == 'disabled' and u in trig:
            continue  # socket/timer/path activated (e.g. ssh.socket on Ubuntu 24.04+)
        if st == 'disabled':
            rb.append('✗ service **%s** is running but disabled — started by hand? It will not start on boot (unless something else pulls it in)' % u)
    fstab_t = set()
    for l in read(os.path.join(F, 'etc/fstab')).splitlines():
        p = l.split()
        if len(p) >= 2 and not l.strip().startswith('#'):
            fstab_t.add(p[1])
    for l in lines(os.path.join(S, 'storage/mounts.txt')):
        p = l.split()
        if len(p) >= 3 and p[0] not in fstab_t and p[0] != '/' and p[2] not in ('zfs', 'fuse.rclone', 'nfsd') \
                and p[1].startswith('/dev/') and not p[0].startswith(('/boot', '/snap', '/run', '/mnt/sandbox')):
            if p[1].startswith('/dev/loop'):
                owner = 'incus-startup.service' if p[0].startswith('/var/lib/incus') else ('lxd' if 'lxd' in p[0] else None)
                if owner and (owner in en_units or owner == 'lxd'):
                    rb.append('✓ %s (loop image) is mounted by %s at boot' % (p[0], owner))
                else:
                    rb.append('review **%s** is a loop-device mount (%s) — make sure the service that mounts it is enabled' % (p[0], p[2]))
                continue
            rb.append('✗ **%s** (%s, %s) is mounted but not in /etc/fstab — it will not be mounted after a reboot' % (p[0], p[1], p[2]))
    live = ipt_rules(read(os.path.join(V, 'iptables.raw')))
    saved_txt = read(os.path.join(F, 'etc/iptables/rules.v4'))
    mg = lines(os.path.join(S, 'firewall/managers.txt'))
    if live and saved_txt:
        saved = ipt_rules(saved_txt)
        miss = sorted(live - saved)
        miss = [m for m in miss if not re.match(r'^\w+ -A (ufw6?|f2b|cali|KUBE|ts-)', m)]
        if miss:
            rb.append('✗ %d live iptables rule(s) are NOT in /etc/iptables/rules.v4 — lost on reboot (run `netfilter-persistent save` if they should stay):\n```\n%s\n```' % (len(miss), '\n'.join(miss[:30])))
        else:
            rb.append('✓ live iptables rules match /etc/iptables/rules.v4')
    elif live and not saved_txt and not (set(mg) & {'ufw', 'nftables', 'firewalld'}):
        custom = [r for r in live if re.search(r' -A (INPUT|DOCKER-USER|FORWARD) ', r) and 'DOCKER-USER -j RETURN' not in r and 'ufw' not in r]
        appliers = []
        for u in lines(os.path.join(S, 'systemd/custom-units.txt')):
            base = os.path.basename(u)
            txt = read(os.path.join(F, u.lstrip('/')))
            for ex in re.findall(r'^\s*Exec\w*=\s*[-@+!]*(\S+)', txt, re.M):
                txt += read(os.path.join(F, ex.lstrip('/')))
            if base in en_units and re.search(r'\b(iptables|ip6tables|nft)\b', txt):
                appliers.append(base)
        if custom and appliers:
            rb.append('✓ %d custom iptables rules are re-applied at boot by %s (verify its rule files match: they are restored with it)' % (len(custom), ', '.join(sorted(set(appliers)))))
        elif custom:
            rb.append('✗ %d custom iptables rules are live but nothing saves them (no iptables-persistent/ufw/nftables) — gone after reboot' % len(custom))
    en = set(lines(os.path.join(S, 'systemd/enabled-units.txt')))
    if os.path.exists(os.path.join(S, 'docker/version.txt')) and 'docker.service' not in en and 'docker.socket' not in en:
        rb.append('✗ docker.service is not enabled — no containers start after a reboot')
    for t in ('serverstate-snapshot.timer',):
        pass
    w('\n'.join('- ' + x for x in rb) if rb else '- ✓ nothing found that would be lost on reboot')
    w('')

    # 2. big data not in the backup
    w('## Large directories not covered by BACKUP_PATHS')
    if not bp:
        w('_BACKUP_PATHS is not set, so coverage cannot be judged. Biggest directories:_')
    du = []
    for l in lines(os.path.join(tmp, 'du.tsv')):
        p = l.split('\t', 1)
        if len(p) == 2 and p[0].isdigit():
            du.append((int(p[0]), p[1]))
    paths = set(x[1] for x in du)
    rows = []
    for size, path in sorted(du, reverse=True):
        if size < 200 * 1024 * 1024:
            continue
        if any(q.startswith(path.rstrip('/') + '/') for q in paths):  # has measured children -> report children
            continue
        if bp and under(path, bp):
            continue
        if under(path, intentional()):
            tag = 'intentional (NOT_BACKED_UP_OK)'
        elif re.search(r'/backups?/\d{4}-\d\d-\d\d', path):
            tag = 'local backup copies (their contents go off-site via your backup job?)'
        else:
            tag = 'regenerable (images/packages/cache)' if under(path, REGENERABLE) else ('✗ not backed up' if bp else '')
        if path.startswith('/var/lib/docker/volumes'):
            tag = '✗ docker volumes — app data!' if not (bp and under(path, bp)) else tag
        rows.append((human(size), path, tag))
    w(md_table(['Size', 'Path', 'Note'], rows[:60]) if rows else '- ✓ nothing over 200 MB outside the backup')
    w('')

    # 3. docker specifics
    w('## Docker data that a rebuild could lose')
    dk = []
    for p, what, ro in data_locations(S):
        c = covered(p, bp)
        if c is False and not ro:
            dbish = re.search(r':?/(var/lib/(postgresql|mysql)|data/db|pgdata)|postgres', what + ' ' + p)
            dk.append('%s %s — %s (not in BACKUP_PATHS)%s' % ('review' if dbish else '✗', p, what,
                      ' — a database: fine if your backup dumps it (pg_dumpall / mysqldump)' if dbish else ''))
    for l in lines(os.path.join(tmp, 'docker-size.tsv')):
        n, _, sz = l.partition('\t')
        if parse_size(sz) > 100e6:
            dk.append('✗ container **%s** has %s written inside the container itself (not in a volume) — lost when it is recreated' % (n, sz.split(' (')[0]))
    for l in lines(os.path.join(tmp, 'images.tsv')):
        p = l.split('\t')
        if len(p) >= 3 and p[0] != '<none>' and p[2] in ('<none>', ''):
            dk.append('review image **%s:%s** has no registry digest (built locally?) — it cannot be re-pulled; keep its Dockerfile/build context captured or push it to a registry' % (p[0], p[1]))
    svcs = load(os.path.join(S, 'docker/services.json'), []) or []
    if svcs:
        dk.append('review %d swarm services (Dokploy) — restore needs Dokploy + its Postgres data back before it can redeploy them; '
                  'images Dokploy built from git show "no registry digest" — Dokploy rebuilds them on redeploy' % len(svcs))
    w('\n'.join('- ' + x for x in dk) if dk else '- ✓ nothing found')
    w('')

    # 4. /etc: custom + changed files that restore would not put back
    owned = set()
    for f in glob.glob('/var/lib/dpkg/info/*.list'):
        for l in read(f).splitlines():
            owned.add(l)
    w('## Configuration the restore would NOT put back automatically')
    w('_Everything in /etc is captured in the snapshot; these files are custom or modified and are not covered by RESTORE_ETC_PATHS or another restore phase. Add the ones that matter to RESTORE_ETC_PATHS._')
    GEN_RX = re.compile(r'^/etc/(apparmor\.d/(local|tunables)/|console-setup/|xml/|lvm/(archive|backup)/|pam\.d/common-|multipath/|'
                        r'iscsi/initiatorname|security/opasswd|pollinate/|python3/debian_config|network/fan|networks$|shells$|'
                        r'udev/rules\.d/.*ubuntu--vg|tmpfiles\.d/screen-cleanup|locale\.(gen|conf)$|default/(keyboard|console-setup)$)|\.ucf-(dist|old|new)$|\.old$')
    try:
        inst = float(read(os.path.join(tmp, 'install-time')).strip() or 0)
    except ValueError:
        inst = 0
    refd = referenced_etc(F)
    cust = []
    if owned:
        for dp, dns, fns in os.walk('/etc'):
            dns[:] = [d for d in dns if not under(os.path.join(dp, d), GENERATED_ETC)]
            for fn in fns:
                fp = os.path.join(dp, fn)
                if fp in owned or os.path.islink(fp) or under(fp, GENERATED_ETC) or under(fp, HANDLED_ETC) or under(fp, re_etc) \
                        or under(fp, refd) or GEN_RX.search(fp) or fp.endswith(('.pre-serverstate', '.dpkg-old', '.dpkg-dist', '~')):
                    continue
                try:
                    if inst and os.stat(fp).st_mtime <= inst + 86400:
                        continue  # untouched since the OS was installed
                except OSError:
                    pass
                cust.append(fp)
    mod = []
    try:
        import hashlib
        import subprocess
        outp = subprocess.run(['dpkg-query', '-W', '-f=${Conffiles}\\n'], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                              universal_newlines=True, timeout=60).stdout
        for l in outp.splitlines():
            p = l.split()
            if len(p) >= 2 and p[0].startswith('/etc/') and 'obsolete' not in p and os.path.isfile(p[0]):
                try:
                    if hashlib.md5(open(p[0], 'rb').read()).hexdigest() != p[1] and not under(p[0], HANDLED_ETC) \
                            and not under(p[0], re_etc) and not under(p[0], GENERATED_ETC):
                        mod.append(p[0])
                except OSError:
                    pass
    except Exception:
        pass
    if refd:
        w('**Read by your units/scripts — now restored automatically, %d:** %s' % (len(refd), ', '.join('`%s`' % x for x in refd)))
    if cust:
        w('**Custom files changed since the OS was installed (no package owns them), %d:**' % len(cust))
        w(code('\n'.join(sorted(cust)), cap=120))
    if mod:
        w('**Package config files you changed, %d:**' % len(mod))
        w(code('\n'.join(sorted(set(mod))), cap=80))
    if not cust and not mod:
        w('- ✓ nothing (or dpkg not available)')
    w('')

    # 5. app state outside the usual places
    w('## Application state in /var/lib and /opt')
    sizes = {pth: sz for sz, pth in du}
    rows = []
    for base in ('/var/lib', '/opt', '/srv'):
        if not os.path.isdir(base):
            continue
        for e in sorted(os.listdir(base)):
            pth = os.path.join(base, e)
            if not os.path.isdir(pth) or pth in owned or under(pth, REGENERABLE) or pth in ('/var/lib/serverstate', '/var/lib/docker', '/opt/serverstate') \
                    or (sizes.get(pth, 0) and sizes.get(pth, 0) < 64 * 1024 and not os.path.exists(os.path.join(F, pth.lstrip('/')))):
                continue
            cap_ = os.path.exists(os.path.join(F, pth.lstrip('/')))
            cov = covered(pth, bp)
            rows.append((pth, human(sizes.get(pth, '')) if sizes.get(pth) else '', 'yes' if cap_ else '',
                         '?' if cov is None else ('intentional' if cov == 'intentional' else ('✓' if cov else '✗'))))
    w(md_table(['Path', 'Size', 'Config captured', 'In data backup'], rows) if rows else '- ✓ nothing')
    w('')

    # 6. secrets embedded in scripts
    sec = []
    for pth in lines(os.path.join(S, 'scripts/index.txt')):
        txt = read(os.path.join(F, pth.lstrip('/')))
        for m in re.finditer(r'^\s*(?:export\s+)?([A-Z0-9_]*(?:KEY|TOKEN|PASS|SECRET)[A-Z0-9_]*)=["\']?[A-Za-z0-9+/_-]{16,}', txt, re.M):
            sec.append('%s: %s' % (pth, m.group(1)))
    if sec:
        w('## Secrets written into scripts')
        w('_Captured (encrypted) and restored, but consider moving them to a root-only env file._')
        w('\n'.join('- review ' + x for x in sorted(set(sec))))
        w('')

    # 7. host-level listeners
    hl = []
    for l in lines(os.path.join(V, 'listening.txt')):
        p = l.split(None, 2)
        procs = set(re.findall(r'"([^"]+)"', p[2] if len(p) > 2 else ''))
        if procs and not procs <= {'docker-proxy', 'systemd-resolve', 'sshd', 'systemd', 'chronyd', 'dockerd'}:
            hl.append('%s %s — %s' % (p[0], p[1], ', '.join(sorted(procs))))
    if hl:
        w('## Programs listening directly on the host (not docker)')
        w('_Make sure each is installed by a package, script or unit that the rebuild recreates._')
        w('\n'.join('- ' + x for x in sorted(set(hl))[:60]))
        w('')
    dk_ = read(os.path.join(tmp, 'dkms.txt')).strip()
    if dk_:
        w('## DKMS kernel modules (rebuilt per kernel)')
        w(code(dk_))
    sys.stdout.write('\n'.join(o).rstrip() + '\n')


def count(what, snap):
    if what == 'containers':
        print(len(load(os.path.join(snap, 'docker/containers.json'), []) or []))
    elif what == 'projects':
        print(len(load(os.path.join(snap, 'docker/compose-projects.json'), []) or []))


# --------------------------------------------------------------------------- main
def main():
    a = sys.argv[1:]
    if not a:
        sys.exit(__doc__)
    cmd, args = a[0], a[1:]
    if cmd == 'normalize-containers':
        dump(norm_containers(json.load(sys.stdin)))
    elif cmd == 'normalize-networks':
        dump(norm_networks(json.load(sys.stdin)))
    elif cmd == 'normalize-volumes':
        dump(norm_volumes(json.load(sys.stdin)))
    elif cmd == 'docker-diff':
        docker_diff(*args)
    elif cmd == 'compose-projects':
        dump(compose_projects(*args[:3]))
    elif cmd == 'compose-paths':
        compose_paths(args[0])
    elif cmd == 'compose-restore-list':
        compose_restore_list(*args)
    elif cmd == 'standalone-run':
        standalone_run(*args)
    elif cmd == 'network-create-cmds':
        network_create_cmds(args[0])
    elif cmd == 'referenced-paths':
        referenced_paths(*args)
    elif cmd == 'referenced-etc':
        print('\n'.join(referenced_etc(args[0])))
    elif cmd == 'lsblk-table':
        lsblk_table(args[0])
    elif cmd == 'lsblk-find':
        lsblk_find(*args)
    elif cmd == 'lsblk-label':
        lsblk_label(*args)
    elif cmd == 'data-locations':
        print_data_locations(args[0], args[1] if len(args) > 1 else '')
    elif cmd == 'notes-stubs':
        notes_stubs(args[0])
    elif cmd == 'context':
        context(*args)
    elif cmd == 'normalize-services':
        dump(norm_services(json.load(sys.stdin), args[0] if args else ''))
    elif cmd == 'integrations':
        integrations(args[0])
    elif cmd == 'kuma-inventory':
        kuma_inventory(args[0])
    elif cmd == 'audit':
        audit(*args)
    elif cmd == 'count':
        count(*args)
    else:
        sys.exit('unknown command ' + cmd)


if __name__ == '__main__':
    main()
