## Installation

| Système | Fichier |
|---|---|
| macOS 11 ou plus récent (Apple Silicon et Intel) | [`akolade-macos-universal.dmg`](https://github.com/emmanuellion/akolade/releases/latest/download/akolade-macos-universal.dmg) |
| Windows 10/11 (x64) | [`akolade-windows-x86_64-setup.exe`](https://github.com/emmanuellion/akolade/releases/latest/download/akolade-windows-x86_64-setup.exe) (installateur) ou [`akolade-windows-x86_64.zip`](https://github.com/emmanuellion/akolade/releases/latest/download/akolade-windows-x86_64.zip) (portable) |
| Linux (x64 / ARM) | [`akolade-linux-x86_64.AppImage`](https://github.com/emmanuellion/akolade/releases/latest/download/akolade-linux-x86_64.AppImage) ou [`akolade-linux-aarch64.AppImage`](https://github.com/emmanuellion/akolade/releases/latest/download/akolade-linux-aarch64.AppImage) (ou les `.tar.gz`) |

Git est conseillé : l'outil Git et les marques de modification s'en servent. Les outils facultatifs (Node.js pour les plugins, Claude Code, Docker, kubectl, `gh`) sont cherchés dans le `PATH`, comme dans le terminal.

### macOS
1. Ouvre le `.dmg` et glisse **Akolade** dans **Applications**.
2. L'app n'est pas signée par Apple : au premier lancement, macOS refuse de l'ouvrir.
   Va dans **Réglages Système › Confidentialité et sécurité**, puis clique sur **Ouvrir quand même** en face d'Akolade.
   (Ou dans un terminal : `xattr -dr com.apple.quarantine /Applications/Akolade.app`.)

### Windows
1. Lance `akolade-windows-x86_64-setup.exe` : il installe Akolade pour ton compte (sans droits administrateur), avec un raccourci dans le menu Démarrer. Il se désinstalle depuis **Paramètres › Applications**.
   Version portable : dézippe le `.zip` et lance `akolade.exe`.
2. Windows SmartScreen peut afficher « Windows a protégé votre ordinateur » : clique sur **Informations complémentaires › Exécuter quand même**.

### Linux
```sh
chmod +x akolade-linux-x86_64.AppImage
./akolade-linux-x86_64.AppImage
```
Il faut un pilote Vulkan (`mesa-vulkan-drivers` sur Intel/AMD, le pilote propriétaire sur NVIDIA).
Si l'AppImage ne démarre pas (FUSE absent), utilise l'archive `.tar.gz`.

L'intégrité des fichiers peut être vérifiée avec `SHA256SUMS.txt`.
