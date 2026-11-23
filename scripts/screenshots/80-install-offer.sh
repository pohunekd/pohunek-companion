# The setup screen with no Pohunek CLI on PATH: the title, what the install
# downloads and checks, and the two buttons. Nothing is clicked, because
# Install downloads a release.
caption="Install from the companion"
alt="Setup screen saying Pohunek is not installed, explaining what the install downloads and checks, with Install Pohunek and Check again buttons"
start_args=(--setup install)

shot() {
    wait_label "Pohunek is not installed"
    wait_label "Install Pohunek"
}
