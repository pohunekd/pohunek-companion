# Sidebar narrowed to one project with the Ended state chip switched on.
caption="Filters"
alt="A project chip and the Ended chip active, showing only that project's sessions"
start_args=(--showcase)

shot() {
    click_label ChoiceChips::chip-touch Groups
    click_label ChoiceChips::chip-touch pohunek-companion
    click_label ChoiceChips::chip-touch Ended
    wait_label release-notes-0.1.2
}
