# 4. Opérations

Les opérations sont décrites de façon abstraite : peu importe qu'elles soient appelées depuis une CLI, une API ou une bibliothèque.

---

## 4.1 Créer une collection

**Entrée** : `slug`, et en option `(algo, dict_id)`.

1. Refuser si le `slug` existe déjà. Le contrôle et la création sont protégés par un verrou global de création, pour éviter que deux créations simultanées utilisent le même slug.
2. Si un dictionnaire est demandé, vérifier qu'il existe dans le store.
3. Générer un `uuid` v4 et créer `repository/<uuid>/`.
4. Écrire `manifest.json` (avec `compression_level` à la valeur par défaut de zstd et `split_threshold` à `2³² − 1`).
5. Charger la collection.
6. Ajouter le record `warcinfo` initial, qui contient le manifest (voir [02-layout](02-layout.md)).
7. Enregistrer la collection dans les tables `slug → collection` et `uuid → collection`.

---

## 4.2 Ajouter des records

### a) Record décodé (chemin normal)

1. Sérialiser le record (voir 3.1).
2. Si la collection est compressée, compresser le record dans une frame zstd avec le dictionnaire.
3. Construire la ligne d'index (`url`, `type`, `record_id`, `date`) avec `raw_size` = taille de la frame.
4. Passer à 4.2c avec un lot d'un seul record.

### b) Records bruts (compression déléguée au client)

Entrée : un flux de `ligne CDX\n` + `raw_size` octets, répété.

1. Lire les lignes et les données, et les accumuler dans un lot.
2. Envoyer le lot en 4.2c dès que le record suivant ferait dépasser le seuil de lot (50 Mo dans le legacy), puis envoyer le reste à la fin. Un `raw_size` nul ou absent est refusé.

Le serveur ne vérifie **pas** que les octets reçus sont une frame valide, ni qu'ils utilisent le bon dictionnaire, ni que la ligne CDX correspond au contenu. [RECO] Au minimum, vérifier le magic zstd et le `dict_id` dans l'en-tête de frame.

### c) Écriture d'un lot (cœur commun)

1. **Choisir le segment** : partir du segment courant (gardé en cache, en commençant à 1). Tant que `taille(segment) + taille(lot) >= split_threshold`, passer à `N+1`. Mettre à jour le cache.
2. **Écrire les données** : ajouter le lot entier en fin de segment et récupérer l'offset de départ.
3. **Calculer les localisateurs** : pour chaque record du lot, `offset = offset_départ + somme des raw_size précédents`, et `filename = segment`.
4. **Écrire l'index** : ajouter les lignes CDX à `index.cdx`.
5. **Indexer en base** : insérer chaque ligne avec `flags = ACTIVE` et la génération courante.

Ordre important : **données, puis index CDX, puis base**. Un record n'existe vraiment que lorsqu'il est dans l'index CDX. Des octets en fin de segment sans ligne d'index sont des orphelins, sans conséquence.

---

## 4.3 Lire un record

### Par localisateur

1. Ouvrir le segment (les descripteurs sont gardés en cache, un par fichier).
2. Aller à `offset` avec `seek`.
3. Si la collection est compressée, décompresser une frame zstd avec le dictionnaire de la collection, puis lire un record WARC dans le flux décompressé. Sinon, lire directement un record WARC.

### En brut (décompression déléguée au client)

Lire exactement `raw_size` octets à partir de `offset`, sans les décoder. Renvoyer aussi le `dict_id` pour que le client puisse aller chercher le dictionnaire.

### Par `record_id`

Base : premier record **ACTIVE** avec cet `identifier`, puis lecture par localisateur.

### Par URL et date

Base : parmi les records **ACTIVE** dont `uri = url` exactement et `type ≠ request`, prendre celui qui minimise `|date − date_demandée|`. Sans date, on prend la date actuelle, donc le record le plus récent.

### Recherche par motif

On construit une expression régulière sur la massaged URL à partir de :

| Critère | Motif |
|---|---|
| domaine (avec sous-domaines) | `<massaged_domain>(,[a-z0-9]+)*` |
| hôte exact | `<massaged_host>` |
| port | `:<port>`, sinon `(:[0-9]{1,5})?` |
| chemin préfixe | `<path>.*` |
| chemin exact | `<path>` |

Puis on sépare l'autorité du chemin avec `\)`. Le résultat est limité (100 par défaut).

[LEGACY] Cette recherche ne filtre pas sur ACTIVE, n'échappe pas le chemin, ne gère ni les IP ni les paramètres de requête.

### Parcours complet (scan / grep)

Parcourir les lignes de `index.cdx` (et non le segment) et lire chaque record par son localisateur. On saute ainsi les zones invalidées ou orphelines des segments.

---

## 4.4 Générer un dictionnaire

