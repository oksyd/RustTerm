# POSIX startup reads ENV instead of profiles. Leave POSIX mode before loading
# the normal login configuration, keeping Bash's native login/logout behavior.
set +o posix
if [[ $__RUSTTERM_ENV_SET == x ]]; then
    ENV=$__RUSTTERM_ORIGINAL_ENV
    export ENV
else
    unset ENV
fi
unset __RUSTTERM_ORIGINAL_ENV __RUSTTERM_ENV_SET
command rm -rf -- "$__RUSTTERM_SHELL_DIR"
unset __RUSTTERM_SHELL_DIR
[[ -r /etc/profile ]] && source /etc/profile
for __rustterm_profile in ~/.bash_profile ~/.bash_login ~/.profile; do
    if [[ -r $__rustterm_profile ]]; then
        source "$__rustterm_profile"
        break
    fi
done
unset __rustterm_profile
