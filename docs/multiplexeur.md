# Multiplexeur : architecture et plan

Ce document accompagne la spécification (`docs/multiplexeur-spec.md`). Il note ce qu'on a vérifié avec le vrai CLI `claude`, les choix d'architecture qui en découlent, leurs limites, et les scénarios de démonstration.

## 1. Essais avec le CLI `claude`

Les essais ont été faits avec Claude Code 2.1.325, dans un dossier jetable, avec le modèle Haiku. Le script lance `claude` comme Akolade : `-p --input-format stream-json --output-format stream-json --verbose --replay-user-messages --permission-prompt-tool stdio --permission-mode <mode>`. Il répond « allow » à chaque `can_use_tool` et journalise ce qui revient.

### 1.1 `can_use_tool` selon le mode

Consigne : « Avec l'outil Write crée b.txt, puis avec l'outil Edit modifie a.txt ».

| Mode | Write | Edit | Remarque |
|---|---|---|---|
| `default` | demandé | demandé | chaque écriture passe par Akolade |
| `acceptEdits` | **non demandé** | **non demandé** | les écritures partent sans nous |
| `bypassPermissions` | **non demandé** | **non demandé** | rien n'est demandé |
| `plan` | demandé (après `ExitPlanMode`) | demandé | `ExitPlanMode` est lui-même demandé |

Conséquence : `acceptEdits` et `bypassPermissions` contournent `can_use_tool`, donc les baux ne pourraient pas s'appliquer. Le multiplexeur lance toujours ses sessions en `default` (ou `plan`) et rejoue lui-même ces deux modes : Akolade répond « allow » à la place de l'utilisateur, après avoir vérifié les baux (voir 3.4). L'onglet Claude Code, lui, garde le comportement natif.

Autre limite : une règle d'autorisation de l'utilisateur (`allow` dans `~/.claude/settings.json` ou `.claude/settings.local.json`, par exemple `"Edit"`) fait aussi passer les écritures sans `can_use_tool`. Dans ce cas, c'est le worktree qui protège (voir 3.4).

### 1.2 Message envoyé pendant un tour

On demande `sleep 8` par Bash et on envoie un second message utilisateur 4 s plus tard :

```
[ 2.1] REPLAY u1 (message pris)
[ 2.1] TOOL_USE Bash sleep 8
[ 4.2] >>> INJECT u2 (pendant le tour)
[10.8] REPLAY u2 (message pris)
[13.4] RESULT#1 'fini.\nBANANE'
```

Un message écrit sur stdin pendant un tour est accepté. Claude le prend à l'étape suivante (après le résultat de l'outil en cours) et en tient compte dans le même tour : il n'y a qu'un seul `result`. `--replay-user-messages` renvoie le message avec son `uuid` au moment où il est pris. On peut donc pousser les messages de coordination à tout moment, sans attendre la fin du tour.

### 1.3 Serveur MCP par `--mcp-config`

`--mcp-config` accepte le JSON directement en argument, sans fichier :

```
--mcp-config '{"mcpServers":{"akolade":{"command":"/chemin/akolade","args":["mcp-coord",…],"env":{…}}}}'
```

Ce que l'on a observé :
- Dans le message `init`, le serveur apparaît en `{"name":"akolade","status":"connected","source":"dynamic"}` et ses outils s'appellent `mcp__akolade__<outil>`. Le `.claude/` et le `.mcp.json` du projet ne sont ni lus pour ça, ni modifiés.
- Sans `--strict-mcp-config`, les serveurs MCP de l'utilisateur et du projet restent chargés. Le multiplexeur ne met donc pas ce drapeau : la session garde les outils de l'utilisateur, et le mode restreint décide toujours pour ceux du projet.
- Avant `initialize`, Claude Code envoie une requête `server/discover` (id texte). Le serveur doit répondre par une erreur JSON-RPC `-32601` pour les méthodes inconnues, sinon il bloque le démarrage.
- Les appels aux outils MCP passent par `can_use_tool` en mode `default`. Avec `--allowedTools mcp__akolade__ping`, ils partent sans demande : le multiplexeur liste ainsi ses huit outils.
- Les outils MCP sont « différés » : Claude les charge par `ToolSearch` avant le premier appel, sans effet visible.

### 1.4 Fenêtres et ouverture de projet

`main.rs` ouvre une seule fenêtre au démarrage et quitte quand elle se ferme. `Workspace::set_root` change de projet en place : il arrête les serveurs de langage et les terminaux, puis rouvre les onglets du nouveau projet. Ouvrir une seconde fenêtre demanderait de dupliquer une partie de `main.rs` et du réglage « dernier dossier ».

