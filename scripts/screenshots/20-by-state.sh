# Sidebar in the State view: sessions grouped by Needs you, Working and Idle.
caption="State view"
alt="State view with Needs you, Working and Idle sections"
start_args=(--showcase)

shot() {
    click_label ChoiceChips::chip-touch State
    wait_label Working
    wait_label Idle
}
