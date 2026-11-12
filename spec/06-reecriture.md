# 6. Points ouverts et recommandations pour la réécriture

Ce sont des propositions, pas des exigences. Le concept décrit dans les chapitres 1 à 5 reste valable sans elles.

## À garder tel quel

- **WARC comme format de record**, avec des **frames zstd indépendantes par record** et un **dictionnaire par collection**. C'est le cœur de l'idée.
- **Segments en append-only + localisateur `(segment, offset, raw_size)`** : un seul `seek` par lecture.
- **Index par collection comme référence, base comme cache** que l'on peut reconstruire.
- **Générations côte à côte** pour recompresser sans interruption.
- **Délégation** de la compression et de la décompression au client, grâce au `dict_id` contenu dans les frames.
- **`warcinfo` initial** pour que chaque collection soit auto-descriptive.

## À corriger dans le modèle

| Sujet | Problème | Proposition |
|---|---|---|
| Génération implicite | Elle est déduite de noms de fichiers et de colonnes nullables | En faire une entité : `generation_id` (entier croissant) dans le manifest, dans les noms de fichiers (`seg.<gen>.<N>.warc.zst`) et en base. Le dictionnaire devient un attribut de la génération. On règle ainsi « rebuild vers le même dictionnaire » et le `NULL = NULL`. |
| Format CDX maison | Pas d'échappement, pas de dictionnaire ni de digest | Format binaire de taille fixe, ou JSON Lines, avec une version. Ajouter un **digest** (par ex. `WARC-Block-Digest` ou xxh3 de la frame) pour vérifier l'intégrité à la lecture. |
| Ordre des en-têtes | Perdu à la sérialisation | Garder une liste ordonnée. Stocker les octets d'entrée tels quels si possible. |
| Suppression | Pas définie | Index avec tombstones + flag en base, mise à zéro facultative, compaction lors du rebuild. |
| Validation de l'import brut | Aucune | Vérifier le magic zstd, le `dict_id` de la frame, et éventuellement décompresser pour contrôler l'en-tête WARC par rapport à la ligne CDX. |
| Record > `split_threshold` | Boucle infinie | Le placer seul dans un segment neuf. |
| `.zstd` | Extension non standard | `.zst` |
| Rotation de l'index | Prévue, jamais faite | Soit rotation en `.cdx.gz`, soit un index segmenté comme les données (`index.<gen>.<N>`), scellé quand son segment de données l'est. |
| Unicité des ids | Non garantie | Décider : unique par collection et par génération active (recommandé), ou multi-valué. Dans le second cas, définir quel exemplaire est renvoyé. |
| Recherche par motif | Ignore ACTIVE, n'échappe rien | Filtrer sur ACTIVE et échapper les parties fournies. Gérer les IP et l'IDNA. |
| Base obligatoire | Tout passe par PostgreSQL | Envisager un index embarqué (SQLite, LMDB, ou un B-tree maison sur `massaged_url`) pour respecter « standalone ». |

## Durabilité et concurrence

- Appliquer le protocole de commit par manifest (voir [05-invariants](05-invariants.md#protocole-de-bascule-recommandé-reco)).
- Utiliser `fsync` par lot (group commit), pas par record.
- Poser un verrou exclusif sur le repository (`flock` sur `<workdir>/.lock`) pour interdire deux processus à la fois. La CLI passe alors par le processus serveur, ou prend le verrou elle-même.
- Prendre un verrou d'écriture par collection pour les ajouts. Les lectures se font en `pread`, sans verrou.
- Faire une **vérification au démarrage** (`fsck`) : tronquer ou ignorer la dernière ligne d'index si elle est incomplète, nettoyer les générations orphelines, contrôler I2 sur la fin de chaque segment.

## Questions à trancher

1. Une collection peut-elle avoir **plusieurs générations actives** en même temps (par exemple, compresser seulement les nouveaux records avec un nouveau dictionnaire, sans recompresser l'historique) ? Le modèle « frame autonome + `dict_id` dans la frame » le permet. Ce serait plus souple qu'un rebuild complet.
2. Le **buffer « collection en attente de dictionnaire »** : est-ce qu'on le garde ? Si oui, en faire une génération « non compressée » ordinaire suivie d'un rebuild automatique, plutôt qu'une structure à part avec un fichier par record (ce qui va à l'encontre de l'objectif « pas de gaspillage d'inodes »).
3. **Déduplication** : le WARC a les records `revisit`. Faut-il dédupliquer par digest de contenu ?
4. **Compression sans dictionnaire** : le legacy l'interdit. Pour de gros objets (plus de 1 Mo), une frame zstd sans dictionnaire est pourtant pertinente. On peut l'autoriser par record (le `dict_id` de la frame vaut alors 0).
5. **Taille de segment par défaut** : `2³² − 1` permet de tenir les offsets sur 32 bits. Faut-il en faire une contrainte du format (offsets en u32 dans un index binaire) ?
