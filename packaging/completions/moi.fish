# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_moi_global_optspecs
    string join \n config= secrets= json q/quiet h/help V/version
end

function __fish_moi_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_moi_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_moi_using_subcommand
    set -l cmd (__fish_moi_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c moi -n "__fish_moi_needs_command" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_needs_command" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_needs_command" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_needs_command" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_needs_command" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_needs_command" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_needs_command" -f -a "init" -d 'Discover the server, write config, install the service, take the first snapshot'
complete -c moi -n "__fish_moi_needs_command" -f -a "snapshot" -d 'Capture the full server state into the versioned state dir'
complete -c moi -n "__fish_moi_needs_command" -f -a "log" -d 'Show the change history of the server'
complete -c moi -n "__fish_moi_needs_command" -f -a "diff" -d 'Show what changed in a snapshot (default: the latest)'
complete -c moi -n "__fish_moi_needs_command" -f -a "context" -d 'Regenerate CONTEXT.md for AI assistants, or share it with an expiring link'
complete -c moi -n "__fish_moi_needs_command" -f -a "notes" -d 'Edit the owner\'s notes merged into CONTEXT.md'
complete -c moi -n "__fish_moi_needs_command" -f -a "audit" -d 'Find what is not backed up, not restorable, lost on reboot, or on the wrong disk'
complete -c moi -n "__fish_moi_needs_command" -f -a "check" -d 'Run health checks'
complete -c moi -n "__fish_moi_needs_command" -f -a "monitors" -d 'Uptime Kuma monitors'
complete -c moi -n "__fish_moi_needs_command" -f -a "backup" -d 'Back up databases and identity data locally and off-site'
complete -c moi -n "__fish_moi_needs_command" -f -a "push" -d 'Upload the encrypted state bundle off-site'
complete -c moi -n "__fish_moi_needs_command" -f -a "pull" -d 'Download the latest state bundle (on a new machine)'
complete -c moi -n "__fish_moi_needs_command" -f -a "restore" -d 'Rebuild this machine from a state bundle and the data backup'
complete -c moi -n "__fish_moi_needs_command" -f -a "verify" -d 'Check that everything that should run, runs (also after every boot)'
complete -c moi -n "__fish_moi_needs_command" -f -a "recovery-card" -d 'Print what you must keep outside the server to rebuild it'
complete -c moi -n "__fish_moi_needs_command" -f -a "firewall" -d 'Host firewall rules managed by moi'
complete -c moi -n "__fish_moi_needs_command" -f -a "migrate" -d 'Move a server from legacy scripts and cron jobs to moi'
complete -c moi -n "__fish_moi_needs_command" -f -a "config" -d 'Configuration'
complete -c moi -n "__fish_moi_needs_command" -f -a "status" -d 'Daemon status, last results and next scheduled jobs'
complete -c moi -n "__fish_moi_needs_command" -f -a "daemon" -d 'Run the daemon (used by moi.service)'
complete -c moi -n "__fish_moi_needs_command" -f -a "uninstall" -d 'Remove moi and restore anything it replaced'
complete -c moi -n "__fish_moi_needs_command" -f -a "ssh" -d 'SSH to this server from anywhere, through a tunnel'
complete -c moi -n "__fish_moi_needs_command" -f -a "devbox" -d 'Set up and run a devbox VM (devbox\'s own commands, with moi\'s firewall and records)'
complete -c moi -n "__fish_moi_needs_command" -f -a "completions" -d 'Print a shell completion script (bash, zsh, fish)'
complete -c moi -n "__fish_moi_needs_command" -f -a "man" -d 'Print the man page (roff)'
complete -c moi -n "__fish_moi_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand init" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand init" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand init" -l non-interactive -d 'Do not prompt; write defaults and secret: references'
complete -c moi -n "__fish_moi_using_subcommand init" -l observe -d 'Only capture state (no service, no checks, no backups)'
complete -c moi -n "__fish_moi_using_subcommand init" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand init" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand init" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand init" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand snapshot" -l state -d 'Override the state directory' -r -F
complete -c moi -n "__fish_moi_using_subcommand snapshot" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand snapshot" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand snapshot" -l push -d 'Push the state bundle afterwards (if changed)'
complete -c moi -n "__fish_moi_using_subcommand snapshot" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand snapshot" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand snapshot" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand snapshot" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand log" -s n -d 'Number of entries' -r
complete -c moi -n "__fish_moi_using_subcommand log" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand log" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand log" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand log" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand log" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand log" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand diff" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand diff" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand diff" -l show-secrets -d 'Show secret values instead of masking them (root only)'
complete -c moi -n "__fish_moi_using_subcommand diff" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand diff" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand diff" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand diff" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand context; and not __fish_seen_subcommand_from share help" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand context; and not __fish_seen_subcommand_from share help" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand context; and not __fish_seen_subcommand_from share help" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand context; and not __fish_seen_subcommand_from share help" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand context; and not __fish_seen_subcommand_from share help" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand context; and not __fish_seen_subcommand_from share help" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand context; and not __fish_seen_subcommand_from share help" -f -a "share" -d 'Upload CONTEXT.md to the private share bucket and print an expiring link'
complete -c moi -n "__fish_moi_using_subcommand context; and not __fish_seen_subcommand_from share help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand context; and __fish_seen_subcommand_from share" -l ttl -d 'Link lifetime such as 30m or 2h (default from config, capped at context.share.max_ttl)' -r
complete -c moi -n "__fish_moi_using_subcommand context; and __fish_seen_subcommand_from share" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand context; and __fish_seen_subcommand_from share" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand context; and __fish_seen_subcommand_from share" -l revoke -d 'Delete every shared copy now (links stop working immediately)'
complete -c moi -n "__fish_moi_using_subcommand context; and __fish_seen_subcommand_from share" -l list -d 'List active shares and their expiry'
complete -c moi -n "__fish_moi_using_subcommand context; and __fish_seen_subcommand_from share" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand context; and __fish_seen_subcommand_from share" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand context; and __fish_seen_subcommand_from share" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand context; and __fish_seen_subcommand_from share" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand context; and __fish_seen_subcommand_from help" -f -a "share" -d 'Upload CONTEXT.md to the private share bucket and print an expiring link'
complete -c moi -n "__fish_moi_using_subcommand context; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand notes" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand notes" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand notes" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand notes" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand notes" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand notes" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand audit" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand audit" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand audit" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand audit" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand audit" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand audit" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand check" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand check" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand check" -l push -d 'Push results to Uptime Kuma (manual runs never push otherwise)'
complete -c moi -n "__fish_moi_using_subcommand check" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand check" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand check" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand check" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand monitors; and not __fish_seen_subcommand_from sync list help" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand monitors; and not __fish_seen_subcommand_from sync list help" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand monitors; and not __fish_seen_subcommand_from sync list help" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand monitors; and not __fish_seen_subcommand_from sync list help" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand monitors; and not __fish_seen_subcommand_from sync list help" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand monitors; and not __fish_seen_subcommand_from sync list help" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand monitors; and not __fish_seen_subcommand_from sync list help" -f -a "sync" -d 'Adopt existing monitors and create missing ones'
complete -c moi -n "__fish_moi_using_subcommand monitors; and not __fish_seen_subcommand_from sync list help" -f -a "list" -d 'List monitors and which check owns each'
complete -c moi -n "__fish_moi_using_subcommand monitors; and not __fish_seen_subcommand_from sync list help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from sync" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from sync" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from sync" -l dry-run -d 'Print the plan only (works with read-only access to kuma.db)'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from sync" -l update -d 'Apply drift (never changes push tokens)'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from sync" -l prune -d 'Delete orphaned monitors that moi created'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from sync" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from sync" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from sync" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from sync" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from list" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from list" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from list" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from list" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from list" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from list" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from help" -f -a "sync" -d 'Adopt existing monitors and create missing ones'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from help" -f -a "list" -d 'List monitors and which check owns each'
complete -c moi -n "__fish_moi_using_subcommand monitors; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand backup" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand backup" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand backup" -l dry-run -d 'Show what would be backed up (paths, dumps, sizes, why) and run every guard, without writing'
complete -c moi -n "__fish_moi_using_subcommand backup" -l drill -d 'Restore-test the latest backup into throwaway containers'
complete -c moi -n "__fish_moi_using_subcommand backup" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand backup" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand backup" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand backup" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand push" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand push" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand push" -l force -d 'Push even if nothing changed'
complete -c moi -n "__fish_moi_using_subcommand push" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand push" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand push" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand push" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand pull" -l repository -d 'restic repository URL (default: from config)' -r
complete -c moi -n "__fish_moi_using_subcommand pull" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand pull" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand pull" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand pull" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand pull" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand pull" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand restore" -l bundle -d 'Extracted bundle directory (from `moi pull`)' -r -F
complete -c moi -n "__fish_moi_using_subcommand restore" -l only -d 'Only these phases (comma separated)' -r
complete -c moi -n "__fish_moi_using_subcommand restore" -l from -d 'Start at this phase' -r
complete -c moi -n "__fish_moi_using_subcommand restore" -l skip -d 'Skip these phases (comma separated)' -r
complete -c moi -n "__fish_moi_using_subcommand restore" -l snapshot -d 'Restore the data from this restic snapshot instead of the latest one' -r
complete -c moi -n "__fish_moi_using_subcommand restore" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand restore" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand restore" -l dry-run -d 'Print every action without changing anything'
complete -c moi -n "__fish_moi_using_subcommand restore" -l yes -d 'Don\'t ask before each phase (never skips disk-format or firewall confirmations)'
complete -c moi -n "__fish_moi_using_subcommand restore" -l list -d 'List the phases'
complete -c moi -n "__fish_moi_using_subcommand restore" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand restore" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand restore" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand restore" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand verify" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand verify" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand verify" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand verify" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand verify" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand verify" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand recovery-card" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand recovery-card" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand recovery-card" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand recovery-card" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand recovery-card" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand recovery-card" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand firewall; and not __fish_seen_subcommand_from apply diff status help" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand firewall; and not __fish_seen_subcommand_from apply diff status help" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand firewall; and not __fish_seen_subcommand_from apply diff status help" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand firewall; and not __fish_seen_subcommand_from apply diff status help" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand firewall; and not __fish_seen_subcommand_from apply diff status help" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand firewall; and not __fish_seen_subcommand_from apply diff status help" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand firewall; and not __fish_seen_subcommand_from apply diff status help" -f -a "apply" -d 'Apply the managed chains (auto-rollback when run over SSH)'
complete -c moi -n "__fish_moi_using_subcommand firewall; and not __fish_seen_subcommand_from apply diff status help" -f -a "diff" -d 'Show the difference between live and configured rules'
complete -c moi -n "__fish_moi_using_subcommand firewall; and not __fish_seen_subcommand_from apply diff status help" -f -a "status" -d 'Show which chains moi manages and their state'
complete -c moi -n "__fish_moi_using_subcommand firewall; and not __fish_seen_subcommand_from apply diff status help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from apply" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from apply" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from apply" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from apply" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from apply" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from apply" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from diff" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from diff" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from diff" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from diff" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from diff" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from diff" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from status" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from status" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from status" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from status" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from status" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from status" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from help" -f -a "apply" -d 'Apply the managed chains (auto-rollback when run over SSH)'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from help" -f -a "diff" -d 'Show the difference between live and configured rules'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from help" -f -a "status" -d 'Show which chains moi manages and their state'
complete -c moi -n "__fish_moi_using_subcommand firewall; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -f -a "plan" -d 'Show what will be replaced by what (read-only)'
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -f -a "shadow" -d 'Run moi checks without pushing while legacy keeps running'
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -f -a "compare" -d 'Compare moi\'s shadow messages with what Kuma received from legacy scripts'
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -f -a "switch" -d 'Hand items over from legacy to moi (same monitors, same tokens)'
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -f -a "rollback" -d 'Undo switched items from the archive'
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -f -a "finish" -d 'Remove archived legacy files from disk and finish the migration'
complete -c moi -n "__fish_moi_using_subcommand migrate; and not __fish_seen_subcommand_from plan shadow compare switch rollback finish help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from plan" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from plan" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from plan" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from plan" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from plan" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from plan" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from shadow" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from shadow" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from shadow" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from shadow" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from shadow" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from shadow" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from compare" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from compare" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from compare" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from compare" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from compare" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from compare" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from switch" -l only -d 'Only these items (comma separated)' -r
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from switch" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from switch" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from switch" -l force -d 'Switch even if comparisons are not clean'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from switch" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from switch" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from switch" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from switch" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from rollback" -l only -d 'Only these items (comma separated)' -r
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from rollback" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from rollback" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from rollback" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from rollback" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from rollback" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from rollback" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from finish" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from finish" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from finish" -l now -d 'Don\'t wait for the 7-day stability window'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from finish" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from finish" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from finish" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from finish" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from help" -f -a "plan" -d 'Show what will be replaced by what (read-only)'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from help" -f -a "shadow" -d 'Run moi checks without pushing while legacy keeps running'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from help" -f -a "compare" -d 'Compare moi\'s shadow messages with what Kuma received from legacy scripts'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from help" -f -a "switch" -d 'Hand items over from legacy to moi (same monitors, same tokens)'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from help" -f -a "rollback" -d 'Undo switched items from the archive'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from help" -f -a "finish" -d 'Remove archived legacy files from disk and finish the migration'
complete -c moi -n "__fish_moi_using_subcommand migrate; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand config; and not __fish_seen_subcommand_from check path help" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand config; and not __fish_seen_subcommand_from check path help" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand config; and not __fish_seen_subcommand_from check path help" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand config; and not __fish_seen_subcommand_from check path help" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand config; and not __fish_seen_subcommand_from check path help" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand config; and not __fish_seen_subcommand_from check path help" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand config; and not __fish_seen_subcommand_from check path help" -f -a "check" -d 'Validate config and secrets, print the effective config (secrets masked)'
complete -c moi -n "__fish_moi_using_subcommand config; and not __fish_seen_subcommand_from check path help" -f -a "path" -d 'Print the path of the config and secrets files'
complete -c moi -n "__fish_moi_using_subcommand config; and not __fish_seen_subcommand_from check path help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from check" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from check" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from check" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from check" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from check" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from check" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from path" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from path" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from path" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from path" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from path" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from path" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "check" -d 'Validate config and secrets, print the effective config (secrets masked)'
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "path" -d 'Print the path of the config and secrets files'
complete -c moi -n "__fish_moi_using_subcommand config; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand status" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand status" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand status" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand status" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand status" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand status" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand daemon" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand daemon" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand daemon" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand daemon" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand daemon" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand daemon" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand uninstall" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand uninstall" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand uninstall" -l purge -d 'Also delete state, history and archives'
complete -c moi -n "__fish_moi_using_subcommand uninstall" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand uninstall" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand uninstall" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand uninstall" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -f -a "up" -d 'Set up or adopt the tunnel path (idempotent)'
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -f -a "status" -d 'Connector, ingress, DNS, public handshake, keys'
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -f -a "connect" -d 'Print the one command to run on a computer'
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -f -a "authorize" -d 'Allow a computer\'s key to log in'
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -f -a "confirm" -d 'Keep a change made over the tunnel'
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -f -a "down" -d 'Restore what `up` replaced'
complete -c moi -n "__fish_moi_using_subcommand ssh; and not __fish_seen_subcommand_from up status connect authorize confirm down help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from up" -l hostname -d 'The public host name that answers SSH (default: `[ssh] hostname`)' -r
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from up" -l inner -d 'Used by moi itself: apply the tunnel steps into this record (the unit behind the rollback timer)' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from up" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from up" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from up" -l dry-run -d 'Print the plan without changing anything'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from up" -l yes -d 'Don\'t ask before applying the plan'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from up" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from up" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from up" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from up" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from status" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from status" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from status" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from status" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from status" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from status" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from connect" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from connect" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from connect" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from connect" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from connect" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from connect" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from authorize" -l name -d 'A name for the key (its comment)' -r
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from authorize" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from authorize" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from authorize" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from authorize" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from authorize" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from authorize" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from confirm" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from confirm" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from confirm" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from confirm" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from confirm" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from confirm" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from down" -l rollback -d 'Used by moi itself: undo this record (the rollback timer)' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from down" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from down" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from down" -l dry-run -d 'Print what would be restored without changing anything'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from down" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from down" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from down" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from down" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from help" -f -a "up" -d 'Set up or adopt the tunnel path (idempotent)'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from help" -f -a "status" -d 'Connector, ingress, DNS, public handshake, keys'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from help" -f -a "connect" -d 'Print the one command to run on a computer'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from help" -f -a "authorize" -d 'Allow a computer\'s key to log in'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from help" -f -a "confirm" -d 'Keep a change made over the tunnel'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from help" -f -a "down" -d 'Restore what `up` replaced'
complete -c moi -n "__fish_moi_using_subcommand ssh; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -f -a "up" -d 'Set up or adopt the VM, the gateway\'s config, routing and the firewall rule'
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -f -a "down" -d 'Undo routing, firewall and config changes, restoring what was there'
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -f -a "status" -d 'Show the VM, gateway, routing, hub and health on one screen'
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -f -a "doctor" -d 'Run every check and print the fix for each failure'
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -f -a "connect" -d 'Print the URLs, an SSH config snippet and how to install the phone app'
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -f -a "password" -d 'Rotate a password'
complete -c moi -n "__fish_moi_using_subcommand devbox; and not __fish_seen_subcommand_from up down status doctor connect password help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l domain -d 'The domain: the editor is at code.<domain>, the agents app at agents.<domain>' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l code-host -d 'The editor\'s host, when it is not code.<domain>' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l agents-host -d 'The agents app\'s host, when it is not agents.<domain>' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l instance -d 'The Incus instance [default: devbox]' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l cpu -d 'vCPUs of a new VM [default: 3]' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l memory -d 'Memory of a new VM [default: 5GiB]' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l disk -d 'Disk of a new VM [default: 60GiB]' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l image -d 'Image of a new VM [default: images:ubuntu/26.04/cloud]' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l user -d 'The user in the VM [default: the user who ran sudo]' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l dotfiles -d 'Your dotfiles repository, cloned into the VM and applied' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l idle -d 'Idle time before the VM sleeps [default: 15m]' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l no-agents -d 'Agents not to install (claude, codex, opencode)' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l routing -d 'How the hosts are routed to the gateway [default: traefik-file]' -r -f -a "traefik-file\t'Write a Traefik dynamic configuration file'
none\t'Route the hosts yourself'"
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l traefik-file -d 'The Traefik dynamic configuration file to write' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l github-token-from -d 'Give the VM this host\'s GitHub token: from gh, or from the environment' -r -f -a "gh\t''
env\t''"
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l ssh-keys -d 'SSH public keys for a new VM\'s user [default: the keys you log in with]' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l wake-password-file -d 'A file with the password that wakes the VM' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l app-password-file -d 'A file with the password of the agents app' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l binaries -d 'A directory with devbox-hub and devbox built for the VM, instead of a release' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l release -d 'The release to install into the VM [default: the latest]' -r
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l adopt -d 'Use an instance that exists already; its disk and data are kept'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l dry-run -d 'Show what would change and change nothing'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l yes -d 'Do not ask before applying (never supplies a password)'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from up" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from down" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from down" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from down" -l keep-vm -d 'Keep the VM (the default; here so that the choice can be written out)'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from down" -l destroy -d 'Also delete the VM, if `devbox up` created it'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from down" -l dry-run -d 'Show what would be undone and change nothing'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from down" -l yes -d 'Do not ask before undoing'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from down" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from down" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from down" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from down" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from status" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from status" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from status" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from status" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from status" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from status" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from doctor" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from doctor" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from doctor" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from doctor" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from doctor" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from doctor" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from connect" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from connect" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from connect" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from connect" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from connect" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from connect" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from password" -l password-file -d 'A file with the new password, instead of being asked for it' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from password" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from password" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from password" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from password" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from password" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from password" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from help" -f -a "up" -d 'Set up or adopt the VM, the gateway\'s config, routing and the firewall rule'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from help" -f -a "down" -d 'Undo routing, firewall and config changes, restoring what was there'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from help" -f -a "status" -d 'Show the VM, gateway, routing, hub and health on one screen'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from help" -f -a "doctor" -d 'Run every check and print the fix for each failure'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from help" -f -a "connect" -d 'Print the URLs, an SSH config snippet and how to install the phone app'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from help" -f -a "password" -d 'Rotate a password'
complete -c moi -n "__fish_moi_using_subcommand devbox; and __fish_seen_subcommand_from help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand completions" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand completions" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand completions" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand completions" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand completions" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand completions" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand man" -l config -d 'Config file' -r -F
complete -c moi -n "__fish_moi_using_subcommand man" -l secrets -d 'Secrets file [default: secrets.toml next to the config file]' -r -F
complete -c moi -n "__fish_moi_using_subcommand man" -l json -d 'Machine-readable JSON output where supported'
complete -c moi -n "__fish_moi_using_subcommand man" -s q -l quiet -d 'Only print warnings and errors'
complete -c moi -n "__fish_moi_using_subcommand man" -s h -l help -d 'Print help'
complete -c moi -n "__fish_moi_using_subcommand man" -s V -l version -d 'Print version'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "init" -d 'Discover the server, write config, install the service, take the first snapshot'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "snapshot" -d 'Capture the full server state into the versioned state dir'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "log" -d 'Show the change history of the server'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "diff" -d 'Show what changed in a snapshot (default: the latest)'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "context" -d 'Regenerate CONTEXT.md for AI assistants, or share it with an expiring link'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "notes" -d 'Edit the owner\'s notes merged into CONTEXT.md'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "audit" -d 'Find what is not backed up, not restorable, lost on reboot, or on the wrong disk'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "check" -d 'Run health checks'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "monitors" -d 'Uptime Kuma monitors'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "backup" -d 'Back up databases and identity data locally and off-site'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "push" -d 'Upload the encrypted state bundle off-site'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "pull" -d 'Download the latest state bundle (on a new machine)'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "restore" -d 'Rebuild this machine from a state bundle and the data backup'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "verify" -d 'Check that everything that should run, runs (also after every boot)'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "recovery-card" -d 'Print what you must keep outside the server to rebuild it'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "firewall" -d 'Host firewall rules managed by moi'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "migrate" -d 'Move a server from legacy scripts and cron jobs to moi'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "config" -d 'Configuration'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "status" -d 'Daemon status, last results and next scheduled jobs'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "daemon" -d 'Run the daemon (used by moi.service)'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "uninstall" -d 'Remove moi and restore anything it replaced'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "ssh" -d 'SSH to this server from anywhere, through a tunnel'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "devbox" -d 'Set up and run a devbox VM (devbox\'s own commands, with moi\'s firewall and records)'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "completions" -d 'Print a shell completion script (bash, zsh, fish)'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "man" -d 'Print the man page (roff)'
complete -c moi -n "__fish_moi_using_subcommand help; and not __fish_seen_subcommand_from init snapshot log diff context notes audit check monitors backup push pull restore verify recovery-card firewall migrate config status daemon uninstall ssh devbox completions man help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from context" -f -a "share" -d 'Upload CONTEXT.md to the private share bucket and print an expiring link'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from monitors" -f -a "sync" -d 'Adopt existing monitors and create missing ones'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from monitors" -f -a "list" -d 'List monitors and which check owns each'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from firewall" -f -a "apply" -d 'Apply the managed chains (auto-rollback when run over SSH)'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from firewall" -f -a "diff" -d 'Show the difference between live and configured rules'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from firewall" -f -a "status" -d 'Show which chains moi manages and their state'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from migrate" -f -a "plan" -d 'Show what will be replaced by what (read-only)'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from migrate" -f -a "shadow" -d 'Run moi checks without pushing while legacy keeps running'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from migrate" -f -a "compare" -d 'Compare moi\'s shadow messages with what Kuma received from legacy scripts'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from migrate" -f -a "switch" -d 'Hand items over from legacy to moi (same monitors, same tokens)'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from migrate" -f -a "rollback" -d 'Undo switched items from the archive'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from migrate" -f -a "finish" -d 'Remove archived legacy files from disk and finish the migration'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from config" -f -a "check" -d 'Validate config and secrets, print the effective config (secrets masked)'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from config" -f -a "path" -d 'Print the path of the config and secrets files'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from ssh" -f -a "up" -d 'Set up or adopt the tunnel path (idempotent)'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from ssh" -f -a "status" -d 'Connector, ingress, DNS, public handshake, keys'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from ssh" -f -a "connect" -d 'Print the one command to run on a computer'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from ssh" -f -a "authorize" -d 'Allow a computer\'s key to log in'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from ssh" -f -a "confirm" -d 'Keep a change made over the tunnel'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from ssh" -f -a "down" -d 'Restore what `up` replaced'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from devbox" -f -a "up" -d 'Set up or adopt the VM, the gateway\'s config, routing and the firewall rule'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from devbox" -f -a "down" -d 'Undo routing, firewall and config changes, restoring what was there'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from devbox" -f -a "status" -d 'Show the VM, gateway, routing, hub and health on one screen'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from devbox" -f -a "doctor" -d 'Run every check and print the fix for each failure'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from devbox" -f -a "connect" -d 'Print the URLs, an SSH config snippet and how to install the phone app'
complete -c moi -n "__fish_moi_using_subcommand help; and __fish_seen_subcommand_from devbox" -f -a "password" -d 'Rotate a password'
