if not set -q __RUSTTERM_INTEGRATED
    set -g __RUSTTERM_INTEGRATED 1
    function __rustterm_preexec --on-event fish_preexec
        printf '\e]133;C\a'
    end
    function __rustterm_postexec --on-event fish_postexec
        set -l code $status
        printf '\e]133;D;%s\a' $code
    end
    function __rustterm_cwd --on-event fish_prompt
        set -l path (string escape --style=url -- $PWD)
        set path (string replace -a '%2F' '/' -- $path)
        printf '\e]7;file://%s%s\a' $hostname $path
    end
    functions --copy fish_prompt __rustterm_original_prompt
    function __rustterm_status
        return $argv[1]
    end
    function fish_prompt
        set -l code $status
        printf '\e]133;P;RustTerm=fish\a'
        printf '\e]133;A\a'
        __rustterm_status $code
        __rustterm_original_prompt
        printf '\e]133;B\a'
    end
end
