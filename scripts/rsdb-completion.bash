_rsdb_complete_edit() {
    local cur prev target i word
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev=""
    if (( COMP_CWORD > 0 )); then
        prev="${COMP_WORDS[COMP_CWORD-1]}"
    fi

    if [[ "${COMP_WORDS[1]:-}" != "edit" ]]; then
        return 0
    fi

    if [[ "$prev" == "--target" || "$prev" == "--editor" ]]; then
        return 0
    fi

    if [[ "$cur" == -* ]]; then
        COMPREPLY=( $(compgen -W "--target --editor" -- "$cur") )
        return 0
    fi

    target=""
    for (( i=2; i<COMP_CWORD; i++ )); do
        word="${COMP_WORDS[i]}"
        if [[ "$word" == "--target" ]] && (( i + 1 < COMP_CWORD )); then
            target="${COMP_WORDS[i+1]}"
            ((i++))
            continue
        fi
        if [[ "$word" == "--editor" ]] && (( i + 1 < COMP_CWORD )); then
            ((i++))
            continue
        fi
    done

    local suggestions
    if [[ -n "$target" ]]; then
        suggestions="$(rsdb __complete-edit-path --target "$target" --partial "$cur" 2>/dev/null)" || return 0
    else
        suggestions="$(rsdb __complete-edit-path --partial "$cur" 2>/dev/null)" || return 0
    fi

    local line has_dir=0
    while IFS= read -r line; do
        [[ -z "$line" ]] && continue
        COMPREPLY+=("$line")
        [[ "$line" == */ ]] && has_dir=1
    done <<< "$suggestions"

    if (( has_dir )); then
        compopt -o nospace 2>/dev/null || true
    fi
}

_rsdb_completion() {
    case "${COMP_WORDS[1]:-}" in
        edit)
            _rsdb_complete_edit
            ;;
    esac
}

complete -F _rsdb_completion rsdb
