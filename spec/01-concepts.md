# 1. Concepts et vocabulaire

## Repository

Le répertoire de travail qui contient tout l'état persistant : la configuration, les collections, les dictionnaires et les buffers. Une instance de Masstuffy gère exactement un repository.

## Collection

L'unité d'organisation, de compression et de droits. Une collection est un ensemble de records qui partagent :

- une **identité** : un `uuid` immuable (le nom du répertoire sur disque) et un `slug` lisible et unique (le nom utilisé par les humains et par les permissions) ;
- une **politique de compression** : aucune, ou zstd avec un dictionnaire précis et un niveau de compression ;
- un **seuil de découpage** (`split_threshold`) : la taille maximale d'un segment.

La collection est la bonne granularité pour un dictionnaire : les objets d'une même collection se ressemblent (même site, même type de document), donc un dictionnaire entraîné sur eux compresse bien.

Le `uuid` est la clé technique : il sert dans l'index secondaire et dans les chemins. Le `slug` est un alias. Renommer une collection ne devrait toucher que le manifest.

## Record

L'objet stocké. C'est un **record WARC** : une ligne de version, des en-têtes `Clé: valeur`, un corps opaque.

Les attributs qui comptent pour le système :

| Attribut | Source | Rôle |
|---|---|---|
| `record_id` | `WARC-Record-ID` (sans les `<>`), en général `urn:uuid:…` | Identifiant de l'objet |
| `type` | `WARC-Type` (`response`, `request`, `resource`, `warcinfo`, `metadata`…) | Filtrage. Les `request` sont exclues de la résolution par URL. |
| `date` | `WARC-Date` | Dimension temporelle (plusieurs versions d'une même URL) |
| `url` | `WARC-Target-URI` (optionnel) | Seconde clé d'accès |

Un record est **immuable** une fois écrit. Le modifier revient à écrire un nouveau record.

**Unicité du `record_id`** : elle n'est **pas** garantie globalement. [LEGACY] La contrainte d'unicité a été retirée parce qu'un même record existe temporairement en deux exemplaires pendant une reconstruction (rebuild), et parce que rien n'empêche d'importer deux fois le même fichier WARC. La recherche par id renvoie le premier exemplaire **actif**.

## Segment (fichier de records)

Un fichier de la collection qui contient une suite de records concaténés. Un segment est en **append-only** : on n'écrit qu'à la fin. Quand l'ajout d'un record lui ferait dépasser `split_threshold`, on ouvre le segment suivant (`N+1`).

Un segment appartient à une **génération** de compression : son nom contient l'identifiant du dictionnaire et l'algorithme. Deux générations peuvent donc coexister dans le même répertoire, ce qui permet la reconstruction sans rien détruire (voir *Rebuild*).

## Localisateur (locator)

Le triplet qui permet de lire un record avec un seul `seek` :

```
(segment_filename, offset, raw_size)
```

- `offset` : position du premier octet du record (ou de sa frame zstd) dans le segment.
- `raw_size` : nombre d'octets **tels que stockés**, donc compressés si la collection est compressée.

Le localisateur n'a de sens que dans une collection donnée. Le chemin complet d'un record est donc `(collection_uuid, segment, offset, raw_size)`.

## Index de collection (CDX)

Un fichier texte, une ligne par record, qui associe les attributs d'un record à son localisateur. C'est la **référence** pour savoir quels records existent dans une collection.

On l'utilise pour :

- reconstruire l'index secondaire ;
- parcourir tous les records sans dépendre du contenu des segments (qui peuvent contenir des zones invalidées) ;
- reconstruire la collection (rebuild).

Le format s'inspire du CDX de l'archivage web, mais il est simplifié et propre à Masstuffy (voir [03-formats](03-formats.md)).

## Dictionnaire

Un dictionnaire zstd entraîné sur un échantillon de records. Il est **partagé** au niveau du repository : plusieurs collections peuvent utiliser le même, et il est identifié par un `dict_id` de 32 bits **inscrit dans le dictionnaire lui-même**. Chaque frame zstd compressée avec ce dictionnaire porte aussi cet id dans son en-tête.

Un dictionnaire est **immuable** et ne doit jamais être supprimé tant qu'une génération de segments l'utilise. Sans lui, les données sont illisibles.

**Pourquoi un dictionnaire** : un petit objet seul (quelques Ko) compresse mal, faute d'historique. Le dictionnaire fournit cet historique commun, ce qui permet de garder chaque record dans une frame **indépendante** (donc lisible par accès direct) tout en obtenant un taux proche de celui d'une compression de bout en bout.

## Génération

L'ensemble des segments, des lignes d'index et des entrées d'index secondaire produits avec un même couple `(algorithme, dict_id)`. Une collection a **une génération active**. Pendant un rebuild, elle en a temporairement deux : l'ancienne, encore active, et la nouvelle, en construction et pas encore active.

[LEGACY] La génération n'est pas une entité explicite : elle est déduite de `(collection, dict_type, dict_id)` dans la base et du suffixe des noms de segments.

## Buffer

Une zone de travail temporaire dans le repository, sous forme d'un répertoire nommé. Il a deux usages :

1. **Implémenté** : préparation de l'échantillon pour entraîner un dictionnaire (les records y sont écrits décompressés, un fichier par record). L'existence du répertoire sert de **verrou** : on ne lance pas deux générations de dictionnaire en même temps pour une même collection.
2. **Prévu, non implémenté** : une collection créée « en attente de dictionnaire ». Les records y sont stockés sans compression, chacun dans son fichier. Une fois un seuil atteint, on entraîne un dictionnaire, puis on déverse le buffer dans une collection compressée.

## Index secondaire (base de données)

Une table qui reprend toutes les lignes d'index de toutes les collections, enrichies de :

- `flags` (bit 0 = **ACTIVE**) ;
- `dict_type` et `dict_id` (la génération) ;
- `massaged_url` (une forme normalisée de l'URL, faite pour les recherches par préfixe et par domaine).

Elle permet de résoudre :

- **par id** : `record_id` → localisateur ;
- **par URL et date** : `(url, date)` → le record actif, hors `request`, dont la date est **la plus proche** de la date demandée (sémantique « Wayback ») ;
- **par motif** : domaine (avec ou sans sous-domaines), hôte exact, port, préfixe de chemin ou chemin exact.

Elle se reconstruit entièrement à partir des index CDX et des manifests. Ce n'est **pas** une source de vérité.

## Massaged URL

Une forme canonique d'URL, proche du SURT, conçue pour que « tous les sous-domaines de X » et « tous les chemins sous Y » deviennent des recherches par préfixe ou par expression régulière simple :

```
https://www.Example.com:8443/a/b?z=2&a=1#frag
→ com,example,www:8443)/a/b?a=1&z=2
```

Règles :

1. Hôte : les labels sont inversés et joints par `,` (les `.` en tête ou en fin sont retirés). Une IP est gardée telle quelle.
2. Port : ajouté sous la forme `:port` **seulement** s'il diffère du port par défaut du schéma (80 pour http, 443 pour https).
3. Le séparateur `)` marque la fin de l'autorité.
4. Le chemin est gardé tel quel.
5. Requête : les paires sont triées (par clé, puis par valeur), les valeurs sont encodées en URL, et le tout est préfixé par `?` et joint par `&`.
6. Le schéma et le fragment sont supprimés.

[LEGACY] Pas d'encodage IDNA, pas de passage en minuscules explicite au-delà de ce que fait le parseur d'URL, pas de suppression du `www`. Une URL qui ne se parse pas donne une massaged URL vide.

## Résumé des relations

```
Repository 1 ── * Collection
Repository 1 ── * Dictionary
Collection * ── 0..1 Dictionary   (génération active)
Collection 1 ── * Segment         (groupés par génération)
Collection 1 ── 1 Index CDX       (+ 1 index temporaire pendant un rebuild)
Segment    1 ── * Record
Index CDX  1 ── * Ligne ──► 1 Record (via le localisateur)
```
