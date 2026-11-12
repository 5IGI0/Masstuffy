# 5. Invariants, concurrence et reprise après crash

## 5.1 Invariants

Ce qui doit être vrai au repos, en dehors d'une opération en cours :

1. **I1 — Segments en append-only** : un octet écrit dans un segment n'est jamais modifié. La seule exception est la mise à zéro volontaire pour une suppression physique.
2. **I2 — Index → données** : chaque ligne de `index.cdx` désigne un localisateur valide. Les octets `[offset, offset+raw_size)` existent dans le segment et contiennent un record complet, encodé selon la génération du segment.
3. **I3 — Données ↛ index** : l'inverse n'est pas exigé. Des octets de segment sans ligne d'index sont des orphelins qu'on ignore.
4. **I4 — Une génération active** : toutes les lignes de `index.cdx` désignent des segments de la génération indiquée par le manifest.
5. **I5 — Dictionnaire présent** : le `dict_id` du manifest existe dans le store. Tant qu'un segment le référence, un dictionnaire ne doit jamais être supprimé.
6. **I6 — Unicité des ids de dictionnaire** : un `dict_id` correspond à un seul fichier dans le store.
7. **I7 — Unicité des slugs** : un slug correspond à une seule collection, et un uuid à un seul répertoire.
8. **I8 — Base dérivée** : l'ensemble des entrées ACTIVE en base d'une collection est égal à l'ensemble des lignes de son index (fichiers `.gz` et courant).
9. **I9 — Lots contigus** : les records d'un même lot sont contigus dans un seul segment.

## 5.2 Concurrence

Modèle : **un seul processus** gère le repository.

[LEGACY] La CLI et le serveur peuvent pourtant agir sur le même repository en même temps. Rien ne l'empêche (« detect when the server runs » est resté en TODO).

Verrous, sur le principe :

| Ressource | Verrou | Raison |
|---|---|---|
| Création de collection | verrou global | Unicité du slug sans bloquer les lectures |
| Segment (fichier) | mutex par fichier | Le `seek` puis la lecture ou l'écriture sur un descripteur partagé doit être exclusif. Un ajout fait « seek fin → position → écriture → flush » sous le verrou. |
| Segment courant | cache protégé | Éviter un `stat` à chaque ajout |
| Taille des segments | cache mis à jour après chaque ajout | Choix du segment sans `stat` |
| Manifest | lecture/écriture | Le rebuild le modifie |
| Dictionnaire en cache | lecture/écriture, chargement à la demande | |
| Génération de dictionnaire | existence du répertoire buffer | Verrou durable, entre processus |

Point d'attention : il n'y a **pas de verrou au niveau de la collection pour les ajouts**. Deux ajouts concurrents peuvent choisir le même segment. Chaque écriture reste atomique grâce au mutex de fichier, mais les lignes d'index de deux lots peuvent s'entrelacer dans `index.cdx`. C'est sans conséquence, puisque chaque ligne est autonome.

[RECO] Utiliser `pread` (lecture positionnelle) pour les lectures : on supprime le mutex de lecture par fichier et on permet les lectures parallèles sur un même segment.

## 5.3 Reprise après crash

### Ce que le design permet

L'ordre « données → index → base » combiné à I3 donne une reprise simple :

| Crash pendant… | État | Reprise |
|---|---|---|
| écriture des données | queue de segment tronquée ou orpheline | rien à faire (I3) |
| écriture de l'index | dernière ligne de `index.cdx` tronquée | ignorer ou supprimer la ligne incomplète (pas de `\n` final) |
| insertion en base | base en retard sur l'index | reconstruire l'index secondaire (4.7), ou rejouer la fin |
| rebuild, avant la bascule | nouvelle génération partielle et inactive | nettoyer la cible et relancer |
| rebuild, pendant la bascule | **incohérent** (voir plus bas) | — |

### Ce qui manque au legacy

- **Aucun `fsync`** : ni sur les segments, ni sur l'index, ni sur les répertoires.
- **Manifest écrit sur place** : un disque plein ou un crash peut laisser un manifest corrompu, et la collection ne se charge plus.
- **Bascule du rebuild non atomique** : base activée → base de l'ancienne génération supprimée → manifest → segments supprimés → index renommé. Un crash entre ces étapes peut laisser, par exemple, un manifest qui pointe sur D2 et un `index.cdx` qui pointe encore sur les segments D1, déjà supprimés.
- **Suppression de collection non atomique** : `rm -rf` puis la base. Un crash au milieu laisse un répertoire à moitié supprimé.
- **Ajout de dictionnaire non atomique** : écriture directe dans le store.
- Une ligne d'index tronquée **arrête** la lecture de l'index au lieu d'être sautée.

### Protocole de bascule recommandé [RECO]

Le manifest doit devenir le **point de commit unique** :

1. Écrire la nouvelle génération (segments et `.index.<gen>.cdx`), puis faire `fsync` sur chaque fichier.
2. Écrire `manifest.json.tmp` avec la nouvelle génération et le nom de son index, puis `fsync`, puis `rename` vers `manifest.json`, puis `fsync` du répertoire. **C'est le commit.**
3. Après le commit (opérations idempotentes, qu'on peut rejouer au démarrage) : mettre à jour la base, puis supprimer les fichiers de l'ancienne génération.

Au démarrage, tout fichier de segment ou d'index qui n'appartient pas à la génération du manifest est un reste à supprimer.

Le même schéma (*tmp + fsync + rename + fsync du répertoire*) s'applique au manifest à la création, aux dictionnaires et aux configurations. Pour la suppression d'une collection : renommer d'abord le répertoire en `repository/.trash-<uuid>`, ce qui est atomique, puis supprimer.
