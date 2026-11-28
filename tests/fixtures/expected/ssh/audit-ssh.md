
- ✗ sshd accepts passwords, and it answers at the public name ssh.example.com: set `PasswordAuthentication no` (moi does not edit sshd)
- ✗ sshd lets root log in with a password, and it answers at the public name ssh.example.com: set `PermitRootLogin prohibit-password` or `no`
