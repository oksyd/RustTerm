LC_ALL=C
export LC_ALL
if [ "$(uname -s)" != Linux ] || ! command -v ss >/dev/null 2>&1; then
    exit 1
fi
listeners=$(ss -H -ltnpe 2>/dev/null) || exit 1
printf 'RUSTTERM_PORTS_V1 %s\n' "$(id -u)"
printf '%s\n' "$listeners"
printf 'RUSTTERM_PORTS_END\n'
printf 'RUSTTERM_SSH_PORT %s\n' "${SSH_CONNECTION##* }"
for pid in $(printf '%s\n' "$listeners" | grep -o 'pid=[0-9]*' | cut -d= -f2 | sort -u | head -n 256); do
    owner=$(stat -c %u "/proc/$pid" 2>/dev/null) || continue
    executable=$(readlink "/proc/$pid/exe" 2>/dev/null) || continue
    printf 'RUSTTERM_PROCESS\t%s\t%s\t%s\n' "$pid" "$owner" "$executable"
done