**Entrée** : collection, nombre d'échantillons N (100 000 par défaut), taille maximale (5 Mo par défaut), option *rebuild*.

1. Créer `buffer/gen_<slug>_dict/`. **S'il existe déjà, abandonner** : une génération est déjà en cours, ou une précédente a planté et il faut nettoyer à la main.
2. Choisir jusqu'à N records ACTIVE de la collection dans l'ordre pseudo-aléatoire déterministe.
3. Lire chaque record **décompressé**, le resérialiser et l'écrire comme un fichier dans le buffer.
4. Entraîner un dictionnaire zstd sur ces fichiers.
5. Attribuer un `dict_id` libre (voir 3.3).
6. Supprimer le buffer.
7. Écrire `dict/zstd/<slug>_<date>.<dict_id>.zstdict`, puis recharger le store.
8. Si *rebuild* est demandé, lancer 4.5 avec ce dictionnaire.

---

## 4.5 Reconstruire une collection (rebuild / recompression)

Il s'agit de faire passer la collection d'une génération à une autre (par exemple : sans compression vers zstd+D1, ou zstd+D1 vers zstd+D2) **sans perdre la lecture pendant l'opération**.

**Préparation**

1. Lister les records **à partir de l'index CDX** et non des segments. Le contenu des segments n'est pas fiable : des zones ont pu être mises à zéro pour supprimer des records.
2. Trier les localisateurs par `(segment, offset)`, pour lire de façon séquentielle.
3. Nettoyer une tentative précédente interrompue : supprimer en base les entrées de la génération cible, et supprimer `.index.cdx`.

**Écriture de la nouvelle génération** (l'ancienne reste active et lisible)

4. Pour chaque record :
   1. Lire et décompresser avec l'ancien dictionnaire.
   2. Resérialiser et compresser avec le nouveau.
   3. Ajouter à `records.<k>.<new_dict>.warc.zstd`, en changeant de segment quand `split_threshold` est atteint.
   4. Ajouter la ligne à `.index.cdx`.
   5. Insérer en base avec `flags = 0` (**inactive**) et la génération cible.
   6. Un record illisible est **abandonné** avec un avertissement.

**Bascule (commit)**

5. En base, activer la nouvelle génération (`flags |= ACTIVE`).
6. En base, supprimer les entrées de l'ancienne génération.
7. Mettre à jour le manifest (`compression` et `dict_id`), puis l'écrire.
8. Supprimer les segments de l'ancienne génération et retirer leurs descripteurs du cache.
9. Vider le cache du dictionnaire de la collection.
10. Supprimer `index.cdx.gz`, puis renommer `.index.cdx` en `index.cdx`.

L'idée de conception : **les deux générations coexistent sur le disque, et la bascule se fait par le flag ACTIVE en base, puis par le manifest.** Une lecture faite pendant la reconstruction trouve toujours l'ancienne génération, qui est active et complète.

[LEGACY] Limites connues :

- seule la cible zstd est supportée ;
- un rebuild vers le **même** dictionnaire écrase les noms et casse tout ;
- pas de protection contre deux rebuilds simultanés ;
- les flags autres que ACTIVE sont perdus ;
- le nettoyage en base ne fonctionne pas pour une génération sans compression (`NULL = NULL` est faux en SQL) ;
- les segments partiels d'une tentative interrompue ne sont pas supprimés (l'étape 3 tente `remove_dir` sur un fichier) ;
- les étapes 5 à 10 ne forment pas un tout atomique (voir [05-invariants](05-invariants.md)).

---

## 4.6 Supprimer

### Une collection

1. Supprimer récursivement `repository/<uuid>/`.
2. Supprimer les entrées de la collection en base.
3. La retirer des tables en mémoire.

Les dictionnaires ne sont pas supprimés, puisqu'ils peuvent être partagés.

### Un record (concept prévu, non implémenté)

Les segments sont en append-only, donc on ne peut pas retirer un record physiquement sur place. Le concept qui ressort du code :

- **logiquement** : retirer le flag ACTIVE en base, ou supprimer l'entrée (« mark for deletion ») ;
- **physiquement** (en option) : **mettre à zéro** les octets du record dans le segment, pour effacer réellement les données ;
- **compaction** : le prochain rebuild ne recopie que les records présents dans l'index. [RECO] Le rebuild doit donc lire un index qui tient compte des suppressions : soit retirer la ligne de l'index CDX, soit ajouter des lignes « tombstone ».

---

## 4.7 Reconstruire l'index secondaire

Pour chaque collection (dans son manifest : `uuid`, et la génération courante `compression`/`dict_id`), lire `index.cdx.gz` puis `index.cdx` et insérer chaque ligne avec `flags = ACTIVE`. La massaged URL est recalculée depuis `url`.

Cette opération doit toujours pouvoir donner une base identique à celle d'origine (aux `id` techniques près). C'est ce qui garantit que la base n'est qu'un cache.
