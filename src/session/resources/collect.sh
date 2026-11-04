#!/bin/sh
set -efu
LC_ALL=C
export LC_ALL

if [ ! -r /proc/sys/kernel/ostype ] || [ "$(cat /proc/sys/kernel/ostype)" != Linux ]; then
    printf 'RTRES_UNSUPPORTED\n'
    exit 0
fi

printf 'RTRES\t1\n'
IFS= read -r boot_id < /proc/sys/kernel/random/boot_id
printf 'BOOT\t%s\n' "$boot_id"
awk '
    FILENAME == "/proc/stat" {
        if ($1 == "cpu" && NF >= 9) {
            cpu = $2 "\t" $3 "\t" $4 "\t" $5 "\t" $6 "\t" $7 "\t" $8 "\t" $9
        } else if ($1 ~ /^cpu[0-9]+$/) {
            cpus++
        }
        next
    }
    FILENAME == "/proc/meminfo" {
        if ($1 == "MemTotal:" || $1 == "MemAvailable:" || $1 == "SwapTotal:" || $1 == "SwapFree:") {
            if ($3 != "kB") exit 1
            memory[$1] = $2
        }
        next
    }
    FILENAME == "/proc/loadavg" { printf "LOAD\t%s\t%s\t%s\n", $1, $2, $3; next }
    FILENAME == "/proc/uptime" { printf "UPTIME\t%s\n", $1; next }
    FILENAME == "/proc/net/dev" && index($0, ":") {
        split($0, parts, ":")
        name = parts[1]
        gsub(/^[ \t]+|[ \t]+$/, "", name)
        values = parts[2]
        sub(/^[ \t]+/, "", values)
        split(values, counters, /[ \t]+/)
        if (++interfaces > 256) exit 1
        printf "NET\t%s\t%s\t%s\n", name, counters[1], counters[9]
    }
    END {
        if (cpu == "" || cpus < 1 || !("MemTotal:" in memory) || !("MemAvailable:" in memory) || !("SwapTotal:" in memory) || !("SwapFree:" in memory)) exit 1
        printf "CPU\t%d\t%s\n", cpus, cpu
        printf "MEM\t%s\t%s\t%s\t%s\n", memory["MemTotal:"], memory["MemAvailable:"], memory["SwapTotal:"], memory["SwapFree:"]
    }
' /proc/stat /proc/meminfo /proc/loadavg /proc/uptime /proc/net/dev

# stat only touches selected local filesystems. Never query network, FUSE or
# automount volumes. Kernel mountinfo escaping keeps whitespace and backslashes
# unambiguous both in shell arguments and in the output protocol.
if [ "${1:-0}" = 1 ] && [ -r /proc/self/mountinfo ] && command -v stat >/dev/null 2>&1; then
    if mounts=$(awk '
        {
            for (i = 7; i <= NF && $i != "-"; i++) {}
            type = $(i + 1)
            if (type !~ /^(ext2|ext3|ext4|xfs|btrfs|f2fs|jfs|reiserfs|zfs|bcachefs)$/ || seen[$3]++) next
            if (++count > 32 || length($5) > 16384) exit 1
            print $5
        }
    ' /proc/self/mountinfo); then
        if [ -n "$mounts" ]; then
            if disks=$(printf '%s\n' "$mounts" | while IFS= read -r encoded; do
                # The sentinel preserves a mount point ending in a newline.
                mount_point=$(printf '%b.' "$encoded")
                mount_point=${mount_point%.}
                blocks=$(stat -f -c '%S %b %f %a' -- "$mount_point") || exit 1
                set -- $blocks
                [ "$#" = 4 ] || exit 1
                printf 'DISK\t%s\t%s\t%s\t%s\t%s\n' "$encoded" "$1" "$2" "$3" "$4"
            done); then
                printf 'DISKS\n%s\n' "$disks"
            fi
        else
            printf 'DISKS\n'
        fi
    fi
fi
printf 'END\n'