Choix retenu : un clic sur un fichier d'un autre projet propose « Ouvrir le projet » (`set_root` sur le dossier de la session, avec confirmation) ou « Ouvrir seulement le fichier » (un onglet hors projet, comme aujourd'hui). Pour une session Isolée, le dossier et le fichier sont ceux du worktree.

## 2. Architecture

```
src/tools/claude.rs        ClaudeSession (modèle + conversation) et ClaudeView (l'onglet)
src/tools/mux/mod.rs       MuxView : l'outil « Multiplexeur » (grille, barres, Échanges)
src/tools/mux/project.rs   identification du projet, mode par défaut, chemins des worktrees
src/tools/mux/coord.rs     coordinateur : registre, baux, messagerie, file (pur, testé)
src/tools/mux/mcp.rs       JSON-RPC MCP (pur, testé), relais stdio, serveur local
src/tools/mux/queue.rs     file d'intégration : merge, build et tests, retour à la session
src/git.rs                 worktree add/remove/list/prune, merge, rebase, abort
src/trust.rs               un worktree hérite de la confiance de son projet
src/main.rs                sous-commande `akolade mcp-coord`
```

### 2.1 ClaudeSession et ClaudeView

`ClaudeSession` est une entité gpui qui porte le processus `claude`, le flux d'`Event`, les cartes, les statistiques, les demandes d'autorisation, le dossier de travail et la file des messages. Elle dessine aussi sa conversation (cartes, autorisations, zone de saisie). `ClaudeView` est l'onglet : il crée une session et l'affiche avec son en-tête et son panneau de statistiques.

Compromis : la session dessine elle-même ses cartes. La séparer complètement de son rendu obligerait à copier les cartes à chaque image, ou à recâbler 1 500 lignes de rendu sur une entité empruntée. Le multiplexeur affiche la même entité en mode compact (`chrome: false`), sans en-tête ni statistiques. Il n'y a ainsi aucun code dupliqué, et aucune différence de comportement entre l'onglet et le multiplexeur.

