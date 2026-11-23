# Sidebar in the Groups view: the sessions that need the owner come first,
# followed by the groups of working and idle sessions.
caption="Sidebar"
alt="Sidebar with sessions that need the owner listed first, followed by groups of working and idle agent sessions"
start_args=(--showcase)

shot() {
    click_label ChoiceChips::chip-touch Groups
    wait_label "Needs you"
    wait_label review-pr-1412
}
