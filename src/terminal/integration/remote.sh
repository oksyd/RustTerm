set -eu
__rustterm_shell=${SHELL:-/bin/sh}
case ${__rustterm_shell##*/} in
    bash|zsh|fish) ;;
    *) exec "$__rustterm_shell" -l ;;
esac

__rustterm_umask=$(umask)
umask 077
__rustterm_directory=$(mktemp -d "${TMPDIR:-/tmp}/rustterm-shell.XXXXXXXXXX")
trap '__rustterm_status=$?; trap - 0; rm -rf -- "$__rustterm_directory"; exit "$__rustterm_status"' 0
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
export __RUSTTERM_SHELL_DIR=$__rustterm_directory

case ${__rustterm_shell##*/} in
    bash)
        printf '%s\n' @BASH_STARTUP@ > "$__rustterm_directory/env.bash"
        export __RUSTTERM_ORIGINAL_ENV=${ENV-}
        export __RUSTTERM_ENV_SET=${ENV+x}
        export ENV=$__rustterm_directory/env.bash
        umask "$__rustterm_umask"
        "$__rustterm_shell" --posix --login -i
        ;;
    zsh)
        printf '%s\n' @ZSH_ENV@ > "$__rustterm_directory/.zshenv"
        printf '%s\n' @ZSH_PROFILE@ > "$__rustterm_directory/.zprofile"
        printf '%s\n' @ZSH_RC@ > "$__rustterm_directory/.zshrc"
        printf '%s\n' @ZSH_LOGIN@ > "$__rustterm_directory/.zlogin"
        export __RUSTTERM_ORIGINAL_ZDOTDIR=${ZDOTDIR-}
        export __RUSTTERM_ZDOTDIR_SET=${ZDOTDIR+x}
        export ZDOTDIR=$__rustterm_directory
        umask "$__rustterm_umask"
        "$__rustterm_shell" --login -i
        ;;
    fish)
        printf '%s\n' @FISH_STARTUP@ > "$__rustterm_directory/init.fish"
        umask "$__rustterm_umask"
        "$__rustterm_shell" --login --interactive --init-command 'source "$__RUSTTERM_SHELL_DIR/init.fish"'
        ;;
esac
