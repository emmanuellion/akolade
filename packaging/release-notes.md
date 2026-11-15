## Installation

| Système | Fichier |
|---|---|
| macOS 11 ou plus récent (Apple Silicon et Intel) | [`akolade-macos-universal.dmg`](https://github.com/emmanuellion/akolade/releases/latest/download/akolade-macos-universal.dmg) |

1. Ouvre le `.dmg` et glisse **Akolade** dans **Applications**.
2. L'app n'est pas signée par Apple : au premier lancement, macOS refuse de l'ouvrir.
   Va dans **Réglages Système › Confidentialité et sécurité**, puis clique sur **Ouvrir quand même** en face d'Akolade.
   (Ou dans un terminal : `xattr -dr com.apple.quarantine /Applications/Akolade.app`.)

Git est conseillé (`xcode-select --install`). Les outils facultatifs (Node.js pour les plugins, Claude Code, Docker, kubectl, `gh`) sont cherchés dans le `PATH` de ton shell, comme dans le terminal.

L'intégrité des fichiers peut être vérifiée avec `SHA256SUMS.txt`.