Ce que le multiplexeur ajoute à une session, par `SessionOptions` :
- `name`, `role`, couleur du projet ;
- `extra_args` : `--mcp-config`, `--allowedTools` des outils `mcp__akolade__*` ;
- `system_prompt` : ajouté à `--append-system-prompt`, reconstruit quand le projet change de composition (redémarrage par `--resume` au prochain tour, comme un changement de mode aujourd'hui) ;
- `gate` : appelé à chaque `can_use_tool`. Il peut autoriser (mode auto rejoué ou bail libre), refuser (bail détenu par une autre session, avec son nom et son intention) ou laisser la carte à l'utilisateur.

### 2.2 Projets et modes

`project_of(dir)` renvoie la racine du dépôt (`git::toplevel`), sinon le dossier lui-même. Deux sessions dans `X/api` et `X/web` d'un même dépôt appartiennent au même projet.

Mode par défaut :
- **Directe** si la session est seule sur son projet ;
- **Isolée** dès qu'une session existe déjà sur le projet.

Règles :
- une seule session Directe active par projet ;
- un projet sans Git n'accepte qu'une session. L'interface l'explique et propose `git init`.

### 2.3 Worktrees

Les sessions Isolées travaillent dans `app_dir()/worktrees/<projet>/<session>`, sur la branche `akolade/<session>` créée depuis la branche courante de l'utilisateur. `<projet>` est le nom du dossier suivi d'un court hachage de son chemin, pour que deux projets homonymes ne se mélangent pas.

À la fermeture d'une session, son worktree est retiré mais **sa branche est gardée** : son travail n'est jamais perdu. Au lancement d'Akolade, tout dossier de `worktrees/` sans session enregistrée est un orphelin. Il est retiré avec `git worktree remove --force`, ou supprimé puis `git worktree prune` si le projet n'existe plus.

### 2.4 Coordinateur

Le coordinateur vit dans le processus Akolade, partagé (`Arc<Mutex<…>>`) entre l'interface et le serveur local. Il est persisté :
- `app_dir()/multiplexeur.json` : projets, sessions et messagerie (global) ;
- `app_dir()/multiplexeur/<projet>.json` : baux et file d'intégration (par projet).

**Baux.** Un bail porte sur un chemin relatif à la racine du dépôt, normalisé (`/`, sans `.` ni `..`, lettre de lecteur en minuscule sous Windows). Il a un propriétaire, une intention et une échéance (TTL de 15 minutes, renouvelé à chaque écriture du propriétaire). Les baux d'un projet ne voient jamais ceux d'un autre.

Côté `can_use_tool`, pour Edit, Write, MultiEdit et NotebookEdit :
- fichier libre : le bail est pris implicitement ;
- fichier détenu par une autre session : refus automatique avec le nom du détenteur et son intention ;
- session seule sur son projet : aucun bail visible, mais les baux restent tenus pour le jour où une autre session arrive.

**Limite : Bash.** Une commande (`sed -i`, `npm run format`…) écrit sans `can_use_tool` de type Edit. Les baux ne la voient pas. C'est le worktree qui protège : chaque session Isolée écrit dans sa propre copie, et seule la file d'intégration réunit le travail.

**Messagerie.** Un message a un expéditeur, un destinataire (session, projet ou tout le monde), une date et un état lu ou non lu. Il passe entre projets. Il est aussi poussé à la session destinataire comme message utilisateur (« [Message de api · projet X] … »), possible pendant un tour d'après 1.2. Une session en pause le trouve dans `read_inbox` à sa reprise.

### 2.5 Serveur MCP

`akolade mcp-coord --session <id>` est un relais stdio, sans état. Il lit les lignes JSON-RPC de Claude Code, les envoie au processus principal sur `127.0.0.1:<port>` (TCP, qui marche pareil sur macOS, Linux et Windows), et recopie les réponses. Le port et un jeton aléatoire de 128 bits passent par l'`env` de `--mcp-config`. Le serveur refuse toute ligne sans le bon jeton, et une session ne peut parler qu'en son propre nom.

Le JSON-RPC est écrit à la main avec serde_json : `initialize`, `tools/list`, `tools/call`, erreurs `-32601` / `-32602`, notifications ignorées. Les outils sont `list_sessions`, `send_message`, `broadcast`, `read_inbox`, `claim_files`, `release_files`, `list_claims` et `request_merge`.

### 2.6 File d'intégration

Chaque projet a sa branche `akolade/integration`, dans un worktree `worktrees/<projet>/integration`. `request_merge` met la branche de la session en file (FIFO). Le traitement, une demande à la fois par projet, se fait en arrière-plan :
1. `git merge --no-ff akolade/<session>` dans le worktree d'intégration. En cas de conflit : `merge --abort`, et la liste des fichiers est renvoyée à la session.
2. Build et tests avec la commande détectée par l'outil Tests (cargo, go, pytest, jest, vitest). En cas d'échec : `reset --hard` sur l'état d'avant, et la fin de la sortie est renvoyée à la session.
3. Sinon, la fusion est gardée et annoncée dans les Échanges.

Fusionner `akolade/integration` dans la branche de l'utilisateur se fait seulement par son bouton. Il n'y a jamais de push.

### 2.7 Confiance

Le mode restreint reste par projet : une session ne démarre dans un projet non approuvé qu'après la même question que l'onglet Claude Code. Un worktree est rattaché à son projet (`trust::link`) et hérite de son approbation : Git et Claude Code y tournent comme dans le projet.

### 2.8 Processus

Chaque `claude` est lancé dans son propre groupe (`platform::own_group`) et tué avec tout ce qu'il a lancé (`platform::kill_tree`) quand la session s'arrête, se ferme, ou quand Akolade quitte. Le relais `mcp-coord` est un enfant de `claude` : il meurt avec lui, et s'arrête aussi de lui-même quand stdin se ferme.

### 2.9 Performance

- Les événements de chaque session arrivent par un canal et un seul `notify` par lot. Rien ne tourne quand aucune session ne parle : ni minuteur, ni sondage.
- Les cartes sont virtualisées (`ListState`), comme dans l'onglet.
- Un panneau replié n'affiche que sa barre et ne dessine pas sa conversation.
- Le serveur local attend sur `accept` et `read`, sans boucle active.

Mesures : voir 5.

## 3. Étapes de livraison

1. Extraction de `ClaudeSession`, sans changement de l'onglet Claude Code.
2. Multiplexeur avec N sessions Directes sur des projets différents (usage B).
3. Sessions Isolées en worktrees (git.rs : worktree, merge, rebase).
4. Coordinateur et baux, appliqués via `can_use_tool` ; modes auto rejoués.
5. Serveur MCP et messagerie.
6. File d'intégration.
7. Finitions de l'interface et persistance après redémarrage.

## 4. Scénarios de démonstration

*(complétés à l'étape 7)*

## 5. Mesures

*(complétées à l'étape 7)*
