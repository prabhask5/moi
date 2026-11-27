# Before the package goes ($1 = 0 on removal, 1 on upgrade): stop the service.
if [ "$1" = 0 ] && command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]; then
    systemctl disable --now moi.service >/dev/null 2>&1 || true
fi
