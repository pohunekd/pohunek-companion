# The New session dialog for the tokio project, with a session name and a
# prompt filled in, so the shot shows every field the dialog collects.
caption="New session"
alt="New session dialog with host, project, agent and prompt fields"
start_args=(--showcase)

# Waits until the element TAG shows TEXT as its accessible value. A LineEdit
# and a TextEdit carry their content there, not in the accessible label, so
# wait_label cannot see it.
wait_value() {
    local tag=$1 text=$2 waited=0 tree
    while [ "$waited" -lt "$wait_secs" ]; do
        if tree=$("$agent" tree 1000); then
            require_complete_tree "$tree"
            if printf '%s' "$tree" |
                jq -e --arg tag "$tag" --arg t "$text" '
                    any(.elements[]
                        | select(any(.typeNamesAndIds[]?; .id == $tag));
                        .accessibleValue == $t)' >/dev/null; then
                return 0
            fi
        fi
        sleep 1
        waited=$((waited + 1))
    done
    die "$tag does not show the value \"$text\" after ${wait_secs}s"
}

# The dialog opens with the project list dropped down over the agent, name and
# prompt fields; clicking the field itself closes it again. Escape is no way
# out here: dispatch_key_event sends it as the literal text "Escape".
close_project_list() {
    local waited=0 tree
    while [ "$waited" -lt "$wait_secs" ]; do
        tree=$("$agent" tree 1000) || die "cannot read the element tree"
        require_complete_tree "$tree"
        if ! printf '%s' "$tree" |
            jq -e 'any(.elements[]; .accessibleLabel == "neovim")' >/dev/null; then
            return 0
        fi
        "$agent" click NewSessionWindow::project-field 0 >/dev/null ||
            die "click on the project field failed"
        sleep 1
        waited=$((waited + 1))
    done
    die "the project list is still open after ${wait_secs}s"
}

# Clicks the field and types TEXT into it one character at a time.
# set_element_value only writes the accessible value: the visible text of both
# fields stays empty, so the shot would show a dialog with empty boxes.
type_into() {
    local tag=$1 text=$2 gen index i ch
    read -r gen index < <("$agent" call list_windows '{}' |
        jq -r '.result.content[0].text | fromjson
               | .windowHandles[0] | "\(.generation) \(.index)"')
    [ -n "${gen:-}" ] || die "no window to type into $tag"
    "$agent" click "$tag" 0 >/dev/null || die "click on $tag failed"
    sleep 1
    i=0
    while [ "$i" -lt "${#text}" ]; do
        ch=${text:i:1}
        "$agent" call dispatch_key_event \
            "$(jq -nc --arg gen "$gen" --arg index "$index" --arg text "$ch" '
                {windowHandle: {generation: $gen, index: $index},
                 text: $text, eventType: "PressAndRelease"}')" >/dev/null ||
            die "cannot type character $i into $tag"
        i=$((i + 1))
    done
}

shot() {
    local project dialog
    project=$("$agent" pohunek project list --json |
        jq -r '.ok[] | select(.label == "tokio") | .id')
    [ -n "$project" ] || die "the isolated daemon has no project named tokio"
    read -r dialog _ < <("$agent" dialog new-session --project "$project")
    [ -n "${dialog:-}" ] || die "cannot start the new-session dialog"
    export UI_AGENT_DIALOG="$dialog"
    wait_label tokio
    wait_label shell
    close_project_list
    type_into NewSessionWindow::name-edit "bump-tokio-1.48"
    type_into NewSessionWindow::prompt-edit \
        "Bump tokio to 1.48 and fix the deprecations it reports."
    wait_value NewSessionWindow::name-edit "bump-tokio-1.48"
    wait_value NewSessionWindow::prompt-edit \
        "Bump tokio to 1.48 and fix the deprecations it reports."
}
