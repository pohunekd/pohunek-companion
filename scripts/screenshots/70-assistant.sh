# Assistant dialog for the pohunek-companion project: the Help intent selected
# and a request typed in, so the shot shows what the dialog asks before Start.
caption="Assistant"
alt="Assistant dialog with request and intent fields"
start_args=(--showcase)

# The request field exposes its text as an accessible value, not as an
# accessible label, so this polls the value instead of reusing wait_label.
wait_request() {
    local text=$1 waited=0 tree
    while [ "$waited" -lt "$wait_secs" ]; do
        if tree=$("$agent" tree 2000); then
            if printf '%s' "$tree" |
                jq -e --arg t "$text" 'any(.elements[]; .accessibleValue == $t)' >/dev/null; then
                return 0
            fi
        fi
        sleep 1
        waited=$((waited + 1))
    done
    die "no element has the value \"$text\" after ${wait_secs}s"
}

shot() {
    local project request out n handle
    request="Which of my sessions are waiting for me, and what are they blocked on?"
    project=$("$agent" pohunek project list --json |
        jq -r '.ok[] | select(.label == "pohunek-companion") | .id') ||
        die "cannot read the projects"
    [ -n "$project" ] || die "no project named pohunek-companion"
    out=$("$agent" dialog assistant --project "$project" --intent help) ||
        die "cannot start the assistant dialog"
    n=${out%% *}
    case $n in
        '' | *[!0-9]*) die "unexpected dialog output: $out" ;;
    esac
    export UI_AGENT_DIALOG=$n
    wait_label pohunek-companion
    handle=$("$agent" find MultiLineField::input |
        jq -r '.elementHandles[0] | .generation + ":" + .index') ||
        die "cannot find the request field"
    [ -n "$handle" ] || die "the request field has no handle"
    "$agent" call set_element_value "$(jq -nc \
        --arg generation "${handle%%:*}" --arg index "${handle#*:}" --arg value "$request" \
        '{elementHandle: {generation: $generation, index: $index}, value: $value}')" >/dev/null ||
        die "cannot set the request text"
    wait_request "$request"
}
