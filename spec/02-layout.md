# 2. Organisation sur disque

## Arborescence

```
<workdir>/                              (MASSTUFFY_WORKDIR, par défaut ./)
├── config.json                         configuration de l'instance
└── data/
    ├── buffer/                         zones de travail temporaires
    │   └── <nom>/                      ex. gen_<slug>_dict/
    │       └── ...
    ├── dict/
    │   └── <algo>/                     un sous-répertoire par algorithme (zstd/)
    │       └── <label>.<dict_id>.zstdict
    └── repository/
        └── <collection_uuid>/
            ├── manifest.json
            ├── records.<N>[.<dict_id>].warc[.<algo>]
            ├── index.cdx
            ├── index.cdx.gz            (prévu : partie ancienne de l'index, compressée)
            └── .index.cdx              (temporaire : index de la génération en construction)
```

L'initialisation exige un répertoire **vide**. Elle crée `config.json` et l'arborescence `data/{buffer,dict/zstd,repository}`.

Au démarrage :

1. On charge `config.json`.
2. On recense les dictionnaires (`data/dict/zstd/*`).
3. On charge chaque sous-répertoire de `data/repository/` comme une collection. Une collection dont le manifest est invalide, ou dont le dictionnaire est introuvable, est **ignorée** : elle n'est pas chargée, mais le démarrage continue.

## Collection

### Répertoire

Il est nommé d'après l'`uuid` (v4) de la collection et non d'après son slug. On peut ainsi renommer une collection sans toucher au disque, et le slug n'a pas besoin d'être valide comme nom de fichier.

### `manifest.json`

```json
{
  "uuid": "1b4e28ba-2fa1-11d2-883f-0016d3cca427",
  "slug": "my-collection",
  "compression": "zstd",          // null = pas de compression
  "compression_level": 3,         // niveau zstd (valeur par défaut de zstd)
  "dict_id": 1234567890,          // null si pas de compression
  "split_threshold": 4294967295   // taille max d'un segment en octets (2^32 - 1 par défaut)
}
```

Règles de validation :

- `compression` vaut `null` ou `"zstd"`.
- Si `compression` n'est pas nul, `dict_id` doit l'être aussi. **La compression sans dictionnaire n'est pas supportée** : c'est un choix de conception, puisque compresser chaque petit record seul n'apporte presque rien.
- `dict_id` doit désigner un dictionnaire présent dans le store.

Le manifest est la seule donnée **mutable** de la collection. Il change au rebuild (`compression` et `dict_id`).

### Segments

Format du nom :

```
records.<N>.warc                       sans compression
records.<N>.<dict_id>.warc.<algo>      avec compression (algo = "zstd")
```

- `N` commence à **1** et augmente sans trou à l'intérieur d'une génération.
- Le `dict_id` dans le nom sépare les générations : un rebuild vers un nouveau dictionnaire écrit `records.1.<new>.warc.zstd` à côté de `records.1.<old>.warc.zstd`, sans collision.
- [LEGACY] L'extension est `.zstd` et non `.zst`, qui est l'extension standard.

Choix du segment pour un ajout : on part du dernier segment connu, puis on passe au suivant tant que `taille_actuelle + taille_à_écrire >= split_threshold`. Un lot de records (ajout en masse) est écrit **d'un seul bloc** dans un seul segment : un lot n'est jamais coupé entre deux segments.

Un record isolé plus grand que `split_threshold` fait avancer indéfiniment le choix du segment : il n'y a **pas de garde-fou**. [RECO] Accepter de l'écrire seul dans un segment neuf.

### Premier record : `warcinfo`

À la création, le premier record écrit dans la collection est un `warcinfo` dont le corps (`application/warc-fields`) contient :

```
format: WARC File Format 1.1
software: masstuffy/<version> (<homepage>)
masstuffy-collection-manifest: <manifest JSON initial>
```

La collection est ainsi **auto-descriptive** : avec le premier segment seul, on retrouve l'uuid, le slug et la politique de compression de départ. Ce record est indexé comme les autres.

### `index.cdx` et `index.cdx.gz`

L'index courant reçoit les nouvelles lignes en append-only. [LEGACY, non implémenté] Une fois `index.cdx` assez gros, son contenu devait être ajouté à `index.cdx.gz` (sous forme de membre gzip concaténé), puis `index.cdx` remis à zéro. La lecture complète de l'index revient alors à lire `index.cdx.gz` puis `index.cdx`.

`.index.cdx` (fichier caché) contient l'index de la génération en cours de reconstruction. Il est renommé en `index.cdx` au moment de la bascule.

## Dictionary store

```
data/dict/zstd/<label>.<dict_id>.zstdict
```

- Le nom doit comporter **exactement 3 parties** séparées par des `.`. Sinon, le fichier est ignoré avec un avertissement. Le `<label>` ne peut donc pas contenir de `.`.
- `<label>` est libre et sert aux humains. Les dictionnaires générés reçoivent `<slug>_<YYYYmmddHHMMSS>`.
- `<dict_id>` en décimal doit être égal à l'id inscrit dans le dictionnaire (octets 4 à 7, little-endian, voir [03-formats](03-formats.md)).
- Deux fichiers différents avec le même `dict_id` sont une **erreur fatale**.
- Les dictionnaires sont chargés en mémoire à la première utilisation, puis gardés en cache.

## Buffer

`data/buffer/<nom>/` est créé à la demande et supprimé à la fin de l'opération. Son existence vaut verrou (voir *Génération de dictionnaire* dans [04-operations](04-operations.md)).

Structure prévue pour les collections en attente de dictionnaire (non implémentée) :

```
data/buffer/<slug>.<id>/
├── manifest.json
└── records/
    ├── 1
    ├── 2
    └── ...
```
