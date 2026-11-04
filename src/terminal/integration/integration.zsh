if [[ -z ${__RUSTTERM_INTEGRATED-} ]]; then
    typeset -g __RUSTTERM_INTEGRATED=1
    autoload -Uz add-zsh-hook
    __rustterm_precmd() {
        local rustterm_code=$? rustterm_cwd=$PWD
        rustterm_cwd=${rustterm_cwd//'%'/'%25'}
        rustterm_cwd=${rustterm_cwd//$'\e'/'%1B'}
        rustterm_cwd=${rustterm_cwd//$'\a'/'%07'}
        rustterm_cwd=${rustterm_cwd//$'\n'/'%0A'}
        rustterm_cwd=${rustterm_cwd//$'\r'/'%0D'}
        builtin printf '\e]133;D;%s\a\e]7;file://%s%s\a' "$rustterm_code" "${HOST-}" "$rustterm_cwd"
        builtin printf '\e]133;P;RustTerm=zsh\a'
        if [[ $PROMPT != *'133;A'* ]]; then
            PROMPT=$'%{\e]133;A\a%}'"$PROMPT"$'%{\e]133;B\a%}'
        fi
        return "$rustterm_code"
    }
    __rustterm_preexec() { builtin printf '\e]133;C\a'; }
    add-zsh-hook precmd __rustterm_precmd
    add-zsh-hook preexec __rustterm_preexec
fi
