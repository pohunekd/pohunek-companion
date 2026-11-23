# Groups view with the pointer over one session row: the row shows its action
# icons (details, open folder, open link, stop, remove) on the right.
caption="Row actions"
alt="Action icons shown on a hovered session row"
start_args=(--showcase)

row_tag=SessionRow::row-touch
icon_tag=ActionIcon::icon-touch

# Prints the handle of the ROW_TAG element whose bounds contain the text TEXT,
# or nothing when there is none. Same bounds test as click_label in
# scripts/capture-screenshots.
match_handle() {
    local tag=$1 text=$2 tree=$3
    printf '%s' "$tree" | jq -r --arg tag "$tag" --arg text "$text" '
        [.elements[] | select(.accessibleLabel == $text)] as $texts
        | [.elements[]
           | select(any(.typeNamesAndIds[]?; .id == $tag))
           | . as $box
           | select(any($texts[];
               . as $t
               | (($t.absolutePosition.x // 0) >= (($box.absolutePosition.x // 0) - 1))
                 and ((($t.absolutePosition.x // 0) + $t.size.width)
                      <= ((($box.absolutePosition.x // 0) + $box.size.width) + 1))
                 and (($t.absolutePosition.y // 0) >= (($box.absolutePosition.y // 0) - 1))
                 and ((($t.absolutePosition.y // 0) + $t.size.height)
                      <= ((($box.absolutePosition.y // 0) + $box.size.height) + 1))))
           | (.handle.generation + ":" + .handle.index)]
        | .[0] // empty'
}

# True when the tree holds an ICON_TAG element inside the ROW_TAG element of the
# text TEXT, that is, when the row shows its action strip.
icons_shown() {
    local text=$1 tree=$2
    printf '%s' "$tree" | jq -e --arg row "$row_tag" --arg icon "$icon_tag" --arg text "$text" '
        [.elements[] | select(.accessibleLabel == $text)] as $texts
        | [.elements[]
           | select(any(.typeNamesAndIds[]?; .id == $row))
           | . as $box
           | select(any($texts[];
               . as $t
               | (($t.absolutePosition.x // 0) >= (($box.absolutePosition.x // 0) - 1))
                 and ((($t.absolutePosition.x // 0) + $t.size.width)
                      <= ((($box.absolutePosition.x // 0) + $box.size.width) + 1))
                 and (($t.absolutePosition.y // 0) >= (($box.absolutePosition.y // 0) - 1))
                 and ((($t.absolutePosition.y // 0) + $t.size.height)
                      <= ((($box.absolutePosition.y // 0) + $box.size.height) + 1))))
           ] as $rows
        | [.elements[]
           | select(any(.typeNamesAndIds[]?; .id == $icon))
           | . as $i
           | select(any($rows[];
               . as $r
               | (($i.absolutePosition.x // 0) >= ($r.absolutePosition.x // 0))
                 and ((($i.absolutePosition.x // 0) + $i.size.width)
                      <= ((($r.absolutePosition.x // 0) + $r.size.width) + 1))
                 and (($i.absolutePosition.y // 0) >= ($r.absolutePosition.y // 0))
                 and ((($i.absolutePosition.y // 0) + $i.size.height)
                      <= ((($r.absolutePosition.y // 0) + $r.size.height) + 1))))]
        | length > 0'
}

# Hovers the row of TEXT once. The element tree carries the handle hover_element
# takes, so no `find` lookup is needed.
hover_row() {
    local text=$1 handle gen index tree
    tree=$("$agent" tree 1000) || die "cannot read the element tree"
    require_complete_tree "$tree"
    handle=$(match_handle "$row_tag" "$text" "$tree") ||
        die "cannot match $row_tag against the element tree"
    [ -n "$handle" ] || return 1
    gen=${handle%%:*}
    index=${handle##*:}
    "$agent" call hover_element \
        "{\"elementHandle\":{\"generation\":\"$gen\",\"index\":\"$index\"}}" >/dev/null ||
        die "hover on $row_tag failed"
}

shot() {
    local text=docs-install-guide waited=0 tree
    click_label ChoiceChips::chip-touch Groups
    wait_label "$text"
    # The pointer move and the layout the icons need are not ordered with the
    # tree read, so hover again until the tree lists the icons inside that row.
    while [ "$waited" -lt "$wait_secs" ]; do
        hover_row "$text"
        sleep 1
        if tree=$("$agent" tree 1000); then
            require_complete_tree "$tree"
            if icons_shown "$text" "$tree"; then
                return 0
            fi
        fi
        waited=$((waited + 1))
    done
    die "the $text row shows no action icons after ${wait_secs}s"
}
