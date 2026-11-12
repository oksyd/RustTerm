if [[ -z ${__RUSTTERM_INTEGRATED-} ]]; then
    typeset -g __RUSTTERM_INTEGRATED=1
    autoload -Uz add-zsh-hook
    typeset -g __rustterm_search_hooks=0 __rustterm_search_active=0 __rustterm_search_redraw=0
    __rustterm_isearch_update() {
        __rustterm_search_redraw=1
        if (( ! __rustterm_search_active )); then
            __rustterm_search_active=1
            builtin printf '\e]133;P;RustTermInput=history-search\a'
        fi
        return 0
    }
    __rustterm_isearch_exit() {
        __rustterm_search_active=0
        builtin printf '\e]133;P;RustTermInput=editing\a'
        return 0
    }
    __rustterm_line_finish() {
        __rustterm_search_active=0
        builtin printf '\e]133;P;RustTermInput=finished\a'
        return 0
    }
    __rustterm_line_pre_redraw() {
        # Zsh can skip isearch-exit on a terminal interrupt. Inside incremental
        # search each redraw is preceded by isearch-update; the first ordinary
        # editor redraw therefore confirms the exit without inspecting keys/text.
        if (( __rustterm_search_active && ! __rustterm_search_redraw )); then
            __rustterm_isearch_exit
        fi
        __rustterm_search_redraw=0
        return 0
    }
    # The standard dispatcher preserves existing widgets and does not rebind keys.
    # Older/minimal Zsh installations continue with prompt/execution markers only.
    if zmodload zsh/zle 2>/dev/null &&
        autoload -Uz +X add-zle-hook-widget 2>/dev/null; then
        if add-zle-hook-widget isearch-update __rustterm_isearch_update 2>/dev/null &&
            add-zle-hook-widget isearch-exit __rustterm_isearch_exit 2>/dev/null &&
            add-zle-hook-widget line-init __rustterm_isearch_exit 2>/dev/null &&
            add-zle-hook-widget line-pre-redraw __rustterm_line_pre_redraw 2>/dev/null &&
            add-zle-hook-widget line-finish __rustterm_line_finish 2>/dev/null; then
            __rustterm_search_hooks=1
        else
            add-zle-hook-widget -d isearch-update __rustterm_isearch_update 2>/dev/null
            add-zle-hook-widget -d isearch-exit __rustterm_isearch_exit 2>/dev/null
            add-zle-hook-widget -d line-init __rustterm_isearch_exit 2>/dev/null
            add-zle-hook-widget -d line-pre-redraw __rustterm_line_pre_redraw 2>/dev/null
            add-zle-hook-widget -d line-finish __rustterm_line_finish 2>/dev/null
        fi
    fi
    __rustterm_precmd() {
        local rustterm_code=$? rustterm_cwd=$PWD
        __rustterm_search_active=0
        rustterm_cwd=${rustterm_cwd//'%'/'%25'}
        rustterm_cwd=${rustterm_cwd//$'\e'/'%1B'}
        rustterm_cwd=${rustterm_cwd//$'\a'/'%07'}
        rustterm_cwd=${rustterm_cwd//$'\n'/'%0A'}
        rustterm_cwd=${rustterm_cwd//$'\r'/'%0D'}
        builtin printf '\e]133;D;%s\a\e]7;file://%s%s\a' "$rustterm_code" "${HOST-}" "$rustterm_cwd"
        builtin printf '\e]133;P;RustTerm=zsh\a'
        builtin printf '\e]133;P;RustTermHistorySearch=%s\a' "$__rustterm_search_hooks"
        if [[ $PROMPT != *'133;A'* ]]; then
            PROMPT=$'%{\e]133;A\a%}'"$PROMPT"$'%{\e]133;B\a%}'
        fi
        return "$rustterm_code"
    }
    __rustterm_preexec() { builtin printf '\e]133;C\a'; }
    add-zsh-hook precmd __rustterm_precmd
    add-zsh-hook preexec __rustterm_preexec
fi
