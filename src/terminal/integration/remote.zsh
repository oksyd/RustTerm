if [[ $__rustterm_user_zdotdir_set == x ]]; then
    ZDOTDIR=$__rustterm_user_zdotdir
else
    unset ZDOTDIR
fi
# Startup files must run at top level: sourcing them inside a function changes
# the scope of user declarations such as typeset -U path.
[[ -r ${ZDOTDIR-$HOME}/@STARTUP_FILE@ ]] && source "${ZDOTDIR-$HOME}/@STARTUP_FILE@"
if [[ @FINAL_STAGE@ == yes || ! -o rcs ]]; then
    command rm -rf -- "$__RUSTTERM_SHELL_DIR"
    unset __RUSTTERM_SHELL_DIR __rustterm_user_zdotdir __rustterm_user_zdotdir_set
else
    __rustterm_user_zdotdir=${ZDOTDIR-}
    __rustterm_user_zdotdir_set=${ZDOTDIR+x}
    ZDOTDIR=$__RUSTTERM_SHELL_DIR
fi
