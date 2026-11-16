# Multiplexeur — spécification

## Contexte

Akolade est un éditeur de code natif en Rust (edition 2024) avec une interface gpui 0.2.2 et gpui-component 0.5.1. Les deux sont patchés dans `vendor/`, et chaque modification y est marquée « akolade: ». Tout tourne dans un seul processus. L'interface est en français, les commentaires du code en anglais.

Claude Code est déjà intégré dans `src/tools/claude.rs`, avec une seule conversation par vue :
- `Proc` lance `claude -p --input-format stream-json --output-format stream-json --permission-prompt-tool stdio`.
- `parse()` transforme le flux en `Event`. Les demandes d'autorisation (`can_use_tool`) reviennent à Akolade et deviennent des cartes Autoriser / Refuser.
- On y trouve aussi les modes, le resume, le suivi tokens/coût/limites et l'historique.
- `ClaudeView` sait déjà travailler dans un autre dossier que celui de l'éditeur (champ `custom`).

Git passe par le CLI dans `src/git.rs` (`git(root, args)`, status, commit, conflits). Il n'y a encore aucune gestion des worktrees.

Le mode restreint (`src/trust.rs`) empêche d'exécuter le code d'un projet non approuvé, y compris les hooks et serveurs MCP du dossier `.claude/`.

Les outils sont déclarés dans `src/tools/mod.rs`. L'outil Tests (`src/tools/tests.rs`) sait lancer cargo, go, pytest, jest et vitest.

## Objectif

Ajouter un outil « Multiplexeur » : une seule vue qui pilote N sessions Claude Code en parallèle. Il doit couvrir deux usages, mélangeables dans la même vue :

- **A. Plusieurs fonctionnalités d'un même projet en parallèle.** Les sessions de ce projet se coordonnent pour ne jamais modifier les mêmes fichiers en même temps ni casser le projet.
- **B. Plusieurs projets en parallèle.** Chaque session travaille dans son propre projet, indépendamment, et le multiplexeur sert de tableau de bord commun.

Exemple attendu : deux sessions sur deux fonctionnalités du projet X, plus une session sur le projet Y, toutes visibles dans la même vue.

## Modèle de données

- **Projet** : un dossier racine. S'il est dans un dépôt git, il est identifié par la racine de ce dépôt (`git::toplevel`). Plusieurs sessions dans des sous-dossiers d'un même dépôt appartiennent donc au même projet.
- **Session** : rattachée à exactement un projet, avec un nom, un rôle ou une tâche, et un mode de travail :
  - **Directe** : la session travaille dans le dossier du projet lui-même. C'est le choix par défaut quand elle est seule sur son projet (usage B).
  - **Isolée** : la session travaille dans un worktree dédié, sur une branche `akolade/<session>`. C'est le choix par défaut dès qu'une session rejoint un projet qui en a déjà une (usage A).
- Une seule session Directe peut être active par projet. Si on ajoute une deuxième session sur un projet qui a une session Directe, l'UI propose de passer la nouvelle en Isolée.
- Un projet sans git n'accepte qu'une session à la fois, en Directe. L'UI explique pourquoi et propose `git init`.
- Le multiplexeur est indépendant du dossier ouvert dans l'éditeur. Ouvrir ou changer de projet dans l'éditeur ne touche pas aux sessions en cours.

## Étape 0 : analyse et plan (avant tout code)

Lis `src/tools/claude.rs`, `src/tools/mod.rs`, `src/workspace.rs`, `src/git.rs`, `src/trust.rs`, `src/platform.rs` et `src/tools/tests.rs`. Écris ensuite `docs/multiplexeur.md` avec l'architecture, les trade-offs et les étapes.

Vérifie par de vrais essais avec le CLI `claude` installé les comportements dont dépend le design, et note-les dans le doc :
- `can_use_tool` est-il appelé pour Edit / Write / MultiEdit dans chaque permission-mode, en particulier `acceptEdits` et `bypassPermissions` ?
- Peut-on injecter un message utilisateur sur stdin pendant un tour, ou seulement entre deux tours ?
- Comment brancher un serveur MCP avec `--mcp-config` sans toucher au `.claude/` du projet ?

Regarde aussi comment `workspace.rs` gère l'ouverture d'un projet et si l'app sait ouvrir plusieurs fenêtres. Ça décide de ce que fait un clic sur un fichier d'un autre projet que celui de l'éditeur.

## Architecture attendue

### 1. Extraire le cœur de session

Sépare de `ClaudeView` un modèle `ClaudeSession` (Entity gpui) qui porte le `Proc`, le flux d'`Event`, les items, les stats, les demandes d'autorisation en attente et son dossier de travail. `ClaudeView` devient une vue de ce modèle, et son comportement actuel doit rester identique. Le multiplexeur réutilise ce modèle. Interdiction de dupliquer les 2000 lignes de `claude.rs`.

### 2. Projets et confiance

