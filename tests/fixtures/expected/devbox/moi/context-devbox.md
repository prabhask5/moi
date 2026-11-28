The instance **devbox** is a devbox: the owner's development machine (VS Code in the browser, coding agents, their repositories), user `dev`. It sleeps when idle and wakes when someone opens it; a gateway container on this host does the waking.
- **URLs:** editor `https://code.example.com`, agents app `https://agents.example.com`.
- **Sleep:** after 15m idle, stateful (its memory is saved to disk when it sleeps and restored when it wakes).
- **Now:** VM running, gateway running (phase idle); last put to sleep 2026-09-21 11:26 (idle, took 4.2 s).
- **Health:** ok
- **Deployed:** gateway `3f9c2d1` (its container was not found).
- **The owner's developer context:** no dotfiles repository is configured (`moi devbox up --dotfiles <git url>`).
- **Rebuild:** the VM's disk is not backed up; `/etc/devbox` is. `moi devbox up` recreates and provisions the VM from it. Firewall: the gateway reaches the VM through a marked block in moi's rules file (`# devbox: begin … end`), managed by `moi`.
- **Commands:** `moi devbox status`, `moi devbox doctor`, `moi devbox connect`; change it with `moi devbox up` (never by editing `/etc/devbox` by hand).
