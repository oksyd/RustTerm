if [[ -z ${__RUSTTERM_INTEGRATED-} ]]; then
    __RUSTTERM_INTEGRATED=1
    __rustterm_precmd() {
        local status=$? path=$PWD
        path=${path//'%'/'%25'}
        path=${path//$'\e'/'%1B'}
        path=${path//$'\a'/'%07'}
        path=${path//$'\n'/'%0A'}
        path=${path//$'\r'/'%0D'}
        builtin printf '\e]133;D;%s\a\e]7;file://%s%s\a' "$status" "${HOSTNAME-}" "$path"
        return "$status"
    }
    __rustterm_prompt() {
        builtin printf '\e]133;P;RustTerm=bash\a'
        if [[ $PS1 != *'133;A'* ]]; then
            PS1='\[\e]133;A\a\]'"$PS1"'\[\e]133;B\a\]'
        fi
    }
    PS0=$'\e]133;C\a'"${PS0-}"
    PROMPT_COMMAND=(__rustterm_precmd "${PROMPT_COMMAND[@]}" __rustterm_prompt)
fi