- Ajouter une session : choisir un projet parmi les récents ou un dossier quelconque, puis le mode (Directe ou Isolée, avec le défaut décrit plus haut), le nom, le rôle et la tâche initiale.
- Le mode restreint s'applique projet par projet, exactement comme aujourd'hui. Un projet non approuvé demande l'approbation avant le premier lancement. Un worktree hérite de l'approbation de son projet.
- Les réglages (modèle, mode, effort, budget) sont réglables par session. Ils partent des réglages globaux.
- Coût et limites : un compteur par session, par projet, et un total pour le multiplexeur. Les limites d'abonnement (5 h / 7 j) sont partagées par toutes les sessions et doivent être affichées comme telles.

### 3. Isolation par git worktree (sessions Isolées)

Ajoute à `src/git.rs` : worktree add / remove / list, merge, rebase, avec des tests qui suivent le modèle des tests git existants (dépôts temporaires). Les worktrees vivent dans un dossier géré par Akolade, hors du projet, rangés par projet. Nettoie-les à la fermeture d'une session, et détecte les orphelins au lancement.

### 4. Coordinateur (dans le processus Akolade)

L'état est persisté en JSON dans `app_dir()` via les helpers de `settings.rs` :
- **Global** : le registre des projets et des sessions, et la messagerie.
- **Par projet** : les baux de fichiers (chemin relatif à la racine du dépôt, propriétaire, intention, TTL, renouvellement) et la file d'intégration. Deux sessions de projets différents n'interagissent jamais via les baux ou la file.

