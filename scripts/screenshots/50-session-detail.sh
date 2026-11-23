# The details view of one session, opened from the info action of its sidebar
# row: identity, agent, location and Work facts, each copyable, with Copy all
# and Close at the bottom.
caption="Session detail"
alt="Session details view with copyable id, host, agent, state and working directory"
start_args=(--showcase)

# Bounds of the SessionRow::row-touch element that holds the text TEXT, read
# from the element tree on stdin: "generation index y height".
row_bounds() {
    jq -r --arg text "$1" '
        ( [.elements[] | select(.accessibleLabel == $text)] ) as $texts
        | [ .elements[]
            | select(any(.typeNamesAndIds[]?; .id == "SessionRow::row-touch"))
            | . as $row
            | select(any($texts[];
                . as $t
                | (($t.absolutePosition.x // 0) >= (($row.absolutePosition.x // 0) - 1))
                  and ((($t.absolutePosition.x // 0) + $t.size.width)
                       <= ((($row.absolutePosition.x // 0) + $row.size.width) + 1))
                  and (($t.absolutePosition.y // 0) >= (($row.absolutePosition.y // 0) - 1))
                  and ((($t.absolutePosition.y // 0) + $t.size.height)
                       <= ((($row.absolutePosition.y // 0) + $row.size.height) + 1))))
            | [.handle.generation, .handle.index,
               .absolutePosition.y, .size.height] | @tsv ]
        | .[0] // empty'
}

# Index in `find SessionRow::info-icon` of the icon whose vertical bounds lie
# inside the row starting at Y with height H; the tree arrives on stdin.
icon_index() {
    local tree handle
    tree=$(cat)
    handle=$(printf '%s' "$tree" | jq -r --argjson y "$1" --argjson h "$2" '
        [ .elements[]
          | select(any(.typeNamesAndIds[]?; .id == "SessionRow::info-icon"))
          | select((.absolutePosition.y // 0) >= ($y - 1))
          | select(((.absolutePosition.y // 0) + .size.height) <= ($y + $h + 1))
          | (.handle.generation + ":" + .handle.index) ]
        | .[0] // empty') || return 1
    [ -n "$handle" ] || return 1
    "$agent" find SessionRow::info-icon |
        jq -r --arg h "$handle" \
            '[.elementHandles[] | (.generation + ":" + .index)] | index($h) // empty'
}

# The row actions exist only while their row is hovered, so hover the row that
# shows the label first, then click the info icon inside that row's bounds.
shot() {
    click_label ChoiceChips::chip-touch Groups
    wait_label docs-install-guide
    local waited=0 tree row n
    while [ "$waited" -lt "$wait_secs" ]; do
        if tree=$("$agent" tree 1000) && row=$(printf '%s' "$tree" | row_bounds docs-install-guide) &&
            [ -n "$row" ]; then
            read -r gen index row_y row_h <<<"$row"
            "$agent" call hover_element \
                "{\"elementHandle\":{\"generation\":\"$gen\",\"index\":\"$index\"}}" >/dev/null ||
                die "cannot hover the row of docs-install-guide"
            sleep 1
            tree=$("$agent" tree 1000) || die "cannot read the element tree after hovering"
            if n=$(printf '%s' "$tree" | icon_index "$row_y" "$row_h"); then
                "$agent" click SessionRow::info-icon "$n" >/dev/null ||
                    die "click on SessionRow::info-icon failed"
                wait_label "Copy all"
                return 0
            fi
        fi
        sleep 1
        waited=$((waited + 1))
    done
    die "the info action of docs-install-guide does not open its details after ${wait_secs}s"
}
