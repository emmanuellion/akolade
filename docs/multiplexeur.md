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

Trois modes de travail :
- **Directe** : dans le dossier du projet. Le mode par défaut d'une session seule sur son projet.
- **Copie** (`copy.rs`) : dans sa propre copie du dossier, sans Git, avec à côté son point de départ (les fichiers tels qu'ils étaient). Le mode par défaut d'une session qui rejoint d'autres sessions sur un projet : aucune de ses commandes ne touche le dossier des autres.
- **Isolée** : dans un worktree Git (2.3). Elle seule demande un dépôt ; sans dépôt, l'interface le dit et propose `git init`.

Le menu « + » d'une session ou de son projet, et « Mode de travail » du formulaire, choisissent le mode.

**Réintégrer une copie.** Quand la session a fini, elle appelle `request_merge` avec un résumé qui dit, fichier par fichier, ce qu'elle a changé et pourquoi. La demande passe par la file du projet (une réintégration à la fois). Pour chaque fichier, comparé au point de départ :
- changé dans la copie seulement : repris tel quel (créé, modifié ou supprimé) ;
- changé des deux côtés à des endroits différents : fusionné ligne à ligne (`git merge-file`, si Git est installé) ;
- changé des deux côtés aux mêmes endroits : un Claude Code tranche (`claude -p`, sans outils, sans session enregistrée, cinq minutes au plus). Il reçoit les trois versions, le résumé de la session et, pour le côté du projet, les résumés des copies réintégrées depuis qui ont touché ce fichier. Il écrit le fichier final et dit en une phrase ce qu'il a gardé ;
- binaire, ou arbitre en échec : la version du projet est gardée, celle de la copie y reste, et le rapport le dit.

Le rapport va à la session (un message d'Akolade) et dans la file du projet. Ensuite la copie et son point de départ repartent du dossier tel qu'il est devenu. Les dossiers de dépendances et de build (`node_modules`, `target`, `dist`…) ne sont ni comparés ni réintégrés : la copie garde les siens. Fermer une session copie supprime sa copie, sauf si elle a du travail non réintégré : elle reste alors, et une note dit où.

Les sessions Directes d'un même dossier partagent l'état de Git : leurs consignes leur interdisent de changer de branche, `git stash`, `git reset` et `git checkout` de fichiers, et de commiter d'autres fichiers que les leurs. Leur travail n'entre pas dans la file d'intégration (pas de branche) : il est déjà dans votre dossier.

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

**Limite : Bash.** Une commande (`sed -i`, `npm run format`…) écrit sans `can_use_tool` de type Edit. Les baux ne la voient pas. C'est le worktree qui protège : chaque session Isolée écrit dans sa propre copie, et seule la file d'intégration réunit le travail. Des sessions Directes dans un même dossier n'ont pas cette protection : une commande de l'une peut réécrire un fichier qu'une autre a réservé. Pour des formateurs ou des scripts qui touchent tout le projet, préférez des sessions Isolées.

**Messagerie.** Un message a un expéditeur, un destinataire (session, projet ou tout le monde), une date et un état lu ou non lu. Il passe entre projets. Il est aussi poussé à la session destinataire comme message utilisateur (« [Message de api · projet X] … »), possible pendant un tour d'après 1.2. Une session en pause le trouve dans `read_inbox` à sa reprise.

### 2.5 Serveur MCP

`akolade mcp-coord --session <id>` est un relais stdio, sans état. Il lit les lignes JSON-RPC de Claude Code, les envoie au processus principal sur `127.0.0.1:<port>` (TCP, qui marche pareil sur macOS, Linux et Windows), et recopie les réponses. Le port et un jeton aléatoire de 128 bits passent par l'`env` de `--mcp-config`. Le serveur refuse toute ligne sans le bon jeton, et une session ne peut parler qu'en son propre nom.

Vérifié sur macOS et sur Linux (Debian, dans Docker) : `cargo test --locked` y passe, dont `the_akolade_binary_relays_stdio_to_the_coordinator`, qui lance le vrai binaire `akolade mcp-coord` et lui parle en JSON-RPC par stdin et stdout. Windows : Akolade et ses tests compilent pour `x86_64-pc-windows-gnu` (MinGW, dans Docker), et les tests du multiplexeur tournent sous Wine 8 (émulé x86_64). 17 passent, dont le relais stdio avec le vrai `akolade.exe` et la normalisation des chemins Windows ; les tests de file et de worktrees s'arrêtent d'eux-mêmes faute de Git dans Wine. `the_relay_talks_to_the_server_with_the_token` passe une fois sur trois : les autres fois, Rosetta (l'émulation x86_64 de Docker sur Apple Silicon) s'arrête sur sa propre assertion (`x86_avx_state_ptr`). Pour Wine seulement, deux petites DLL remplacent `icuuc.dll` (`u_strlen`) et `bcryptprimitives.dll` (`ProcessPrng`), absentes de Wine 8. Ce n'est pas un vrai Windows : la CI (`windows-latest`) reste la vérification de référence.

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

### 2.10 Affichage

- **Vues.** « Par projet » et « À plat » ont chacune leurs sessions (`SessionInfo::flat`, celle d'où la session a été lancée).
- **Disposition** (menu Affichage, `mux_layout`) : Grille (trois par rangée), Colonnes (toutes côte à côte), Lignes (une par rangée), Onglets (une à la fois, les autres en onglets).
- **Ordre.** La barre d'une session (ou son onglet) se glisse sur une autre : elle prend sa place. L'ordre est celui du coordinateur, gardé au redémarrage.
- **Style des discussions** : Standard (cartes) ou Terminal (police à chasse fixe, `>` vos messages, `⏺` ses réponses et ses outils, `⎿` leur résultat). Le menu Affichage règle toutes les discussions (`mux_terminal`) ; le menu ⋯ d'une session lui en donne un à elle (`SessionInfo::terminal`). Ce qui attend votre accord reste une carte, avec ses boutons.

## 3. Étapes de livraison

1. Extraction de `ClaudeSession`, sans changement de l'onglet Claude Code.
2. Multiplexeur avec N sessions Directes sur des projets différents (usage B).
3. Sessions Isolées en worktrees (git.rs : worktree, merge, rebase).
4. Coordinateur et baux, appliqués via `can_use_tool` ; modes auto rejoués.
5. Serveur MCP et messagerie.
6. File d'intégration.
7. Finitions de l'interface et persistance après redémarrage.

## 4. Scénarios de démonstration

Préparation, commune aux deux scénarios :

```
scripts/multiplexeur-demo.sh /tmp/akolade-demo
```

Le script crée quatre petits dépôts :
- `calc` : une crate Rust avec ses tests, pour le scénario A ;
- `recettes`, `meteo` et `agenda` : un README chacun, pour le scénario B.

Ensuite, dans Akolade :
1. Ajoutez l'outil depuis le Store (Outils › Multiplexeur › Ajouter), puis ouvrez-le.
2. Pour chaque session : « Nouvelle session », collez le chemin du projet, choisissez le modèle Haiku (le moins coûteux pour une démo), puis « Lancer la session ».

La première fois qu'une session part dans un projet, Akolade demande d'approuver ce dossier (mode restreint).

### Scénario A : plusieurs fonctionnalités d'un même projet

Trois sessions sur `/tmp/akolade-demo/calc`. Choisissez **Isolée** pour chacune (« Mode de travail », ou « + › Dans son propre worktree »). Toutes les trois sont en « Tout autoriser », pour que la démo avance seule : Akolade rejoue ce mode et vérifie toujours les baux.

| Nom | Tâche |
|---|---|
| `mul` | Ajoute dans src/lib.rs une fonction `pub fn mul(a: i64, b: i64) -> i64` avec un test. Réserve d'abord le fichier (claim_files), commite, libère-le, puis appelle request_merge. |
| `div` | Ajoute dans src/lib.rs une fonction `pub fn div(a: i64, b: i64) -> Option<i64>` (None si b vaut 0) avec un test. Commite, puis appelle request_merge. |
| `casse` | Modifie `add` dans src/lib.rs pour qu'elle renvoie `a + b + 1`, sans toucher aux tests. Commite, puis appelle request_merge. |

Ce qu'on observe :
1. Chaque session travaille dans son worktree (barre « Isolée · akolade/mul-1 »…). Votre copie de `calc` ne bouge pas.
2. `mul` réserve `src/lib.rs` : le bail apparaît sous l'en-tête du projet. Quand `div` (ou `casse`) veut écrire ce fichier, sa carte passe en « Refusé ». Sa barre indique « Bloquée par un bail » et la raison, avec le nom `mul`, s'affiche en infobulle. La session attend ou écrit à `mul`, puis réessaie après la libération (le fil apparaît dans « Échanges »).
3. Les demandes de fusion passent dans la file du projet, une à la fois. Les branches de `mul` et `div` arrivent sur `akolade/integration` après `cargo test`. Celle de `casse` est renvoyée : `adds` échoue, rien n'est fusionné, et la session reçoit le rapport dans sa conversation.
4. « Fusionner dans votre branche » apporte `akolade/integration` à `main`, sur votre confirmation. Rien n'est poussé.

### Scénario B : plusieurs projets à la fois

Trois sessions, une par projet : `/tmp/akolade-demo/recettes`, `/meteo` et `/agenda`. Elles sont toutes en **Directe** (le défaut, chacune étant seule sur son projet) et en « Demander ». Tâche de chacune : « Avec l'outil Write, crée NOTES.md contenant une phrase qui décrit ce projet, puis réponds OK. »

Ce qu'on observe :
1. Trois panneaux côte à côte, chacun dans son dossier. Ni worktree, ni bail, ni file : rien de la coordination ne s'affiche.
2. Chaque session demande son autorisation dans sa propre carte. Le compteur « 3 sessions en attente de votre accord » s'affiche en haut. Autoriser une session ne débloque qu'elle.
3. Chaque barre a son coût, chaque projet son total, et le multiplexeur le total général. Les limites de l'abonnement s'affichent une seule fois, comme communes.

### Mixte et messagerie entre projets

Deux sessions Isolées sur `calc` et une Directe sur `meteo`, dans la même vue. Demandez à une session de `calc` : « Envoie à la session de meteo, avec send_message, le texte : la fonction div existe maintenant. » Le message arrive dans la conversation de la session `meteo` (« [Message de … · projet calc] … ») et dans « Échanges ». Les baux et la file de `calc` ne s'affichent pas dans la section de `meteo`, et une demande de fusion de `meteo` serait refusée : elle est en Directe.

### Après un redémarrage

Quittez Akolade et relancez-le, puis rouvrez le Multiplexeur :
- les sessions reviennent en pause, avec leur conversation, leurs messages et leurs baux ;
- « Reprendre » relance une session sur la même conversation ;
- un dossier de `worktrees/` qui n'appartient plus à aucune session est retiré, et son travail non commité est gardé sur sa branche.

## 5. Mesures

CPU du processus Akolade seul (sans les `claude`), version release, sur macOS (Apple Silicon), fenêtre de 1 280 × 820 au premier plan. Mesure faite avec `src/perf.rs` (`perf_cpu_of_a_running_akolade`), qui compte le temps CPU consommé par seconde :

```
AKOLADE_PERF_PID=<pid> AKOLADE_PERF_SECS=40 cargo test perf_cpu -- --ignored --nocapture
```

Chaque session reçoit la même consigne sans outil (« écris un poème de 60 vers puis une explication de 300 mots », Haiku) : 20 à 45 s de texte en continu.

| Sessions | Pendant l'écriture (moyenne sur 40 s) | Pic sur une seconde | Au repos |
|---|---|---|---|
| 1 | 3,5 % | 23 % | — |
| 3 | 9,1 % | 26 % | — |
| 6 | 14,6 % | 40 % | 0,6 % |

Les mêmes mesures **avant** les optimisations de l'étape 7 : 37 % pour une session et 69 % pour trois. Ce qui a changé, pour les panneaux du multiplexeur seulement (l'onglet Claude Code garde son rendu) :
- **Panneaux en cache** (`AnyView::cached`) : un panneau ne se redessine que si sa propre session a changé, pas quand une autre écrit.
- **Texte regroupé** : le texte reçu s'affiche par lots, au plus dix fois par seconde par session (`ClaudeSession::throttle`), sans la révélation lettre par lettre de l'onglet, qui tourne à 60 images par seconde et analyse à nouveau le Markdown à chaque image.
- **Indicateurs fixes** : les animations d'attente redessinaient toute la fenêtre à chaque image tant qu'une session réfléchissait. Dans un panneau, l'indicateur est fixe, et la barre dit déjà « En cours ».
- **Rien quand rien ne bouge** : au-delà de deux rangées (ou dès deux quand la fenêtre fait moins de 1 100 points de haut, pour que chaque conversation garde sa place), la grille est une `uniform_list` qui ne construit que les rangées visibles ; un panneau replié ne dessine pas sa conversation ; une fenêtre cachée ne se redessine pas.

Vérifié en comptant les appels à `render` (compteur temporaire, retiré ensuite), 7 sessions sur trois rangées dans une fenêtre de 1 280 × 820 :
- pendant 20 s où la session de la troisième rangée, hors écran, écrit un long texte : 0 rendu de son panneau, 3 de la vue du multiplexeur, 4 de chaque panneau visible (les changements d'en-tête : coût, limites) ;
- une fois la grille défilée jusqu'à elle, son panneau se redessine normalement ;
- au repos, pendant 30 s : 0 rendu, 0,6 % de CPU, et 2 à 3 réveils du processus par seconde (`top -stats idlew`), ceux de macOS et de gpui, sans minuteur du multiplexeur.

Au repos, avec 6 sessions ouvertes, Akolade reste à 0,6 % (le README annonce 0,3 % pour l'éditeur seul). Le multiplexeur n'ajoute aucun sondage : ses seuls réveils sont les événements des sessions et du coordinateur.