Les baux sont appliqués côté Akolade : une demande `can_use_tool` pour Edit / Write / MultiEdit / NotebookEdit sur un fichier détenu par une autre session du même projet est refusée automatiquement, avec le nom du détenteur et son intention. Si le fichier est libre, le bail est pris implicitement. Si un mode contourne `can_use_tool` (voir l'étape 0), le multiplexeur ne propose pas ce mode tel quel. Il le remplace par une auto-approbation faite par Akolade, qui vérifie toujours les baux. Une session seule sur son projet ne paie aucun coût de coordination visible.

Les écritures via Bash ne sont pas couvertes par les baux : c'est l'isolation en worktree qui protège contre elles. Documente cette limite.

### 5. Serveur MCP de coordination

Il donne à chaque session les outils :
- `list_sessions(scope: projet | tout)` : les sessions de son projet par défaut, toutes sur demande, avec leur projet ;
- `send_message(to, text)`, qui fonctionne aussi entre projets (utile pour une API et son client dans deux dépôts) ;
- `broadcast(text, scope: projet | tout)`, par défaut limité au projet ;
- `read_inbox()` ;
- `claim_files(paths, intent)`, `release_files(paths)`, `list_claims()` et `request_merge(summary)`, qui portent toujours sur le projet de la session.

Approche préférée, sans nouvelle dépendance : le binaire akolade lui-même joue le serveur MCP stdio (sous-commande `akolade mcp-coord --session <id> --socket …`). Il relaie le JSON-RPC, écrit à la main avec serde_json, vers le processus principal par une socket locale. Il faut que ça marche sur macOS, Linux et Windows (named pipe ou TCP sur localhost avec un jeton).

Les messages reçus sont aussi poussés à la session, en message utilisateur entre deux tours, si l'étape 0 confirme que c'est possible.

Le `--append-system-prompt` de chaque session s'adapte à sa situation :
- seule sur son projet : son rôle, et la possibilité d'écrire aux autres sessions ;
- partage son projet : en plus, les autres sessions du projet et les règles (claim avant d'éditer, release après, lire son inbox, annoncer toute modification d'une interface partagée, `request_merge` quand c'est terminé).

Le prompt est mis à jour, au prochain tour, quand une session rejoint ou quitte le projet.

### 6. File d'intégration (par projet, sessions Isolées)

Chaque projet a sa branche d'intégration `akolade/integration`, qui fusionne les branches une par une, en FIFO. Avant chaque merge, Akolade lance le build et les tests du projet (réutilise la détection de `src/tools/tests.rs`). Si ça échoue ou qu'il y a un conflit, rien n'est fusionné : la session reçoit un message avec la sortie d'erreur ou les fichiers en conflit, puis rebase et corrige. La fusion vers la branche courante de l'utilisateur ne se fait que sur son action explicite. Jamais de push automatique. Les sessions Directes ne passent pas par la file : elles travaillent comme l'onglet Claude Code actuel.

### 7. Interface (outil « Multiplexeur »)

- Une grille redimensionnable de panneaux de session. Réutilise ce que gpui-component fournit (resizable, dock / tiles) plutôt que de réécrire un layout. Chaque panneau reprend le rendu de `ClaudeView` : streaming, cartes d'outils, autorisations.
- Deux façons de regrouper, avec une bascule : par projet (une section par projet, avec ses sessions, ses baux et sa file) ou à plat (toutes les sessions, chacune marquée de la couleur et du nom de son projet).
- Une barre de session : nom, projet, rôle, mode Directe / Isolée et branche, état (idle, en cours, attend une autorisation, bloquée par un bail, erreur), coût.
- Les sessions qui attendent une autorisation ou une réponse sont mises en évidence et regroupées dans un compteur global, pour qu'on n'en rate aucune quand plusieurs projets tournent.
- Un panneau « Échanges » qui montre le fil des messages et les événements de coordination, filtrable par projet.
- Par projet partagé : la liste des baux actifs (avec libération forcée) et l'état de la file d'intégration avec la sortie des tests.
- Un clic sur un fichier dans une carte : si la session est sur le projet ouvert dans l'éditeur, le fichier s'ouvre comme aujourd'hui. Sinon, Akolade propose d'ouvrir ce projet, selon ce que l'étape 0 a établi (nouvelle fenêtre, ou changement de projet avec confirmation). Le fichier ouvert doit être celui du worktree pour une session Isolée.
- Une saisie pour écrire à une session, à un projet ou à toutes. Pause, arrêt et reprise par session.
- Les textes visibles sont en français, avec le ton et le vocabulaire actuels de l'app.

## Contraintes

- **Performance** : Akolade revendique 0,3 % de CPU au repos. Avec N sessions qui streament, les redessins doivent être regroupés, les listes virtualisées (`ListState` comme dans `claude.rs`) et aucun polling actif quand rien ne bouge. Les panneaux repliés ou hors écran ne se redessinent pas. Mesure la consommation avec `src/perf.rs` pour 1, 3 et 6 sessions et note les chiffres dans le doc.
- Aucune nouvelle dépendance sans justification dans `Cargo.toml`, dans le style des commentaires existants. Préfère ce que gpui compile déjà.
- Respecte les conventions du code : commentaires « shortcut: » pour les raccourcis assumés, helpers de `platform.rs` pour les processus (`own_group`, `kill_tree`), `ui.rs` pour les styles.
- Fermer Akolade ou une session tue proprement chaque `claude` et tout ce qu'il a lancé. Aucun processus orphelin.
- L'onglet Claude Code actuel ne doit subir aucune régression.

## Livraison par étapes

Chaque étape compile, passe `cargo test --locked`, puis est commitée.

1. Extraction de `ClaudeSession`, `ClaudeView` inchangé.
2. Multiplexeur avec N sessions Directes sur des projets différents (l'usage B fonctionne de bout en bout).
3. Sessions Isolées en worktrees sur un même projet.
4. Coordinateur et baux par projet, appliqués via `can_use_tool`.
5. Serveur MCP et messagerie (projet et global).
6. File d'intégration par projet, avec tests.
7. Finitions de l'UI (regroupements, attentes, ouverture de fichiers) et persistance / reprise après redémarrage.

## Tests

Tests unitaires `#[cfg(test)]` dans les modules, comme le reste du code, pour :
- l'identification du projet : sous-dossiers d'un même dépôt, projet sans git, choix du mode par défaut ;
- les baux : prise, conflit, TTL, libération forcée, chemins normalisés (y compris Windows), cloisonnement entre projets ;
- la messagerie (scope projet et global) et sa persistance ;
- le protocole MCP : requêtes et réponses JSON-RPC ;
- l'ordre de la file d'intégration et le refus d'un merge cassant ;
- les opérations worktree dans des dépôts temporaires.

La CI tourne sur Ubuntu, macOS et Windows : rien de spécifique à une plateforme sans son équivalent.

## Critères de fin

- **Usage B** : 3 sessions sur 3 projets différents tournent en parallèle dans la même vue, chacune dans son dossier, sans worktree ni coordination visible. Leurs autorisations et leurs coûts sont bien séparés.
- **Usage A** : 3 sessions Isolées sur un même projet avancent en parallèle et échangent via le MCP. Deux sessions qui veulent éditer le même fichier : la seconde est refusée automatiquement avec le nom du détenteur, puis réessaie après la libération. Leurs branches arrivent sur `akolade/integration` uniquement si build et tests passent, et un merge cassant est renvoyé à sa session.
- **Mixte** : 2 sessions sur le projet X et 1 sur le projet Y, dans la même vue. Les baux et la file de X n'affectent jamais Y. Un message envoyé de X vers Y arrive bien.
- Après un redémarrage d'Akolade, les sessions, messages et baux sont restaurés, et les worktrees orphelins sont nettoyés.
- Deux scénarios de démo reproductibles (multi-fonctionnalités et multi-projets) sont décrits dans `docs/multiplexeur.md` et fonctionnent de bout en bout.
- `cargo test --locked` passe. `cargo clippy` n'ajoute aucun avertissement.
- Le README contient une section « Multiplexeur » dans le style des autres sections, qui présente les deux usages.

Si une limite du CLI `claude` bloque un point, documente-la dans `docs/multiplexeur.md` avec le contournement choisi, puis continue.
